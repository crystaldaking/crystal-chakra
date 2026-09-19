use std::process::Command;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn doctor_stdout_is_one_json_document_with_report_and_error_paths() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("project");
    let home = directory.path().join("home");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&home)?;
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()?
            .success()
    );
    let report = directory.path().join("report.json");
    let run = |extra: &[&str]| -> Result<std::process::Output, std::io::Error> {
        Command::new(env!("CARGO_BIN_EXE_chakra"))
            .args(["doctor", "--json", "--agent", "claude", "--repo"])
            .arg(&root)
            .args(extra)
            .env("HOME", &home)
            .env("CHAKRA_UPDATE_CHECK", "0")
            .output()
    };
    let report_path = report.to_str().ok_or("non-UTF8 temporary report path")?;
    for (args, expected) in [
        (vec![], 0),
        (vec!["--report", report_path], 0),
        (vec!["--report", report_path], 1),
        (vec!["--report", report_path, "--force"], 0),
    ] {
        let output = run(&args)?;
        assert_eq!(output.status.code(), Some(expected), "{output:?}");
        let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(json["kind"], "chakra-doctor");
        assert!(json["findings"].is_array());
    }
    std::fs::write(root.join("chakra.toml"), "schema_version = 999\n")?;
    let output = run(&[])?;
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        json["findings"]
            .as_array()
            .ok_or("findings missing")?
            .iter()
            .any(|finding| {
                finding["code"] == "project-config" && finding["severity"] == "error"
            })
    );
    Ok(())
}
