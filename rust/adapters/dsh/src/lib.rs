mod loader;
mod parser;
mod paths;
mod report;

use ccusage_core::{
    PricingMap, Result,
    cli::{AgentCommandArgs, CostMode},
    first_column, log_level, print_json_or_jq, print_usage_table, sort_summaries, summary_period,
    wants_json,
};

pub use loader::load_entries;
pub use report::summarize_entries;

pub fn run(args: AgentCommandArgs) -> Result<()> {
    let kind = args.kind;
    let shared = args.shared;
    let pricing = (shared.mode != CostMode::Display).then(|| {
        PricingMap::load_with_overrides(
            shared.offline,
            log_level() != Some(0),
            shared.pricing_overrides.iter(),
        )
    });
    let mut entries = load_entries(&shared, pricing.as_ref())?;
    ccusage_adapter_common::filter_loaded_entries_by_date(&mut entries, &shared);
    if wants_json(&shared) {
        return print_json_or_jq(
            report::report_from_rows(&summarize_entries(&entries, kind)?, kind),
            shared.jq.as_deref(),
            shared.no_cost,
        );
    }
    let mut rows = summarize_entries(&entries, kind)?;
    sort_summaries(&mut rows, &shared.order, summary_period);
    print_usage_table(
        "DeepSeek Harness Token Usage Report",
        first_column(kind),
        &rows,
        &shared,
        false,
        None,
    )?;
    Ok(())
}

pub fn has_data() -> bool {
    paths::discover_session_files().is_ok_and(|files| !files.is_empty())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use ccusage_core::cli::{AgentReportKind, CostMode, SharedArgs};
    use ccusage_test_support::{EnvVarsGuard, fs_fixture};

    use super::*;

    #[test]
    fn generation_selection_prevents_double_counting_a_migrated_session() {
        let v0 = [
            r#"{"type":"session","version":0,"id":"s","createdAt":1780000000000}"#,
            r#"{"type":"step/start","time":1780000000001,"data":{"turn":1,"step":1}}"#,
            r#"{"type":"assistant/message","time":1780000000002,"data":{"turn":1,"step":1,"usage":{"inputTokens":2,"outputTokens":1},"message":{"source":{"provider":"p","model":"m"}}}}"#,
        ]
        .join("\n");
        let v3 = v0
            .replace("\"version\":0", "\"version\":3")
            .replace("\"inputTokens\":2", "\"inputTokens\":7");
        let fixture = fs_fixture!({
            "sessions/project/s/session.jsonl": v0,
            "sessions/project/s/session.v3.jsonl": v3,
        });
        let _guard = EnvVarsGuard::set_many([(
            "DSH_HOME",
            Some(OsString::from(fixture.root().as_os_str())),
        )]);
        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };

        let entries = load_entries(&shared, None).unwrap();
        let rows = summarize_entries(&entries, AgentReportKind::Daily).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(rows[0].input_tokens, 7);
    }
}
