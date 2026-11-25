fn main() {
    if std::env::var_os("PROFILE").as_deref() == Some(std::ffi::OsStr::new("release"))
        && std::env::var_os("DEP_TAURI_DEV").as_deref() == Some(std::ffi::OsStr::new("true"))
    {
        panic!("Release builds of neteval-gui require `--features custom-protocol` (or `tauri build`) so frontend assets are embedded instead of connecting to devUrl (localhost:1420).");
    }

    let target_os = std::env::var_os("CARGO_CFG_TARGET_OS");
    let target_env = std::env::var_os("CARGO_CFG_TARGET_ENV");
    let is_windows = target_os.as_deref() == Some(std::ffi::OsStr::new("windows"));
    let is_gnu = target_env.as_deref() == Some(std::ffi::OsStr::new("gnu"));

    if is_windows && is_gnu {
        println!("cargo:rustc-cdylib-link-arg=-Wl,--exclude-all-symbols");
    }

    tauri_build::build();

    if is_windows {
        if let Some(out_dir) = std::env::var_os("OUT_DIR") {
            let res_name = if is_gnu {
                "libresource.a"
            } else {
                "resource.lib"
            };
            let res_path = std::path::PathBuf::from(out_dir).join(res_name);
            if res_path.exists() {
                println!("cargo:rustc-link-arg-tests={}", res_path.display());
            }
        }
    }
}
