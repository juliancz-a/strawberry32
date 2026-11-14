use crate::redact::{extract_host, is_private_host, redact_secrets};
use std::io::{self, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetErrorKind {
    ConnectionRefused,
    Timeout,
    DnsFailure,
    TlsError,
    HttpUnauthorized,
    HttpNotFound,
    HttpServerError,
    InvalidJson,
    InvalidManifest,
    InsecurePublicHttp,
    ResponseTooLarge,
    Other,
}

#[derive(Debug, Clone)]
pub struct ClassifiedError {
    pub kind: NetErrorKind,
    pub user_message: String,
    pub detail: String,
}

/// Tries each resolved address in order with `per_addr_timeout` until one connects.
pub fn connect_addrs(addrs: &[SocketAddr], per_addr_timeout: Duration) -> io::Result<TcpStream> {
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "No addresses resolved for host",
        ));
    }
    let mut last_err: Option<io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(addr, per_addr_timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotConnected, "Failed to connect to any address")
    }))
}

/// Resolves `host:port` using a custom resolver and tries each address in order.
pub fn connect_with_resolver<F>(
    host: &str,
    port: u16,
    per_addr_timeout: Duration,
    resolver: F,
) -> io::Result<TcpStream>
where
    F: FnOnce(&str, u16) -> io::Result<Vec<SocketAddr>>,
{
    let addrs = resolver(host, port)?;
    connect_addrs(&addrs, per_addr_timeout)
}

/// Resolves `host:port` via `ToSocketAddrs` and connects dual-stack (trying every address).
pub fn connect_dual_stack(
    host: &str,
    port: u16,
    per_addr_timeout: Duration,
) -> io::Result<TcpStream> {
    connect_with_resolver(host, port, per_addr_timeout, |h, p| {
        let iter = (h, p).to_socket_addrs()?;
        Ok(iter.collect())
    })
}

/// Custom `ureq::Resolver` that resolves via `ToSocketAddrs` (supporting IPv6 + IPv4 dual-stack).
/// `ureq` iterates through all returned `SocketAddr`s with `timeout_connect`.
#[derive(Debug, Default)]
pub struct DualStackResolver;

impl ureq::Resolver for DualStackResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        let addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        Ok(addrs)
    }
}

/// Reads an HTTP body up to `max_bytes`, failing if the response exceeds the limit.
pub fn read_body_capped(mut reader: impl Read, max_bytes: usize) -> Result<Vec<u8>, ClassifiedError> {
    let mut buf = Vec::new();
    let mut limited = reader.by_ref().take((max_bytes + 1) as u64);
    limited.read_to_end(&mut buf).map_err(|e| ClassifiedError {
        kind: NetErrorKind::Other,
        user_message: "Failed reading server response.".to_string(),
        detail: redact_secrets(&e.to_string()),
    })?;
    if buf.len() > max_bytes {
        return Err(ClassifiedError {
            kind: NetErrorKind::ResponseTooLarge,
            user_message: format!("Response exceeded maximum allowed size ({} KB).", max_bytes / 1024),
            detail: format!("Body exceeded {} bytes cap", max_bytes),
        });
    }
    Ok(buf)
}

/// Validates HTTP vs HTTPS policy:
/// - Plain HTTP is allowed for loopback (`localhost`, `127.0.0.1`, `::1`) and private ranges (`10.x`, `172.16-31.x`, `192.168.x`, `169.254.x`).
/// - Public hosts must use HTTPS.
pub fn validate_transport_policy(url: &str) -> Result<(), ClassifiedError> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    if lower.starts_with("http://") {
        let host = extract_host(url);
        if is_private_host(&host) {
            return Ok(());
        }
        return Err(ClassifiedError {
            kind: NetErrorKind::InsecurePublicHttp,
            user_message: "Plain HTTP is only allowed for local/private network servers; use HTTPS for public hosts.".to_string(),
            detail: format!("Rejected plain HTTP for public host '{}'", host),
        });
    }
    Err(ClassifiedError {
        kind: NetErrorKind::Other,
        user_message: "Unsupported URL scheme (use http://, https://, or stremio://).".to_string(),
        detail: "Unsupported scheme".to_string(),
    })
}

