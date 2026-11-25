use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn find_real_dlltool() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("WINLIBS_BIN") {
        let candidate = PathBuf::from(dir).join("dlltool.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let pkgs_dir = PathBuf::from(local_app_data).join("Microsoft\\WinGet\\Packages");
        if let Ok(entries) = fs::read_dir(pkgs_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("BrechtSanders.WinLibs") {
                    let candidate = entry.path().join("mingw64\\bin\\dlltool.exe");
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
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
            let candidate = dir.join("dlltool.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

fn find_lld_link() -> PathBuf {
    if let Ok(output) = Command::new("rustc").args(["--print", "sysroot"]).output() {
        if output.status.success() {
            let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let candidate = PathBuf::from(&sysroot)
                .join("lib")
                .join("rustlib")
                .join("x86_64-pc-windows-gnu")
                .join("bin")
                .join("gcc-ld")
                .join("lld-link.exe");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("lld-link.exe")
}

fn main() -> ExitCode {
    if let Some(real_dlltool) = find_real_dlltool() {
        let mut cmd = Command::new(&real_dlltool);
        cmd.args(env::args_os().skip(1));
        if let Some(winlibs_dir) = real_dlltool.parent() {
            let mut paths = vec![winlibs_dir.to_path_buf()];
            if let Some(existing) = env::var_os("PATH") {
                paths.extend(env::split_paths(&existing));
            }
            if let Ok(joined) = env::join_paths(paths) {
                cmd.env("PATH", joined);
            }
        }
        return match cmd.status() {
            Ok(s) if s.success() => ExitCode::SUCCESS,
            Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
            Err(err) => {
                eprintln!(
                    "dlltool_shim: failed to run {}: {}",
                    real_dlltool.display(),
                    err
                );
                ExitCode::FAILURE
            }
        };
    }

    let args: Vec<String> = env::args().skip(1).collect();
    let mut def_file = None;
    let mut dll_name = None;
    let mut lib_file = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--input-def" => {
                i += 1;
                def_file = args.get(i).cloned();
            }
            "-D" | "--dllname" => {
                i += 1;
                dll_name = args.get(i).cloned();
            }
            "-l" | "--output-lib" => {
                i += 1;
                lib_file = args.get(i).cloned();
            }
            _ => {}
        }
        i += 1;
    }

    let (Some(def_path), Some(lib_path)) = (def_file, lib_file) else {
        eprintln!("dlltool_shim: missing -d or -l in args: {:?}", args);
        return ExitCode::FAILURE;
    };

    if let Some(dll) = dll_name {
        if let Ok(content) = fs::read_to_string(&def_path) {
            if !content
                .lines()
                .any(|l| l.trim_start().to_ascii_uppercase().starts_with("LIBRARY "))
            {
                let patched = format!("LIBRARY \"{}\"\n{}", dll, content);
                if let Err(err) = fs::write(&def_path, patched) {
                    eprintln!("dlltool_shim: failed to patch def {}: {}", def_path, err);
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    let lld = find_lld_link();
    let status = match Command::new(&lld)
        .arg(format!("/def:{}", def_path))
        .arg(format!("/out:{}", lib_path))
        .arg("/machine:x64")
        .status()
    {
        Ok(s) => s,
        Err(err) => {
            eprintln!("dlltool_shim: failed to run {}: {}", lld.display(), err);
            return ExitCode::FAILURE;
        }
    };

    if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
