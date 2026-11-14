use std::net::IpAddr;

const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "api_key",
    "token",
    "x-emby-token",
    "authtoken",
    "secret",
    "password",
    "pw",
];

/// Extracts the scheme and authority (`host` or `host:port`) and the remainder from a URL.
fn split_url_parts(url: &str) -> Option<(&str, &str, &str)> {
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];
    let after_scheme = &url[scheme_end + 3..];
    let path_start = after_scheme.find(['/', '?', '#']).unwrap_or(after_scheme.len());
    let authority = &after_scheme[..path_start];
    let rest = &after_scheme[path_start..];
    Some((scheme, authority, rest))
}

/// Extracts the hostname (without port or IPv6 brackets) from a URL or host string.
pub fn extract_host(url_or_host: &str) -> String {
    let trimmed = url_or_host.trim();
    let authority = if let Some((_, auth, _)) = split_url_parts(trimmed) {
        auth
    } else {
        trimmed.split(['/', '?', '#']).next().unwrap_or(trimmed)
    };
    // Strip userinfo if any
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    if host_port.starts_with('[') {
        if let Some(end) = host_port.find(']') {
            return host_port[1..end].to_string();
        }
    }
    // Unbracketed IPv6 has multiple colons; only strip :port when there is a single colon.
    if host_port.matches(':').count() == 1 {
        host_port.split(':').next().unwrap_or(host_port).to_string()
    } else {
        host_port.to_string()
    }
}

/// Returns true if `host` is a loopback address (`localhost`, `127.0.0.1`, `::1`, `127.x.x.x`).
pub fn is_loopback_host(host: &str) -> bool {
    let h = extract_host(host).to_ascii_lowercase();
    if h == "localhost" || h == "::1" || h == "0:0:0:0:0:0:0:1" {
        return true;
    }
    if let Ok(ip) = h.parse::<IpAddr>() {
        return ip.is_loopback();
    }
    false
}

/// Returns true if `host` is loopback, RFC1918 private (`10.x`, `172.16-31.x`, `192.168.x`),
/// or link-local (`169.254.x`).
pub fn is_private_host(host: &str) -> bool {
    if is_loopback_host(host) {
        return true;
    }
    let h = extract_host(host);
    if let Ok(IpAddr::V4(v4)) = h.parse::<IpAddr>() {
        return v4.is_private() || v4.is_link_local() || v4.is_loopback();
    }
    if let Ok(IpAddr::V6(v6)) = h.parse::<IpAddr>() {
        return v6.is_loopback() || ((v6.segments()[0] & 0xfe00) == 0xfc00);
    }
    false
}

fn redact_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for (i, pair) in query.split('&').enumerate() {
        if i > 0 {
            out.push('&');
        }
        if let Some(eq) = pair.find('=') {
            let k = &pair[..eq];
            let k_lower = k.to_ascii_lowercase();
            if SENSITIVE_QUERY_KEYS.iter().any(|sk| *sk == k_lower) {
                out.push_str(k);
                out.push_str("=***");
            } else {
                out.push_str(pair);
            }
        } else {
            out.push_str(pair);
        }
    }
    out
}

