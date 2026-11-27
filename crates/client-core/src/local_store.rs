use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use protocol::{AssessmentReport, ProbeError, ReportSummary};

use crate::logger::{error, info, warn};

const LOG_MODULE: &str = "c6";
const MAX_REPORT_ID_LEN: usize = 128;

#[derive(Debug, Clone)]
pub struct LocalStore {
    dir: PathBuf,
}

enum ReadReportError {
    Io(io::Error),
    Parse(serde_json::Error),
}

impl fmt::Display for ReadReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => err.fmt(f),
            Self::Parse(err) => err.fmt(f),
        }
    }
}

impl LocalStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn save_report(&self, report: &AssessmentReport) -> Result<(), ProbeError> {
        validate_report_id(&report.report_id)?;

        if let Err(err) = fs::create_dir_all(&self.dir) {
            error(
                LOG_MODULE,
                format!(
                    "save_report create_dir_all failed report_id={} dir={}: {err}",
                    report.report_id,
                    self.dir.display()
                ),
            );
            return Err(ProbeError::Internal(err.to_string()));
        }

        let payload = match serde_json::to_string_pretty(report) {
            Ok(json) => json,
            Err(err) => {
                error(
                    LOG_MODULE,
                    format!(
                        "save_report serialize failed report_id={}: {err}",
                        report.report_id
                    ),
                );
                return Err(ProbeError::Internal(err.to_string()));
            }
        };

        let final_path = self.report_path(&report.report_id);
        let tmp_path = self.tmp_report_path(&report.report_id);

        if let Err(err) = fs::write(&tmp_path, payload.as_bytes()) {
            error(
                LOG_MODULE,
                format!(
                    "save_report write tmp failed report_id={} path={}: {err}",
                    report.report_id,
                    tmp_path.display()
                ),
            );
            cleanup_tmp(&tmp_path);
            return Err(ProbeError::Internal(err.to_string()));
        }

        if let Err(err) = rename_replacing(&tmp_path, &final_path) {
            error(
                LOG_MODULE,
                format!(
                    "save_report rename failed report_id={} tmp={} final={}: {err}",
                    report.report_id,
                    tmp_path.display(),
                    final_path.display()
                ),
            );
            cleanup_tmp(&tmp_path);
            return Err(ProbeError::Internal(err.to_string()));
        }

        info(
            LOG_MODULE,
            format!(
                "save_report saved report_id={} path={}",
                report.report_id,
                final_path.display()
            ),
        );
        Ok(())
    }

    pub fn list_reports(&self) -> Result<Vec<ReportSummary>, ProbeError> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }

        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                error(
                    LOG_MODULE,
                    format!(
                        "list_reports read_dir failed dir={}: {err}",
                        self.dir.display()
                    ),
                );
                return Err(ProbeError::Internal(err.to_string()));
            }
        };

        let mut summaries = Vec::new();
        for entry_res in entries {
            let entry = match entry_res {
                Ok(entry) => entry,
                Err(err) => {
                    warn(
                        LOG_MODULE,
                        format!(
                            "list_reports skipping unreadable directory entry in {}: {err}",
                            self.dir.display()
                        ),
                    );
                    continue;
                }
            };

            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") || !path.is_file() {
                continue;
            }

            match read_report_from_path(&path) {
                Ok(report) => summaries.push(ReportSummary {
                    report_id: report.report_id,
                    created_unix_ms: report.created_unix_ms,
                    has_nat: report.nat_report.is_some(),
                    nat_status: report.nat_report.as_ref().map(|n| n.status),
                    has_p2p: report.p2p_report.is_some(),
                    p2p_verdict: report.p2p_report.as_ref().map(|p| p.verdict),
                }),
                Err(err) => {
                    warn(
                        LOG_MODULE,
                        format!(
                            "list_reports skipping corrupt report {}: {err}",
                            path.display()
                        ),
                    );
                }
            }
        }

        summaries.sort_by(|a, b| {
            b.created_unix_ms
                .cmp(&a.created_unix_ms)
                .then_with(|| a.report_id.cmp(&b.report_id))
        });

        Ok(summaries)
    }

    pub fn load_report(&self, id: &str) -> Result<AssessmentReport, ProbeError> {
        validate_report_id(id)?;
        let path = self.report_path(id);

        match read_report_from_path(&path) {
            Ok(report) => {
                info(
                    LOG_MODULE,
                    format!("load_report loaded report_id={id} path={}", path.display()),
                );
                Ok(report)
            }
            Err(ReadReportError::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
                warn(
                    LOG_MODULE,
                    format!(
                        "load_report not found report_id={id} path={}",
                        path.display()
                    ),
                );
                Err(ProbeError::InvalidInput(format!("report_not_found: {id}")))
            }
            Err(err) => {
                error(
                    LOG_MODULE,
                    format!(
                        "load_report failed report_id={id} path={}: {err}",
                        path.display()
                    ),
                );
                Err(ProbeError::Internal(err.to_string()))
            }
        }
    }

    pub fn delete_report(&self, id: &str) -> Result<(), ProbeError> {
        validate_report_id(id)?;
        let path = self.report_path(id);

        match fs::remove_file(&path) {
            Ok(()) => {
                info(
                    LOG_MODULE,
                    format!("delete_report deleted report_id={id} path={}", path.display()),
                );
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                warn(
                    LOG_MODULE,
                    format!(
                        "delete_report not found report_id={id} path={}",
                        path.display()
                    ),
                );
                Err(ProbeError::InvalidInput(format!("report_not_found: {id}")))
            }
            Err(err) => {
                error(
                    LOG_MODULE,
                    format!(
                        "delete_report failed report_id={id} path={}: {err}",
                        path.display()
                    ),
                );
                Err(ProbeError::Internal(err.to_string()))
            }
        }
    }

    fn report_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn tmp_report_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json.tmp"))
    }
}

