use std::collections::HashSet;

use ccusage_core::{LoadedEntry, PricingMap, Result, cli::SharedArgs};

use super::parser;

pub fn load_entries(shared: &SharedArgs, pricing: Option<&PricingMap>) -> Result<Vec<LoadedEntry>> {
    ccusage_core::progress::track_usage_load(
        ccusage_core::progress::UsageLoadAgent("DeepSeek Harness"),
        shared.json,
        || {
            let files = super::paths::discover_session_files()?;
            let loaded =
                ccusage_adapter_common::read_files_parallel(&files, shared.single_thread, |file| {
                    parser::read_session_file(file, shared, pricing).unwrap_or_else(|error| {
                        ccusage_core::debug_log(
                            shared,
                            format!(
                                "Failed to read DeepSeek Harness session file {}: {error}",
                                file.display()
                            ),
                        );
                        Vec::new()
                    })
                });
            let mut entries = Vec::new();
            let mut seen = HashSet::new();
            for file_entries in loaded {
                for entry in file_entries {
                    let id = entry.data.message.id.clone();
                    if id.as_ref().is_none_or(|id| seen.insert(id.clone())) {
                        entries.push(entry);
                    }
                }
            }
            entries.sort_by_key(|entry| {
                (
                    entry.timestamp,
                    entry.session_id.to_string(),
                    entry.data.message.id.clone().unwrap_or_default(),
                )
            });
            Ok(entries)
        },
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use ccusage_core::cli::{CostMode, SharedArgs};
    use ccusage_test_support::{EnvVarsGuard, fs_fixture};

    use super::*;

    #[test]
    fn newer_generation_wins_when_a_session_is_duplicated_across_roots() {
        let fixture = fs_fixture!({
            "old/sessions/project/session-a/session.jsonl": [
                r#"{"type":"session","version":0,"id":"same","createdAt":1780000000000}"#,
                r#"{"type":"step/start","time":1780000000001,"data":{"turn":1,"step":1}}"#,
                r#"{"type":"assistant/message","time":1780000000002,"data":{"turn":1,"step":1,"usage":{"inputTokens":1,"outputTokens":1},"message":{"source":{"provider":"p","model":"m"}}}}"#,
            ].join("\n"),
            "new/sessions/project/session-a/session.v3.jsonl": [
                r#"{"type":"session","version":3,"id":"same","createdAt":1780000000000}"#,
                r#"{"type":"step/start","time":1780000000001,"data":{"turn":1,"step":1}}"#,
                r#"{"type":"assistant/message","time":1780000000002,"data":{"turn":1,"step":1,"usage":{"inputTokens":3,"outputTokens":1},"message":{"source":{"provider":"p","model":"m"}}}}"#,
            ].join("\n"),
        });
        let roots = format!(
            "{},{}",
            fixture.path("old").display(),
            fixture.path("new").display()
        );
        let _guard =
            EnvVarsGuard::set_many([(crate::paths::DSH_HOME_ENV, Some(OsString::from(roots)))]);
        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };

        let entries = load_entries(&shared, None).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.usage.input_tokens, 3);
    }
}
