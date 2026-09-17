use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use jiff::tz::TimeZone as JiffTimeZone;
use serde_json::Value;

use crate::paths::session_file_version;
use ccusage_core::{
    LoadedEntry, PricingMap, Result, TimestampMs, TokenUsageRaw, UsageEntry, UsageMessage,
    calculate_cost_from_pricing,
    cli::{CostMode, SharedArgs},
    format_date_tz, format_rfc3339_millis, parse_tz,
};

const DEFAULT_DSH_MODEL: &str = "unknown";

#[derive(Clone, Debug)]
struct Route {
    provider: String,
    model: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct StepKey {
    turn: u64,
    step: u64,
}

#[derive(Clone, Debug)]
struct AttemptState {
    index: u32,
    timestamp: TimestampMs,
    usage: Option<ParsedUsage>,
    route: Option<Route>,
    closed: bool,
}

#[derive(Clone, Debug)]
struct AttemptSample {
    key: StepKey,
    index: u32,
    timestamp: TimestampMs,
    usage: ParsedUsage,
    route: Option<Route>,
}

#[derive(Clone, Copy, Debug)]
struct ParsedUsage {
    raw: TokenUsageRaw,
    extra_total_tokens: u64,
}

pub(super) fn read_session_file(
    file: &Path,
    shared: &SharedArgs,
    pricing: Option<&PricingMap>,
) -> Result<Vec<LoadedEntry>> {
    let fallback = file_timestamp(file);
    let bytes = fs::read(file)?;
    let content = if session_file_version(file).is_some_and(|version| version.compressed) {
        zstd::stream::decode_all(bytes.as_slice())?
    } else {
        bytes
    };
    let mut records = ccusage_adapter_common::jsonl::records::<Value>(&content, None);
    let Some(header) = records.next().filter(|record| record["type"] == "session") else {
        return Ok(Vec::new());
    };
    let Some(session_id) = string_at(&header, &["id"]) else {
        return Ok(Vec::new());
    };
    let file_generation = session_file_version(file).map(|version| version.generation);
    let header_generation =
        u64_at(&header, &["version"]).and_then(|value| u32::try_from(value).ok());
    if file_generation != header_generation {
        return Ok(Vec::new());
    }
    let project_path = string_at(&header, &["cwd"]).unwrap_or_else(|| "unknown".to_string());
    let created_at = timestamp_at(&header, &["createdAt"]).unwrap_or(fallback);
    let mut current_route = None;
    let mut states = HashMap::<StepKey, AttemptState>::new();
    let mut samples = Vec::new();

    for record in records {
        let event_type = record.get("type").and_then(Value::as_str);
        let Some(data) = record.get("data") else {
            continue;
        };
        let event_time = timestamp_at(&record, &["time"]).unwrap_or(created_at);
        match event_type {
            Some("request/header") => {
                current_route = route_at(data, &["header", "config"]);
            }
            Some("request/context") => {
                current_route = route_at(data, &[]);
            }
            Some("step/start") => {
                if let Some(key) = step_key(data) {
                    states.insert(
                        key,
                        AttemptState {
                            index: 0,
                            timestamp: event_time,
                            usage: None,
                            route: current_route.clone(),
                            closed: false,
                        },
                    );
                }
            }
            Some("llm/retry-started") => {
                if let Some(key) = step_key(data) {
                    let next = states
                        .get(&key)
                        .map_or(0, |state| state.index.saturating_add(1));
                    states.insert(
                        key,
                        AttemptState {
                            index: next,
                            timestamp: event_time,
                            usage: None,
                            route: current_route.clone(),
                            closed: false,
                        },
                    );
                }
            }
            Some("assistant/chunk") => {
                if let (Some(key), Some(usage)) = (step_key(data), usage_at(data, &["chunk"])) {
                    let state = states.entry(key).or_insert_with(|| AttemptState {
                        index: 0,
                        timestamp: event_time,
                        usage: None,
                        route: current_route.clone(),
                        closed: false,
                    });
                    if !state.closed {
                        state.usage = Some(usage);
                    }
                }
            }
            Some("assistant/attempt") => {
                if let Some(key) = step_key(data) {
                    let usage = stream_usage(data.get("stream"));
                    close_attempt(
                        &mut states,
                        &mut samples,
                        key,
                        event_time,
                        usage,
                        current_route.clone(),
                    );
                }
            }
            Some("assistant/message") => {
                if let Some(key) = step_key(data) {
                    let explicit_usage = data.get("usage");
                    let usage = explicit_usage
                        .and_then(|value| usage_from_value(Some(value)))
                        .or_else(|| {
                            explicit_usage
                                .is_none()
                                .then(|| stream_usage(data.get("stream")))
                                .flatten()
                        });
                    if explicit_usage.is_some()
                        && usage.is_none()
                        && let Some(state) = states.get_mut(&key)
                    {
                        state.usage = None;
                    }
                    let route =
                        route_at(data, &["message", "source"]).or_else(|| current_route.clone());
                    close_attempt(&mut states, &mut samples, key, event_time, usage, route);
                }
            }
            Some("llm/retry") | Some("step/end") => {
                if let Some(key) = step_key(data) {
                    close_attempt(
                        &mut states,
                        &mut samples,
                        key,
                        event_time,
                        None,
                        current_route.clone(),
                    );
                    if event_type == Some("step/end") {
                        states.remove(&key);
                    }
                }
            }
            _ => {}
        }
    }

    let tz = parse_tz(shared.timezone.as_deref());
    Ok(samples
        .into_iter()
        .filter(|sample| parsed_total_tokens(sample.usage) > 0)
        .map(|sample| {
            to_loaded_entry(
                &session_id,
                &project_path,
                sample,
                tz.as_ref(),
                shared.mode,
                pricing,
            )
        })
        .collect())
}

fn close_attempt(
    states: &mut HashMap<StepKey, AttemptState>,
    samples: &mut Vec<AttemptSample>,
    key: StepKey,
    event_time: TimestampMs,
    usage: Option<ParsedUsage>,
    route: Option<Route>,
) {
    let state = states.entry(key).or_insert_with(|| AttemptState {
        index: 0,
        timestamp: event_time,
        usage: None,
        route: route.clone(),
        closed: false,
    });
    if state.closed {
        return;
    }
    if usage.is_some() {
        state.usage = usage;
    }
    if route.is_some() {
        state.route = route;
    }
    if let Some(usage) = state.usage {
        samples.push(AttemptSample {
            key,
            index: state.index,
            timestamp: state.timestamp,
            usage,
            route: state.route.clone(),
        });
    }
    state.closed = true;
}

fn step_key(data: &Value) -> Option<StepKey> {
    Some(StepKey {
        turn: u64_at(data, &["turn"])?,
        step: u64_at(data, &["step"])?,
    })
}

fn stream_usage(value: Option<&Value>) -> Option<ParsedUsage> {
    fn visit(value: &Value, latest: &mut Option<ParsedUsage>) {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, latest);
                }
            }
            Value::Object(values) => {
                if values.get("type").and_then(Value::as_str) == Some("usage")
                    && let Some(usage) = usage_from_value(values.get("usage"))
                {
                    *latest = Some(usage);
                }
                for value in values.values() {
                    visit(value, latest);
                }
            }
            _ => {}
        }
    }
    let mut latest = None;
    if let Some(value) = value {
        visit(value, &mut latest);
    }
    latest
}

