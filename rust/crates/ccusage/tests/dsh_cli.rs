use ccusage_test_support::fs_fixture;

fn session_log() -> String {
    [
        r#"{"type":"session","version":3,"id":"sess-dsh","createdAt":1780000000000,"cwd":"/workspace/project"}"#,
        r#"{"type":"step/start","time":1780000000001,"data":{"turn":1,"step":1}}"#,
        r#"{"type":"assistant/message","time":1780000000002,"data":{"turn":1,"step":1,"usage":{"inputTokens":100,"outputTokens":20,"cacheReadTokens":40,"cacheWriteTokens":5,"totalTokens":165},"message":{"source":{"provider":"deepseek","model":"deepseek-v4-pro"}}}}"#,
    ]
    .join("\n")
}

#[test]
fn dsh_cli_json_reports_cover_daily_monthly_and_session_views() {
    let fixture = fs_fixture!({
        "dsh/sessions/project/sess-dsh/session.v3.jsonl": session_log(),
    });

    for (kind, rows_key) in [
        ("daily", "daily"),
        ("monthly", "monthly"),
        ("session", "sessions"),
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ccusage"))
            .env_clear()
            .env("HOME", fixture.path("empty-home"))
            .env("USERPROFILE", fixture.path("empty-userprofile"))
            .env("DSH_HOME", fixture.path("dsh"))
            .args([
                "dsh",
                kind,
                "--json",
                "--mode",
                "display",
                "--timezone",
                "UTC",
            ])
            .output()
            .expect("failed to run ccusage");

        assert!(
            output.status.success(),
            "ccusage dsh {kind} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let row = &report[rows_key][0];
        assert_eq!(row["inputTokens"], 100);
        assert_eq!(row["outputTokens"], 20);
        assert_eq!(row["cacheCreationTokens"], 5);
        assert_eq!(row["cacheReadTokens"], 40);
        assert_eq!(row["totalTokens"], 165);
        assert_eq!(row["modelsUsed"][0], "deepseek-v4-pro");
    }
}

#[test]
fn dsh_cli_tables_snapshot_production_stdout_and_stderr() {
    let fixture = fs_fixture!({
        "dsh/sessions/project/sess-dsh/session.v3.jsonl": session_log(),
    });

    for kind in ["daily", "monthly", "session"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ccusage"))
            .env_clear()
            .env("HOME", fixture.path("empty-home"))
            .env("USERPROFILE", fixture.path("empty-userprofile"))
            .env("DSH_HOME", fixture.path("dsh"))
            .args([
                "dsh",
                kind,
                "--mode",
                "display",
                "--no-color",
                "--timezone",
                "UTC",
            ])
            .output()
            .expect("failed to run ccusage");

        assert!(
            output.status.success(),
            "ccusage dsh {kind} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("CLI stdout was not UTF-8");
        let stderr = String::from_utf8(output.stderr).expect("CLI stderr was not UTF-8");
        insta::assert_snapshot!(
            format!("dsh_cli_{kind}_table"),
            format!("stdout:\n{stdout}\nstderr:\n{stderr}")
        );
    }
}
