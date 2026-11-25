use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn find_real_windres() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("WINLIBS_BIN") {
        let candidate = PathBuf::from(dir).join("windres.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let self_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            if self_dir.as_ref().is_some_and(|sd| sd == &dir) {
                continue;
            }
            let candidate = dir.join("windres.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let pkgs_dir = PathBuf::from(local_app_data).join("Microsoft\\WinGet\\Packages");
        if let Ok(entries) = std::fs::read_dir(pkgs_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("BrechtSanders.WinLibs") {
                    let candidate = entry.path().join("mingw64\\bin\\windres.exe");
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

fn norm(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_ascii_lowercase()
}

fn main() -> ExitCode {
    let Some(real_windres) = find_real_windres() else {
        eprintln!("windres_shim: could not locate real windres.exe in PATH");
        return ExitCode::FAILURE;
    };

    let args: Vec<OsString> = env::args_os().skip(1).collect();

    let mut work_dir: Option<PathBuf> = None;
    for i in 0..args.len() {
        let s = args[i].to_string_lossy();
        if (s == "--input" || s == "-i") && i + 1 < args.len() {
            let p = PathBuf::from(&args[i + 1]);
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() {
                    work_dir = Some(parent.to_path_buf());
                }
            }
            break;
        }
    }

    let mut cmd = Command::new(&real_windres);
    if let Some(winlibs_dir) = real_windres.parent() {
        let mut paths = vec![winlibs_dir.to_path_buf()];
        if let Some(existing) = env::var_os("PATH") {
            paths.extend(env::split_paths(&existing));
        }
        if let Ok(joined) = env::join_paths(paths) {
            cmd.env("PATH", joined);
        }
    }
    if let Some(ref dir) = work_dir {
        cmd.current_dir(dir);
        let dir_norm = norm(dir);
        let mut i = 0;
        while i < args.len() {
            let cur = args[i].to_string_lossy();
            if (cur == "--input" || cur == "-i" || cur == "--output" || cur == "-o")
                && i + 1 < args.len()
            {
                cmd.arg(&args[i]);
                let p = Path::new(&args[i + 1]);
                let same_dir = p.parent().is_some_and(|parent| norm(parent) == dir_norm);
                if same_dir {
                    if let Some(name) = p.file_name() {
                        cmd.arg(name);
                    } else {
                        cmd.arg(&args[i + 1]);
                    }
                } else {
                    cmd.arg(&args[i + 1]);
                }
                i += 2;
                continue;
            }
            if (cur == "--include-dir" || cur == "-I") && i + 1 < args.len() {
                cmd.arg(&args[i]);
                let p = Path::new(&args[i + 1]);
                if norm(p) == dir_norm {
                    cmd.arg(".");
                } else {
                    cmd.arg(&args[i + 1]);
                }
                i += 2;
                continue;
            }
            cmd.arg(&args[i]);
            i += 1;
        }
    } else {
        cmd.args(&args);
    }

    match cmd.status() {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
        Err(err) => {
            eprintln!("windres_shim: failed to run {}: {err}", real_windres.display());
            ExitCode::FAILURE
        }
    }
}