/// Redacts a URL for safe logging and UI display:
/// - Keeps `scheme://host[:port]`
/// - Replaces `/jellio/<token>` with `/jellio/***`
/// - Truncates long paths to first 12 chars + `...`
/// - Replaces sensitive query parameter values (`api_key`, `token`, `X-Emby-Token`, `AuthToken`, etc.) with `***`
pub fn redact_url(url: &str) -> String {
    let trimmed = url.trim();
    let Some((scheme, authority, rest)) = split_url_parts(trimmed) else {
        return redact_secrets(trimmed);
    };

    // Strip userinfo from authority if present
    let clean_auth = if let Some(at) = authority.rfind('@') {
        format!("***@{}", &authority[at + 1..])
    } else {
        authority.to_string()
    };

    let (path_part, query_part) = match rest.find('?') {
        Some(q) => (&rest[..q], Some(&rest[q + 1..])),
        None => (rest, None),
    };

    let redacted_path = if let Some(j_idx) = path_part.to_ascii_lowercase().find("/jellio/") {
        let prefix = &path_part[..j_idx + "/jellio/".len()];
        let after = &path_part[j_idx + "/jellio/".len()..];
        if let Some(slash) = after.find('/') {
            format!("{}***{}", prefix, &after[slash..])
        } else {
            format!("{}***", prefix)
        }
    } else if path_part.len() > 12 {
        let mut end = 12;
        while !path_part.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &path_part[..end])
    } else {
        path_part.to_string()
    };

    let mut out = format!("{}://{}{}", scheme, clean_auth, redacted_path);
    if let Some(q) = query_part {
        let q_no_frag = q.split('#').next().unwrap_or(q);
        out.push('?');
        out.push_str(&redact_query(q_no_frag));
    }
    out
}

/// Redacts URLs, tokens, API keys, passwords, and base64 `AuthToken` JSON blobs from free text.
pub fn redact_secrets(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;

    // Pass 1: redact URLs in text
    while let Some(pos) = find_next_url(rest) {
        result.push_str(&rest[..pos]);
        let after = &rest[pos..];
        let url_end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | ','))
            .unwrap_or(after.len());
        let url_slice = &after[..url_end];
        result.push_str(&redact_url(url_slice));
        rest = &after[url_end..];
    }
    result.push_str(rest);

    // Pass 2: redact key=value, Token="...", X-Emby-Token: ..., and base64 eyJ... AuthToken blobs
    let pass2 = redact_kv_and_headers(&result);
    redact_base64_json_tokens(&pass2)
}

fn find_next_url(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    ["https://", "http://", "stremio://"]
        .iter()
        .filter_map(|p| lower.find(p))
        .min()
}

fn redact_kv_and_headers(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let bytes = input.as_bytes();
    let lower = input.to_ascii_lowercase();

    while i < bytes.len() {
        // Check MediaBrowser Token="..." or Token="..."
        if lower[i..].starts_with("token=\"") {
            out.push_str(&input[i..i + 7]);
            out.push_str("***\"");
            i += 7;
            if let Some(end_q) = input[i..].find('"') {
                i += end_q + 1;
            } else {
                i = bytes.len();
            }
            continue;
        }

        // Check X-Emby-Token: <val>
        if lower[i..].starts_with("x-emby-token:") {
            out.push_str(&input[i..i + 13]);
            out.push_str(" ***");
            i += 13;
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b',' && bytes[i] != b'"' {
                i += 1;
            }
            continue;
        }

        // Check sensitive key=value
        let mut matched_kv = false;
        for key in SENSITIVE_QUERY_KEYS {
            let pat_len = key.len() + 1;
            if lower[i..].starts_with(key) && bytes.get(i + key.len()) == Some(&b'=') {
                // Ensure boundary before key
                let boundary_ok = i == 0
                    || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_';
                if boundary_ok {
                    out.push_str(&input[i..i + pat_len]);
                    out.push_str("***");
                    i += pat_len;
                    while i < bytes.len()
                        && !bytes[i].is_ascii_whitespace()
                        && !matches!(bytes[i], b'&' | b'"' | b'\'' | b',' | b')' | b']')
                    {
                        i += 1;
                    }
                    matched_kv = true;
                    break;
                }
            }
        }
        if matched_kv {
            continue;
        }

        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn redact_base64_json_tokens(input: &str) -> String {
    // Redact base64-encoded JSON objects starting with "eyJ" (e.g. {"AuthToken":...})
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find("eyJ") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        let end = after
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '/' && c != '=' && c != '-' && c != '_')
            .unwrap_or(after.len());
        let candidate = &after[..end];
        if candidate.len() >= 16 {
            out.push_str("***");
        } else {
            out.push_str(candidate);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

