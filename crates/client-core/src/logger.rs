use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_STATE: OnceLock<Mutex<LogSink>> = OnceLock::new();

struct LogSink {
    path: PathBuf,
    also_stderr: bool,
}

fn state() -> &'static Mutex<LogSink> {
    LOG_STATE.get_or_init(|| {
        Mutex::new(LogSink {
            path: PathBuf::from("logs").join("latest.log"),
            also_stderr: false,
        })
    })
}

pub fn init_logger(path: impl AsRef<Path>, truncate: bool, also_stderr: bool) -> std::io::Result<()> {
    let path = path.as_ref().to_path_buf();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    if truncate {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
    }
    if let Ok(mut guard) = state().lock() {
        guard.path = path;
        guard.also_stderr = also_stderr;
    }
    Ok(())
}

pub fn set_stderr(enabled: bool) {
    if let Ok(mut guard) = state().lock() {
        guard.also_stderr = enabled;
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn write_log(level: &str, module: &str, msg: &str) {
    let line = format!("{level} [{}] [{module}] {msg}\n", now_unix_ms());
    if let Ok(guard) = state().lock() {
        if let Some(parent) = guard.path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&guard.path)
        {
            let _ = file.write_all(line.as_bytes());
        }
        if guard.also_stderr {
            eprint!("{line}");
        }
    }
}

pub fn info(module: &str, msg: impl AsRef<str>) {
    write_log("INFO", module, msg.as_ref());
}

pub fn warn(module: &str, msg: impl AsRef<str>) {
    write_log("WARN", module, msg.as_ref());
}

pub fn error(module: &str, msg: impl AsRef<str>) {
    write_log("ERROR", module, msg.as_ref());
}