fn usage_at(data: &Value, path: &[&str]) -> Option<ParsedUsage> {
    let value = at(data, path)?;
    if value.get("type").and_then(Value::as_str) != Some("usage") {
        return None;
    }
    usage_from_value(value.get("usage"))
}

fn usage_from_value(value: Option<&Value>) -> Option<ParsedUsage> {
    let value = value?;
    let input = value.get("inputTokens")?.as_u64()?;
    let output = value.get("outputTokens")?.as_u64()?;
    let cache_read = optional_u64(value, "cacheReadTokens")?;
    let cache_write = optional_u64(value, "cacheWriteTokens")?;
    let reasoning = optional_u64(value, "reasoningTokens")?;
    if reasoning.is_some_and(|reasoning| reasoning > output) {
        return None;
    }
    let known_total = input
        .checked_add(output)?
        .checked_add(cache_read.unwrap_or(0))?
        .checked_add(cache_write.unwrap_or(0))?;
    let extra_total_tokens = if let Some(total) = optional_u64(value, "totalTokens")? {
        if total < known_total
            || (cache_read.is_some() && cache_write.is_some() && total != known_total)
        {
            return None;
        }
        total - known_total
    } else {
        0
    };
    Some(ParsedUsage {
        raw: TokenUsageRaw {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: cache_write.unwrap_or(0),
            cache_read_input_tokens: cache_read.unwrap_or(0),
            cache_creation: None,
            speed: None,
        },
        extra_total_tokens,
    })
}

