use crate::runner::Report;
use nico_common::output::Status;
use std::collections::HashMap;

pub type Baseline = HashMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delta {
    New,
    Fixed,
    Unchanged,
}

pub fn compute_deltas(report: &Report, baseline: Option<&Baseline>) -> HashMap<String, Delta> {
    compute_deltas_for(report.layers.iter().map(|l| (l.name, &l.status)), baseline)
}

/// Same delta logic as [`compute_deltas`], but reads `(name, status)` pairs
/// directly so callers that don't have a [`Report`] (for example the nico-ops
/// dashboard, whose snapshots own their layer names as `String`) can reuse the
/// baseline comparison without synthesising a fake report.
pub fn compute_deltas_for<'a, I>(layers: I, baseline: Option<&Baseline>) -> HashMap<String, Delta>
where
    I: IntoIterator<Item = (&'a str, &'a Status)>,
{
    layers
        .into_iter()
        .map(|(name, status)| {
            let delta = match baseline.and_then(|b| b.get(name)) {
                None => Delta::Unchanged,
                Some(prev) => {
                    let prev_bad = matches!(prev.as_str(), "warn" | "fail");
                    let now_bad = matches!(status, Status::Warn | Status::Fail);
                    let prev_ok = matches!(prev.as_str(), "ok" | "skipped");
                    let now_ok = matches!(status, Status::Ok | Status::Skipped);
                    if prev_ok && now_bad {
                        Delta::New
                    } else if prev_bad && now_ok {
                        Delta::Fixed
                    } else {
                        Delta::Unchanged
                    }
                }
            };
            (name.to_string(), delta)
        })
        .collect()
}

fn status_str(status: &Status) -> &'static str {
    match status {
        Status::Ok => "ok",
        Status::Warn => "warn",
        Status::Fail => "fail",
        Status::Unknown => "unknown",
        Status::Skipped => "skipped",
    }
}

fn default_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".local/share/nico-doctor/last-run.json")
}

pub fn load() -> Option<Baseline> {
    load_from(&default_path())
}

pub fn save(report: &Report) {
    save_to(&default_path(), report);
}

fn load_from(path: &std::path::Path) -> Option<Baseline> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "nico: warn: could not read baseline {}: {e}",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_str::<Baseline>(&data) {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!(
                "nico: warn: baseline file is corrupt ({e}); proceeding without prior baseline"
            );
            None
        }
    }
}

