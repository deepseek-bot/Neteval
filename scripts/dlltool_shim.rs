use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

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
