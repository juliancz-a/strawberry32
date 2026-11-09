fn main() {
    slint_build::compile("ui/app.slint").expect("Slint compilation failed");

    // Embed native Windows PE application icon (.ico) into executable
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let ico_path = std::path::Path::new(&manifest_dir)
            .join("ui")
            .join("icons")
            .join("brand")
            .join("strawberry32.ico");

        if ico_path.exists() {
            println!("cargo:rerun-if-changed={}", ico_path.display());
            let rc_path = std::path::Path::new(&out_dir).join("strawberry32.rc");
            let res_path = std::path::Path::new(&out_dir).join("strawberry32.res");

            // Format path with forward slashes for rc compiler
            let ico_str = ico_path.to_string_lossy().replace('\\', "/");
            let rc_content = format!("1 ICON \"{}\"\n", ico_str);
            if std::fs::write(&rc_path, rc_content).is_ok() {
                // Compile .rc -> .res using llvm-rc
                let mut cmd = std::process::Command::new("llvm-rc");
                cmd.arg(&rc_path).arg(format!("/fo{}", res_path.display()));

                let status = cmd.status().or_else(|_| {
                    std::process::Command::new("D:\\llvm\\bin\\llvm-rc.exe")
                        .arg(&rc_path)
                        .arg(format!("/fo{}", res_path.display()))
                        .status()
                });

                if let Ok(st) = status {
                    if st.success() {
                        println!("cargo:rustc-link-arg={}", res_path.display());
                    }
                }
            }
        }
    }
}