/// Classifies a `std::io::Error` into a user-readable `ClassifiedError`.
pub fn classify_io_error(err: &io::Error) -> ClassifiedError {
    let detail = redact_secrets(&err.to_string());
    let lower = detail.to_ascii_lowercase();

    match err.kind() {
        io::ErrorKind::ConnectionRefused => ClassifiedError {
            kind: NetErrorKind::ConnectionRefused,
            user_message: "Server not reachable (connection refused). Is it running on the right port?".to_string(),
            detail,
        },
        io::ErrorKind::TimedOut => ClassifiedError {
            kind: NetErrorKind::Timeout,
            user_message: "Connection timed out. Check the server address and firewall.".to_string(),
            detail,
        },
        _ => {
            if lower.contains("10061") || lower.contains("refused") {
                ClassifiedError {
                    kind: NetErrorKind::ConnectionRefused,
                    user_message: "Server not reachable (connection refused). Is it running on the right port?".to_string(),
                    detail,
                }
            } else if lower.contains("timed out") || lower.contains("timeout") || lower.contains("10060") {
                ClassifiedError {
                    kind: NetErrorKind::Timeout,
                    user_message: "Connection timed out. Check the server address and firewall.".to_string(),
                    detail,
                }
            } else if lower.contains("dns")
                || lower.contains("failed to lookup")
                || lower.contains("no such host")
                || lower.contains("11001")
            {
                ClassifiedError {
                    kind: NetErrorKind::DnsFailure,
                    user_message: "Could not resolve server hostname (DNS failure).".to_string(),
                    detail,
                }
            } else {
                ClassifiedError {
                    kind: NetErrorKind::Other,
                    user_message: "Network connection error.".to_string(),
                    detail,
                }
            }
        }
    }
}

/// Classifies a `ureq::Error` into a user-readable `ClassifiedError`, with all secrets redacted.
pub fn classify_ureq_error(err: &ureq::Error) -> ClassifiedError {
    match err {
        ureq::Error::Status(code, resp) => {
            let status_text = resp.status_text().to_string();
            let detail = redact_secrets(&format!("HTTP {} {}", code, status_text));
            match *code {
                401 | 403 => ClassifiedError {
                    kind: NetErrorKind::HttpUnauthorized,
                    user_message: "Token expired or revoked (HTTP 401/403): regenerate the link or sign in again.".to_string(),
                    detail,
                },
                404 => ClassifiedError {
                    kind: NetErrorKind::HttpNotFound,
                    user_message: "Not found (HTTP 404): wrong path or plugin not active.".to_string(),
                    detail,
                },
                500..=599 => ClassifiedError {
                    kind: NetErrorKind::HttpServerError,
                    user_message: format!("Server error (HTTP {}): the server encountered an internal error.", code),
                    detail,
                },
                _ => ClassifiedError {
                    kind: NetErrorKind::Other,
                    user_message: format!("Server returned HTTP {}.", code),
                    detail,
                },
            }
        }
        ureq::Error::Transport(t) => {
            let mut full = t.to_string();
            let mut src = std::error::Error::source(t);
            while let Some(s) = src {
                full.push_str(" | ");
                full.push_str(&s.to_string());
                src = std::error::Error::source(s);
            }
            let detail = redact_secrets(&full);
            let lower = detail.to_ascii_lowercase();

            if lower.contains("refused") || lower.contains("10061") {
                ClassifiedError {
                    kind: NetErrorKind::ConnectionRefused,
                    user_message: "Server not reachable (connection refused). Is it running on the right port?".to_string(),
                    detail,
                }
            } else if lower.contains("timed out") || lower.contains("timeout") || lower.contains("10060") {
                ClassifiedError {
                    kind: NetErrorKind::Timeout,
                    user_message: "Connection timed out. Check the server address and firewall.".to_string(),
                    detail,
                }
            } else if lower.contains("dns")
                || lower.contains("failed to lookup")
                || lower.contains("no such host")
                || lower.contains("could not resolve")
                || lower.contains("11001")
            {
                ClassifiedError {
                    kind: NetErrorKind::DnsFailure,
                    user_message: "Could not resolve server hostname (DNS failure).".to_string(),
                    detail,
                }
            } else if lower.contains("tls")
                || lower.contains("ssl")
                || lower.contains("schannel")
                || lower.contains("certificate")
                || lower.contains("handshake")
            {
                ClassifiedError {
                    kind: NetErrorKind::TlsError,
                    user_message: "TLS/certificate error connecting to server.".to_string(),
                    detail,
                }
            } else {
                ClassifiedError {
                    kind: NetErrorKind::Other,
                    user_message: "Network transport error connecting to server.".to_string(),
                    detail,
                }
            }
        }
    }
}

