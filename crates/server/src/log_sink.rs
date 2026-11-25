use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_STATE: OnceLock<Mutex<LogSink>> = OnceLock::new();

struct LogSink {
    path: Option<PathBuf>,
    also_stderr: bool,
}

impl LogSink {
    fn write_entry(&self, level: &str, module: &str, msg: &str) {
        let line = format_log_line(level, module, msg);
        if let Some(ref path) = self.path {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = fs::create_dir_all(parent);
                }
            }
            match OpenOptions::new().create(true).append(true).open(path) {
                Ok(mut file) => {
                    if let Err(err) = file.write_all(line.as_bytes()) {
                        if self.also_stderr {
                            eprintln!(
                                "ERROR [{}] [log_sink] failed to write log file {}: {err}",
                                now_unix_ms(),
                                path.display()
                            );
                        }
                    }
                }
                Err(err) => {
                    if self.also_stderr {
                        eprintln!(
                            "ERROR [{}] [log_sink] failed to open log file {}: {err}",
                            now_unix_ms(),
                            path.display()
                        );
                    }
                }
            }
        }
        if self.also_stderr {
            eprint!("{line}");
        }
    }
}

fn state() -> &'static Mutex<LogSink> {
    LOG_STATE.get_or_init(|| {
        Mutex::new(LogSink {
            path: Some(PathBuf::from("logs").join("latest.log")),
            also_stderr: false,
        })
    })
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn format_log_line(level: &str, module: &str, msg: &str) -> String {
    let clean_msg = msg.replace(['\r', '\n'], " ");
    format!("{level} [{}] [{module}] {clean_msg}\n", now_unix_ms())
}

fn prepare_log_path(path: Option<&Path>, truncate: bool) -> std::io::Result<Option<PathBuf>> {
    match path {
        Some(p) => {
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            if truncate {
                OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(p)?;
            } else {
                OpenOptions::new().create(true).append(true).open(p)?;
            }
            Ok(Some(p.to_path_buf()))
        }
        None => Ok(None),
    }
}

pub fn init_log(path: Option<&Path>, truncate: bool, stderr: bool) -> std::io::Result<()> {
    let owned_path = prepare_log_path(path, truncate)?;
    let mut guard = match state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.path = owned_path;
    guard.also_stderr = stderr;
    Ok(())
}

fn write_log(level: &str, module: &str, msg: &str) {
    let guard = match state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.write_entry(level, module, msg);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_line_starts_with_level_and_sanitizes_newlines() {
        let line = format_log_line("WARN", "s2", "bad\nline\rhere");
        assert!(line.starts_with("WARN ["));
        assert!(line.contains("] [s2] bad line here\n"));
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn write_entry_appends_and_truncates_file() {
        let path = std::env::temp_dir().join(format!(
            "neteval_server_log_test_{}_{}.log",
            std::process::id(),
            now_unix_ms()
        ));
        let prepared = prepare_log_path(Some(&path), true).unwrap();
        let sink = LogSink {
            path: prepared,
            also_stderr: false,
        };
        sink.write_entry("INFO", "s2", "first");
        sink.write_entry("ERROR", "s2", "second");
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("INFO [") && lines[0].ends_with("[s2] first"));
        assert!(lines[1].starts_with("ERROR [") && lines[1].ends_with("[s2] second"));

        let _ = prepare_log_path(Some(&path), true).unwrap();
        let truncated = fs::read_to_string(&path).unwrap();
        assert!(truncated.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn prepare_none_returns_none_for_stderr_only_mode() {
        let prepared = prepare_log_path(None, true).unwrap();
        assert!(prepared.is_none());
        let sink = LogSink {
            path: prepared,
            also_stderr: false,
        };
        sink.write_entry("INFO", "s2", "stderr-only-path-none");
    }
}