fn validate_report_id(id: &str) -> Result<(), ProbeError> {
    if id.is_empty() {
        warn(LOG_MODULE, "invalid report_id: empty string");
        return Err(ProbeError::InvalidInput(
            "invalid report_id: must not be empty".to_string(),
        ));
    }
    if id.len() > MAX_REPORT_ID_LEN {
        warn(
            LOG_MODULE,
            format!(
                "invalid report_id: length {} exceeds {}",
                id.len(),
                MAX_REPORT_ID_LEN
            ),
        );
        return Err(ProbeError::InvalidInput(format!(
            "invalid report_id: length exceeds {MAX_REPORT_ID_LEN}"
        )));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        warn(
            LOG_MODULE,
            format!("invalid report_id: disallowed characters in {id:?}"),
        );
        return Err(ProbeError::InvalidInput(
            "invalid report_id: only [A-Za-z0-9_-] allowed".to_string(),
        ));
    }
    Ok(())
}

fn read_report_from_path(path: &Path) -> Result<AssessmentReport, ReadReportError> {
    let bytes = fs::read(path).map_err(ReadReportError::Io)?;
    serde_json::from_slice(&bytes).map_err(ReadReportError::Parse)
}

fn rename_replacing(tmp_path: &Path, final_path: &Path) -> io::Result<()> {
    match fs::rename(tmp_path, final_path) {
        Ok(()) => Ok(()),
        // Fallback when target replacement fails on filesystems that disallow overwrite-on-rename.
        Err(first_err) if final_path.exists() => {
            warn(
                LOG_MODULE,
                format!(
                    "save_report rename fallback removing existing target {} (reason: {first_err})",
                    final_path.display()
                ),
            );
            fs::remove_file(final_path)?;
            fs::rename(tmp_path, final_path)
        }
        Err(err) => Err(err),
    }
}

