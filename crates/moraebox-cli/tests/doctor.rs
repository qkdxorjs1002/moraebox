#![forbid(unsafe_code)]

use std::process::Command;

#[test]
fn json_doctor_probes_the_effective_cache_volume_and_remediates_failures() {
    let cache = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_morae"))
        .arg("doctor")
        .arg("--json")
        .arg("--cache-dir")
        .arg(cache.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["cache_volume"]["configured_path"],
        cache.path().to_string_lossy().as_ref()
    );
    assert!(report["cache_volume"]["available_bytes"].is_u64());
    assert!(report["cache_volume"]["reflink_supported"].is_boolean());

    let checks = report["checks"].as_array().expect("doctor checks");
    assert!(checks.len() >= 9);
    assert!(checks.iter().all(|check| {
        check["status"] == "pass"
            || check["remediation"]
                .as_str()
                .is_some_and(|remediation| !remediation.is_empty())
    }));
}