fn optional_u64(value: &Value, key: &str) -> Option<Option<u64>> {
    match value.get(key) {
        Some(value) => value.as_u64().map(Some),
        None => Some(None),
    }
}

fn route_at(value: &Value, path: &[&str]) -> Option<Route> {
    let value = at(value, path)?;
    Some(Route {
        provider: string_at(value, &["provider"])?,
        model: string_at(value, &["model"])?,
    })
}

fn at<'a>(mut value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    for segment in path {
        value = value.get(segment)?;
    }
    Some(value)
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let value = at(value, path)?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn u64_at(value: &Value, path: &[&str]) -> Option<u64> {
    at(value, path)?.as_u64()
}

fn timestamp_at(value: &Value, path: &[&str]) -> Option<TimestampMs> {
    at(value, path)?
        .as_i64()
        .filter(|value| *value > 0)
        .map(TimestampMs::from_millis)
}

fn to_loaded_entry(
    session_id: &str,
    project_path: &str,
    sample: AttemptSample,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> LoadedEntry {
    let model = sample
        .route
        .as_ref()
        .map(|route| route.model.clone())
        .unwrap_or_else(|| DEFAULT_DSH_MODEL.to_string());
    let usage = sample.usage.raw;
    let cost = calculate_dsh_cost(
        sample.route.as_ref(),
        &model,
        usage,
        sample.timestamp,
        mode,
        pricing,
    );
    let missing_pricing_model =
        missing_dsh_pricing(sample.route.as_ref(), &model, usage, mode, pricing);
    let id = format!(
        "dsh:{session_id}:{}:{}:{}",
        sample.key.turn, sample.key.step, sample.index
    );
    LoadedEntry {
        data: UsageEntry {
            session_id: Some(session_id.to_string()),
            timestamp: format_rfc3339_millis(sample.timestamp),
            version: None,
            message: UsageMessage {
                usage,
                model: Some(model.clone()),
                id: Some(id),
            },
            cost_usd: None,
            request_id: None,
            is_api_error_message: None,
            is_sidechain: None,
        },
        timestamp: sample.timestamp,
        date: format_date_tz(sample.timestamp, tz),
        project: Arc::from("dsh"),
        session_id: Arc::from(session_id),
        project_path: Arc::from(project_path),
        cost,
        extra_total_tokens: sample.usage.extra_total_tokens,
        credits: None,
        message_count: Some(1),
        model: Some(model),
        usage_limit_reset_time: None,
        missing_pricing_model,
    }
}

fn calculate_dsh_cost(
    route: Option<&Route>,
    model: &str,
    usage: TokenUsageRaw,
    timestamp: TimestampMs,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> f64 {
    if mode == CostMode::Display {
        return 0.0;
    }
    let Some(pricing) = pricing else {
        return 0.0;
    };
    let provider_model = route.map(|route| format!("{}/{}", route.provider, route.model));
    let selected = provider_model
        .as_deref()
        .and_then(|candidate| pricing.find_exact(candidate))
        .or_else(|| pricing.find_at(model, timestamp));
    selected.map_or(0.0, |pricing| calculate_cost_from_pricing(usage, pricing))
}

fn missing_dsh_pricing(
    route: Option<&Route>,
    model: &str,
    usage: TokenUsageRaw,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> Option<String> {
    if mode == CostMode::Display || total_tokens(usage) == 0 {
        return None;
    }
    let pricing = pricing?;
    let provider_match = route
        .map(|route| format!("{}/{}", route.provider, route.model))
        .is_some_and(|candidate| pricing.find_exact(&candidate).is_some());
    (!provider_match && pricing.find(model).is_none()).then(|| model.to_string())
}

fn total_tokens(usage: TokenUsageRaw) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens)
}

fn parsed_total_tokens(usage: ParsedUsage) -> u64 {
    total_tokens(usage.raw).saturating_add(usage.extra_total_tokens)
}

fn file_timestamp(file: &Path) -> TimestampMs {
    fs::metadata(file)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok())
        .map(|duration| TimestampMs::from_millis(duration.as_millis().min(i64::MAX as u128) as i64))
        .unwrap_or(TimestampMs::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ccusage_test_support::fs_fixture;

    use super::*;

    fn shared() -> SharedArgs {
        SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        }
    }

    #[test]
    fn v0_final_message_replaces_its_stream_usage() {
        let fixture = fs_fixture!({
            "session.jsonl": [
                r#"{"type":"session","version":0,"id":"s","createdAt":1780000000000,"cwd":"/work"}"#,
                r#"{"type":"request/context","time":1780000000001,"data":{"provider":"deepseek","model":"deepseek-v4-pro"}}"#,
                r#"{"type":"step/start","time":1780000000002,"data":{"turn":1,"step":2}}"#,
                r#"{"type":"assistant/chunk","time":1780000000003,"data":{"turn":1,"step":2,"chunk":{"type":"usage","usage":{"inputTokens":10,"outputTokens":2}}}}"#,
                r#"{"type":"assistant/message","time":1780000000004,"data":{"turn":1,"step":2,"usage":{"inputTokens":12,"outputTokens":3,"cacheReadTokens":4,"cacheWriteTokens":1},"message":{"source":{"provider":"deepseek","model":"deepseek-v4-pro"}}}}"#,
            ].join("\n"),
        });

        let entries = read_session_file(&fixture.path("session.jsonl"), &shared(), None).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.usage.input_tokens, 12);
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 4);
        assert_eq!(entries[0].data.message.id.as_deref(), Some("dsh:s:1:2:0"));
    }

    #[test]
    fn v3_uses_explicit_usage_instead_of_counting_embedded_stream_twice() {
        let fixture = fs_fixture!({
            "session.v3.jsonl": [
                r#"{"type":"session","version":3,"id":"s3","createdAt":1780000000000,"cwd":"/work"}"#,
                r#"{"type":"step/start","time":1780000000001,"data":{"turn":4,"step":5}}"#,
                r#"{"type":"assistant/message","time":1780000000002,"data":{"turn":4,"step":5,"stream":[{"chunk":{"type":"usage","usage":{"inputTokens":20,"outputTokens":4}}}],"usage":{"inputTokens":20,"outputTokens":4,"totalTokens":24},"message":{"source":{"provider":"ada-provider","model":"kr-gpt-5.6-luna"}}}}"#,
            ].join("\n"),
        });

        let entries =
            read_session_file(&fixture.path("session.v3.jsonl"), &shared(), None).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.usage.input_tokens, 20);
        assert_eq!(entries[0].model.as_deref(), Some("kr-gpt-5.6-luna"));
    }

    #[test]
    fn retry_attempts_with_usage_are_billed_separately() {
        let fixture = fs_fixture!({
            "session.v3.jsonl": [
                r#"{"type":"session","version":3,"id":"retry","createdAt":1780000000000,"cwd":"/work"}"#,
                r#"{"type":"request/context","time":1780000000001,"data":{"provider":"deepseek","model":"deepseek-v4-pro"}}"#,
                r#"{"type":"step/start","time":1780000000002,"data":{"turn":1,"step":1}}"#,
                r#"{"type":"assistant/attempt","time":1780000000003,"data":{"turn":1,"step":1,"stream":[{"chunk":{"type":"usage","usage":{"inputTokens":5,"outputTokens":1}}}]}}"#,
                r#"{"type":"llm/retry","time":1780000000004,"data":{"turn":1,"step":1}}"#,
                r#"{"type":"llm/retry-started","time":1780000000005,"data":{"turn":1,"step":1}}"#,
                r#"{"type":"assistant/message","time":1780000000006,"data":{"turn":1,"step":1,"usage":{"inputTokens":7,"outputTokens":2},"message":{"source":{"provider":"deepseek","model":"deepseek-v4-pro"}}}}"#,
            ].join("\n"),
        });

        let entries =
            read_session_file(&fixture.path("session.v3.jsonl"), &shared(), None).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].data.message.usage.input_tokens, 5);
        assert_eq!(entries[1].data.message.usage.input_tokens, 7);
        assert_eq!(
            entries[0].data.message.id.as_deref(),
            Some("dsh:retry:1:1:0")
        );
        assert_eq!(
            entries[1].data.message.id.as_deref(),
            Some("dsh:retry:1:1:1")
        );
    }

    #[test]
    fn skips_a_filename_and_header_generation_mismatch() {
        let fixture = fs_fixture!({
            "session.v3.jsonl": r#"{"type":"session","version":2,"id":"s","createdAt":1780000000000}"#,
        });

        let entries =
            read_session_file(&fixture.path("session.v3.jsonl"), &shared(), None).unwrap();

        assert!(entries.is_empty());
    }

    #[test]
    fn preserves_an_exact_total_when_optional_cache_buckets_are_undisclosed() {
        let fixture = fs_fixture!({
            "session.v3.jsonl": [
                r#"{"type":"session","version":3,"id":"total","createdAt":1780000000000}"#,
                r#"{"type":"step/start","time":1780000000001,"data":{"turn":1,"step":1}}"#,
                r#"{"type":"assistant/message","time":1780000000002,"data":{"turn":1,"step":1,"usage":{"inputTokens":20,"outputTokens":4,"totalTokens":30},"message":{"source":{"provider":"p","model":"m"}}}}"#,
            ].join("\n"),
        });

        let entries =
            read_session_file(&fixture.path("session.v3.jsonl"), &shared(), None).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].extra_total_tokens, 6);
    }

    #[test]
    fn invalid_explicit_usage_does_not_fall_back_to_stream_or_chunk_usage() {
        let fixture = fs_fixture!({
            "session.v3.jsonl": [
                r#"{"type":"session","version":3,"id":"invalid","createdAt":1780000000000}"#,
                r#"{"type":"step/start","time":1780000000001,"data":{"turn":1,"step":1}}"#,
                r#"{"type":"assistant/chunk","time":1780000000002,"data":{"turn":1,"step":1,"chunk":{"type":"usage","usage":{"inputTokens":9,"outputTokens":2}}}}"#,
                r#"{"type":"assistant/message","time":1780000000003,"data":{"turn":1,"step":1,"stream":[{"type":"usage","usage":{"inputTokens":10,"outputTokens":2}}],"usage":{"inputTokens":10,"outputTokens":2,"cacheReadTokens":"bad"},"message":{"source":{"provider":"p","model":"m"}}}}"#,
            ].join("\n"),
        });

        let entries =
            read_session_file(&fixture.path("session.v3.jsonl"), &shared(), None).unwrap();

        assert!(entries.is_empty());
    }

    #[test]
    fn rejects_malformed_optional_token_fields() {
        let usage = serde_json::json!({
            "inputTokens": 10,
            "outputTokens": 2,
            "totalTokens": "12",
        });

        assert!(usage_from_value(Some(&usage)).is_none());
    }

    #[test]
    fn reads_concatenated_zstd_frames() {
        let fixture = fs_fixture!({});
        let path = fixture.path("session.v3.jsonl.zstd");
        let header =
            b"{\"type\":\"session\",\"version\":3,\"id\":\"z\",\"createdAt\":1780000000000}\n";
        let events = b"{\"type\":\"step/start\",\"time\":1780000000001,\"data\":{\"turn\":1,\"step\":1}}\n{\"type\":\"assistant/message\",\"time\":1780000000002,\"data\":{\"turn\":1,\"step\":1,\"usage\":{\"inputTokens\":3,\"outputTokens\":1},\"message\":{\"source\":{\"provider\":\"p\",\"model\":\"m\"}}}}\n";
        let mut encoded = zstd::stream::encode_all(header.as_slice(), 0).unwrap();
        encoded.extend(zstd::stream::encode_all(events.as_slice(), 0).unwrap());
        fs::write(&path, encoded).unwrap();

        let entries = read_session_file(&path, &shared(), None).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.usage.input_tokens, 3);
    }
}