fn cleanup_tmp(tmp_path: &Path) {
    if let Err(err) = fs::remove_file(tmp_path) {
        if err.kind() != io::ErrorKind::NotFound {
            warn(
                LOG_MODULE,
                format!(
                    "save_report failed to clean up temp file {}: {err}",
                    tmp_path.display()
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{
        IpFamily, LatencyPoint, NatFiltering, NatMapping, NatReport, NatStatus, P2pLatencyTier,
        P2pReport, P2pStabilityGrade, P2pVerdict,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let pid = std::process::id();
            let seq = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!("neteval_c6_{label}_{pid}_{ts}_{seq}"));
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn sample_report(id: &str, created_unix_ms: u64, status: Option<NatStatus>) -> AssessmentReport {
        AssessmentReport {
            report_id: id.to_string(),
            created_unix_ms,
            nat_report: status.map(|st| NatReport {
                status: st,
                public_ipv4: Some("203.0.113.10".to_string()),
                public_ipv6: None,
                ipv6_reachable: false,
                mapping: NatMapping::EndpointIndependent,
                filtering: NatFiltering::PortRestrictedNat3,
                estimated_difficulty: "medium".to_string(),
                notes: vec!["ok".to_string()],
            }),
            p2p_report: None,
            storage_warning: None,
        }
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = TestDir::new("round_trip");
        let store = LocalStore::new(dir.path());
        assert_eq!(store.dir(), dir.path());

        let report = sample_report("rep_01-ABC", 1_700_000_000_000, Some(NatStatus::CompleteFull));
        store.save_report(&report).expect("save_report");

        assert!(dir.path().join("rep_01-ABC.json").is_file());
        assert!(!dir.path().join("rep_01-ABC.json.tmp").exists());

        let loaded = store.load_report("rep_01-ABC").expect("load_report");
        assert_eq!(loaded, report);
    }

    #[test]
    fn save_overwrites_existing_report() {
        let dir = TestDir::new("overwrite");
        let store = LocalStore::new(dir.path());

        let first = sample_report("rep-dup", 100, Some(NatStatus::PublicStunOnly));
        store.save_report(&first).expect("save first");

        let second = sample_report("rep-dup", 200, Some(NatStatus::CompleteSingleVps));
        store.save_report(&second).expect("save second");

        let loaded = store.load_report("rep-dup").expect("load updated");
        assert_eq!(loaded, second);
    }

    #[test]
    fn list_reports_missing_dir_returns_empty() {
        let dir = TestDir::new("missing_dir");
        let store = LocalStore::new(dir.path());

        let list = store.list_reports().expect("list_reports");
        assert!(list.is_empty());
        assert!(!dir.path().exists());
    }

    #[test]
    fn list_reports_sorts_and_skips_corrupt_and_non_json() {
        let dir = TestDir::new("list_sort");
        let store = LocalStore::new(dir.path());

        let r_b = sample_report("r-b", 1000, None);
        let r_a = sample_report("r-a", 1000, Some(NatStatus::CompleteFull));
        let r_new = sample_report("r-new", 2000, Some(NatStatus::PublicStunOnly));
        store.save_report(&r_b).expect("save r-b");
        store.save_report(&r_a).expect("save r-a");
        store.save_report(&r_new).expect("save r-new");

        fs::write(dir.path().join("corrupt.json"), b"{not valid json").expect("write corrupt");
        fs::write(
            dir.path().join("pending.json.tmp"),
            serde_json::to_vec(&sample_report("pending", 9999, None)).unwrap(),
        )
        .expect("write tmp");
        fs::write(
            dir.path().join("notes.txt"),
            serde_json::to_vec(&sample_report("notes", 9999, None)).unwrap(),
        )
        .expect("write txt");

        let list = store.list_reports().expect("list_reports");
        assert_eq!(
            list,
            vec![
                ReportSummary {
                    report_id: "r-new".to_string(),
                    created_unix_ms: 2000,
                    has_nat: true,
                    nat_status: Some(NatStatus::PublicStunOnly),
                    has_p2p: false,
                    p2p_verdict: None,
                },
                ReportSummary {
                    report_id: "r-a".to_string(),
                    created_unix_ms: 1000,
                    has_nat: true,
                    nat_status: Some(NatStatus::CompleteFull),
                    has_p2p: false,
                    p2p_verdict: None,
                },
                ReportSummary {
                    report_id: "r-b".to_string(),
                    created_unix_ms: 1000,
                    has_nat: false,
                    nat_status: None,
                    has_p2p: false,
                    p2p_verdict: None,
                },
            ]
        );
    }

    #[test]
    fn save_and_list_p2p_report_summary() {
        let dir = TestDir::new("p2p_summary");
        let store = LocalStore::new(dir.path());
        let report = AssessmentReport {
            report_id: "p2p_123".to_string(),
            created_unix_ms: 5000,
            nat_report: None,
            p2p_report: Some(P2pReport {
                punched: true,
                peer_label: Some("bob".to_string()),
                active_peer_addr: Some("127.0.0.1:41000".to_string()),
                ip_family: Some(IpFamily::Ipv4),
                punch_elapsed_ms: Some(120),
                sent: 5,
                received: 5,
                loss_ratio: 0.0,
                avg_rtt_ms: Some(1.5),
                p50_rtt_ms: Some(1.5),
                p95_rtt_ms: Some(2.0),
                jitter_ms: Some(0.2),
                iqr_ms: Some(0.0),
                tail_bloat_ms: Some(0.5),
                spike_ratio: Some(0.0),
                latency_tier: Some(P2pLatencyTier::UltraLow),
                stability_grade: Some(P2pStabilityGrade::RockSolid),
                series: vec![LatencyPoint {
                    seq: 1,
                    elapsed_ms: 10,
                    rtt_ms: Some(1.5),
                }],
                verdict: P2pVerdict::Excellent,
                failure_reason: None,
                interrupted_reason: None,
            }),
            storage_warning: None,
        };
        store.save_report(&report).expect("save p2p");
        let list = store.list_reports().expect("list p2p");
        assert_eq!(list.len(), 1);
        assert!(!list[0].has_nat);
        assert!(list[0].has_p2p);
        assert_eq!(list[0].p2p_verdict, Some(P2pVerdict::Excellent));
        assert_eq!(store.load_report("p2p_123").expect("load p2p"), report);
    }

    #[test]
    fn delete_and_missing_report_behavior() {
        let dir = TestDir::new("delete");
        let store = LocalStore::new(dir.path());

        let report = sample_report("del-1", 500, None);
        store.save_report(&report).expect("save");
        store.delete_report("del-1").expect("delete");

        assert_eq!(
            store.load_report("del-1"),
            Err(ProbeError::InvalidInput(
                "report_not_found: del-1".to_string()
            ))
        );
        assert_eq!(
            store.delete_report("del-1"),
            Err(ProbeError::InvalidInput(
                "report_not_found: del-1".to_string()
            ))
        );
    }

    #[test]
    fn load_corrupt_report_returns_internal_error() {
        let dir = TestDir::new("corrupt_load");
        fs::create_dir_all(dir.path()).expect("create dir");
        fs::write(dir.path().join("bad-1.json"), b"{").expect("write bad json");

        let store = LocalStore::new(dir.path());
        assert!(matches!(
            store.load_report("bad-1"),
            Err(ProbeError::Internal(_))
        ));
    }

    #[test]
    fn rejects_invalid_ids_and_accepts_max_len() {
        let dir = TestDir::new("invalid_ids");
        let store = LocalStore::new(dir.path());

        let too_long = "a".repeat(129);
        let invalid_ids = [
            "",
            too_long.as_str(),
            "../escape",
            "a/b",
            "a\\b",
            "a..b",
            "a\0b",
            "bad id",
            "file.json",
        ];

        for bad in invalid_ids {
            let report = sample_report(bad, 1, None);
            assert!(
                matches!(store.save_report(&report), Err(ProbeError::InvalidInput(_))),
                "save should reject {bad:?}"
            );
            assert!(
                matches!(store.load_report(bad), Err(ProbeError::InvalidInput(_))),
                "load should reject {bad:?}"
            );
            assert!(
                matches!(store.delete_report(bad), Err(ProbeError::InvalidInput(_))),
                "delete should reject {bad:?}"
            );
        }

        let max_valid = format!("{}AZ09_-", "x".repeat(122));
        assert_eq!(max_valid.len(), 128);
        let report = sample_report(&max_valid, 42, None);
        store.save_report(&report).expect("save 128-char id");
        assert_eq!(store.load_report(&max_valid).expect("load 128-char id"), report);
    }
}