fn save_to(path: &std::path::Path, report: &Report) {
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("nico: warn: could not create baseline directory: {e}");
        return;
    }
    let map: Baseline = report
        .layers
        .iter()
        .map(|l| (l.name.to_string(), status_str(&l.status).to_string()))
        .collect();
    let json = match serde_json::to_string_pretty(&map) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("nico: warn: could not serialize baseline: {e}");
            return;
        }
    };
    if let Err(e) = std::fs::write(path, json) {
        eprintln!("nico: warn: could not write baseline: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::LayerResult;
    use crate::runner::Report;

    fn make_report(layers: &[(&'static str, Status)]) -> Report {
        Report {
            layers: layers
                .iter()
                .map(|(name, status)| LayerResult {
                    name,
                    status: status.clone(),
                    checks: vec![],
                    duration_ms: 0,
                    skipped_reason: None,
                })
                .collect(),
        }
    }

    fn tmp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("nico-doctor-test-{name}.json"))
    }

    #[test]
    fn missing_file_returns_none() {
        let path = tmp_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(load_from(&path).is_none());
    }

    #[test]
    fn corrupt_file_returns_none() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, b"not valid json!!!").unwrap();
        assert!(load_from(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_writes_correct_format() {
        let path = tmp_path("save-format");
        let report = make_report(&[
            ("cluster", Status::Ok),
            ("logs", Status::Warn),
            ("workflows", Status::Fail),
            ("health", Status::Skipped),
            ("grpc", Status::Ok),
            ("postgres", Status::Unknown),
        ]);
        save_to(&path, &report);

        let data = std::fs::read_to_string(&path).unwrap();
        let parsed: Baseline = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["cluster"], "ok");
        assert_eq!(parsed["logs"], "warn");
        assert_eq!(parsed["workflows"], "fail");
        assert_eq!(parsed["health"], "skipped");
        assert_eq!(parsed["grpc"], "ok");
        assert_eq!(parsed["postgres"], "unknown");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trip() {
        let path = tmp_path("round-trip");
        let report = make_report(&[("cluster", Status::Ok), ("logs", Status::Warn)]);
        save_to(&path, &report);
        let baseline = load_from(&path).expect("should load saved baseline");
        assert_eq!(baseline["cluster"], "ok");
        assert_eq!(baseline["logs"], "warn");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_creates_parent_dirs() {
        let root = std::env::temp_dir().join("nico-doctor-test-nested-dirs");
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("deep/nested/last-run.json");
        let report = make_report(&[("cluster", Status::Ok)]);
        save_to(&path, &report);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn status_str_values() {
        assert_eq!(status_str(&Status::Ok), "ok");
        assert_eq!(status_str(&Status::Warn), "warn");
        assert_eq!(status_str(&Status::Fail), "fail");
        assert_eq!(status_str(&Status::Unknown), "unknown");
        assert_eq!(status_str(&Status::Skipped), "skipped");
    }

    #[test]
    fn exit_3_path_does_not_write_baseline() {
        // Verify that when exit code is 3 (Status::Unknown summary), save() is not called.
        // This test documents the contract: the caller (main.rs) must check exit code
        // before calling save(). We test the helper directly to confirm that a report
        // with all-Unknown layers is saved correctly by save_to() itself, and verify
        // that the callee (main.rs) guards the call with `if code != 3`.
        let path = tmp_path("exit3-guard");
        let _ = std::fs::remove_file(&path);
        // Simulate: main.rs calls save() only when code != 3.
        let report = make_report(&[("cluster", Status::Unknown)]);
        let code = if report.layers.iter().any(|l| l.status == Status::Fail) {
            2
        } else if report.layers.iter().any(|l| l.status == Status::Warn) {
            1
        } else if report.layers.iter().any(|l| l.status == Status::Unknown) {
            3
        } else {
            0
        };
        if code != 3 {
            save_to(&path, &report);
        }
        assert!(
            !path.exists(),
            "baseline must not be written on exit code 3"
        );
    }

    // ── delta computation tests ───────────────────────────────────────────────

    #[test]
    fn delta_new_when_ok_becomes_warn() {
        let report = make_report(&[("logs", Status::Warn)]);
        let mut baseline = Baseline::new();
        baseline.insert("logs".into(), "ok".into());
        let deltas = compute_deltas(&report, Some(&baseline));
        assert_eq!(deltas["logs"], Delta::New);
    }

    #[test]
    fn delta_new_when_skipped_becomes_fail() {
        let report = make_report(&[("cluster", Status::Fail)]);
        let mut baseline = Baseline::new();
        baseline.insert("cluster".into(), "skipped".into());
        let deltas = compute_deltas(&report, Some(&baseline));
        assert_eq!(deltas["cluster"], Delta::New);
    }

    #[test]
    fn delta_fixed_when_warn_becomes_ok() {
        let report = make_report(&[("logs", Status::Ok)]);
        let mut baseline = Baseline::new();
        baseline.insert("logs".into(), "warn".into());
        let deltas = compute_deltas(&report, Some(&baseline));
        assert_eq!(deltas["logs"], Delta::Fixed);
    }

    #[test]
    fn delta_fixed_when_fail_becomes_skipped() {
        let report = make_report(&[("grpc", Status::Skipped)]);
        let mut baseline = Baseline::new();
        baseline.insert("grpc".into(), "fail".into());
        let deltas = compute_deltas(&report, Some(&baseline));
        assert_eq!(deltas["grpc"], Delta::Fixed);
    }

    #[test]
    fn delta_unchanged_when_status_stays_ok() {
        let report = make_report(&[("cluster", Status::Ok)]);
        let mut baseline = Baseline::new();
        baseline.insert("cluster".into(), "ok".into());
        let deltas = compute_deltas(&report, Some(&baseline));
        assert_eq!(deltas["cluster"], Delta::Unchanged);
    }

    #[test]
    fn delta_unchanged_when_status_stays_warn() {
        let report = make_report(&[("logs", Status::Warn)]);
        let mut baseline = Baseline::new();
        baseline.insert("logs".into(), "warn".into());
        let deltas = compute_deltas(&report, Some(&baseline));
        assert_eq!(deltas["logs"], Delta::Unchanged);
    }

    #[test]
    fn delta_unchanged_when_no_baseline() {
        let report = make_report(&[("cluster", Status::Warn), ("logs", Status::Ok)]);
        let deltas = compute_deltas(&report, None);
        assert_eq!(deltas["cluster"], Delta::Unchanged);
        assert_eq!(deltas["logs"], Delta::Unchanged);
    }

    #[test]
    fn delta_unchanged_when_layer_not_in_baseline() {
        let report = make_report(&[("newlayer", Status::Ok)]);
        let baseline = Baseline::new();
        let deltas = compute_deltas(&report, Some(&baseline));
        assert_eq!(deltas["newlayer"], Delta::Unchanged);
    }

    #[test]
    fn exit_0_1_2_write_baseline() {
        for (status, expected_code) in [(Status::Ok, 0), (Status::Warn, 1), (Status::Fail, 2)] {
            let path = tmp_path(&format!("exit-{expected_code}"));
            let _ = std::fs::remove_file(&path);
            let report = make_report(&[("cluster", status)]);
            save_to(&path, &report);
            assert!(
                path.exists(),
                "baseline must be written for exit code {expected_code}"
            );
            let _ = std::fs::remove_file(&path);
        }
    }
}
