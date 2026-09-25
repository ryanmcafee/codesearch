use super::*;
use crate::db_discovery::repos::ReposConfig;
use crate::index::governor::{GateInputs, GateLimits, IndexPriority};
use std::collections::{BTreeMap, HashSet};

fn is_metric_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

fn is_value(value: &str) -> bool {
    matches!(value, "+Inf" | "-Inf" | "NaN") || value.parse::<f64>().is_ok()
}

/// Parse `key="value",...}` and return the unescaped labels plus the rest of the line.
fn parse_labels(mut rest: &str) -> Result<(BTreeMap<String, String>, &str), String> {
    let mut labels = BTreeMap::new();
    loop {
        if let Some(after) = rest.strip_prefix('}') {
            return Ok((labels, after));
        }
        let (key, after) = rest.split_once("=\"").ok_or("label without =\"")?;
        if !is_metric_name(key) || key.contains(':') {
            return Err(format!("bad label name {key:?}"));
        }
        let mut value = String::new();
        let mut chars = after.char_indices();
        let end = loop {
            match chars.next().ok_or("unterminated label value")? {
                (_, '\\') => match chars.next().ok_or("dangling escape")?.1 {
                    '\\' => value.push('\\'),
                    '"' => value.push('"'),
                    'n' => value.push('\n'),
                    c => return Err(format!("bad escape \\{c}")),
                },
                (_, '\n') => return Err("raw newline in label".to_string()),
                (i, '"') => break i,
                (_, c) => value.push(c),
            }
        };
        labels.insert(key.to_string(), value);
        rest = &after[end + 1..];
        rest = rest.strip_prefix(',').unwrap_or(rest);
    }
}

#[derive(Debug)]
struct Sample {
    name: String,
    labels: BTreeMap<String, String>,
    value: String,
}

/// Strict text-format 0.0.4 check: HELP/TYPE once per family before its samples, valid names, labels and values.
fn parse(body: &str) -> Result<(BTreeMap<String, String>, Vec<Sample>), String> {
    let mut types: BTreeMap<String, String> = BTreeMap::new();
    let mut helped = HashSet::new();
    let mut samples = Vec::new();
    let mut current: Option<String> = None;
    for (n, line) in body.lines().enumerate() {
        let err = |msg: String| format!("line {}: {msg}: {line:?}", n + 1);
        if let Some(help) = line.strip_prefix("# HELP ") {
            let (name, _) = help
                .split_once(' ')
                .ok_or_else(|| err("HELP without text".into()))?;
            if !is_metric_name(name) || !helped.insert(name.to_string()) {
                return Err(err("bad or repeated HELP".into()));
            }
        } else if let Some(ty) = line.strip_prefix("# TYPE ") {
            let (name, kind) = ty
                .split_once(' ')
                .ok_or_else(|| err("TYPE without kind".into()))?;
            if !matches!(kind, "counter" | "gauge" | "histogram") {
                return Err(err(format!("unknown type {kind}")));
            }
            if types.insert(name.to_string(), kind.to_string()).is_some() {
                return Err(err("repeated TYPE".into()));
            }
            current = Some(name.to_string());
        } else if line.starts_with('#') || line.is_empty() {
            return Err(err("unexpected comment or blank line".into()));
        } else {
            let split = line
                .find(['{', ' '])
                .ok_or_else(|| err("no value".into()))?;
            let name = &line[..split];
            let (labels, rest) = match line[split..].strip_prefix('{') {
                Some(after) => parse_labels(after).map_err(err)?,
                None => (BTreeMap::new(), &line[split..]),
            };
            let value = rest
                .strip_prefix(' ')
                .ok_or_else(|| err("no space before value".into()))?;
            if !is_metric_name(name) || !is_value(value) {
                return Err(err("bad name or value".into()));
            }
            let family = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|suffix| {
                    name.strip_suffix(suffix)
                        .filter(|base| types.get(*base).is_some_and(|k| k == "histogram"))
                })
                .unwrap_or(name);
            if current.as_deref() != Some(family) {
                return Err(err(format!("sample of {family} outside its family block")));
            }
            samples.push(Sample {
                name: name.to_string(),
                labels,
                value: value.to_string(),
            });
        }
    }
    Ok((types, samples))
}

#[test]
fn label_values_escape_backslash_quote_and_newline() {
    assert_eq!(escape_label(r#"a\b"c"#), r#"a\\b\"c"#);
    assert_eq!(escape_label("line1\nline2"), r"line1\nline2");
    assert_eq!(escape_label("plain /path"), "plain /path");

    let mut m = Exposition::default();
    m.family("x", Kind::Gauge, "help with \\ and\nnewline")
        .sample("x", &[("path", "C:\\r\"e\"\npo")], 1.0);
    let body = m.finish();
    let (_, samples) = parse(&body).unwrap();
    assert_eq!(samples[0].labels["path"], "C:\\r\"e\"\npo");
    assert!(
        body.starts_with("# HELP x help with \\\\ and\\nnewline\n"),
        "{body}"
    );
}

#[test]
fn values_use_prometheus_spellings() {
    assert_eq!(format_value(f64::INFINITY), "+Inf");
    assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
    assert_eq!(format_value(f64::NAN), "NaN");
    assert_eq!(format_value(1.0), "1");
    assert_eq!(format_value(0.025), "0.025");
}

#[test]
fn histograms_emit_cumulative_buckets_then_inf_sum_and_count() {
    let mut h = Histogram::default();
    for secs in [0.005, 0.2, 3.0, 45.0] {
        h.observe(secs);
    }
    let mut m = Exposition::default();
    m.family("d_seconds", Kind::Histogram, "d")
        .histogram("d_seconds", &[("tool", "search")], &h);
    let (_, samples) = parse(&m.finish()).unwrap();

    let buckets: Vec<(&str, f64)> = samples
        .iter()
        .filter(|s| s.name == "d_seconds_bucket")
        .map(|s| (s.labels["le"].as_str(), s.value.parse().unwrap()))
        .collect();
    assert_eq!(buckets.len(), TOOL_CALL_BUCKETS_SECS.len() + 1);
    assert!(buckets.windows(2).all(|w| w[0].1 <= w[1].1), "{buckets:?}");
    assert_eq!(buckets[0], ("0.01", 1.0));
    assert!(buckets.contains(&("0.25", 2.0)));
    assert!(buckets.contains(&("5", 3.0)));
    assert!(buckets.contains(&("30", 3.0)));
    assert_eq!(buckets.last(), Some(&("+Inf", 4.0)));
    assert!(samples.iter().all(|s| s.labels["tool"] == "search"));
    let value = |name: &str| {
        samples
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .value
            .clone()
    };
    assert_eq!(value("d_seconds_count"), "4");
    assert!((value("d_seconds_sum").parse::<f64>().unwrap() - 48.205).abs() < 1e-9);
}

#[test]
fn every_governor_pause_reason_maps_to_a_bounded_category() {
    let limits = GateLimits {
        read_target_ms: 1_000,
        max_other_cpu_percent: 50.0,
        pause_on_battery: true,
    };
    let cases = [
        (
            GateInputs {
                recent_read_max_ms: Some(4_000),
                ..GateInputs::default()
            },
            "read_latency",
        ),
        (
            GateInputs {
                memory_pressure: Some(MemoryPressure::Critical),
                ..GateInputs::default()
            },
            "memory_pressure",
        ),
        (
            GateInputs {
                memory_pressure: Some(MemoryPressure::Warn),
                ..GateInputs::default()
            },
            "memory_pressure",
        ),
        (
            GateInputs {
                other_cpu_percent: Some(90.0),
                ..GateInputs::default()
            },
            "cpu",
        ),
        (
            GateInputs {
                on_battery: Some(true),
                ..GateInputs::default()
            },
            "battery",
        ),
    ];
    for (inputs, expected) in cases {
        let reason = governor::evaluate(&inputs, &limits, IndexPriority::Background).unwrap();
        assert_eq!(pause_reason(&reason), expected, "{reason}");
    }
    assert_eq!(pause_reason("something new"), "other");
    assert!(PAUSE_REASONS.contains(&"other"));
}

#[tokio::test]
async fn metrics_endpoint_serves_valid_exposition_with_every_family() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = ReposConfig::default();
    config
        .repos
        .insert("metrics-svc".to_string(), tmp.path().join("metrics-svc"));
    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();
    let state = Arc::new(ServeState::new(config, Some(config_file)));
    telemetry::record_tool_call("search", Duration::from_millis(30), true);
    telemetry::record_tool_call("find", Duration::from_millis(12), false);
    let repo_path = tmp.path().join("metrics-svc").display().to_string();
    health::log().job_finished(&repo_path, Duration::from_millis(250), Ok(()));

    let response = metrics_handler(State(state)).await.into_response();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    let (types, samples) = parse(&body).unwrap_or_else(|e| panic!("{e}\n{body}"));

    for (family, kind) in [
        ("codesearch_build_info", "gauge"),
        ("codesearch_uptime_seconds", "gauge"),
        ("codesearch_slo_seconds", "gauge"),
        ("codesearch_read_target_seconds", "gauge"),
        ("codesearch_status", "gauge"),
        ("codesearch_degraded_reasons", "gauge"),
        ("codesearch_tool_call_duration_seconds", "histogram"),
        ("codesearch_tool_call_failures_total", "counter"),
        ("codesearch_tool_call_latency_seconds", "gauge"),
        ("codesearch_tool_call_window_calls", "gauge"),
        ("codesearch_index_governor_max_jobs", "gauge"),
        ("codesearch_index_governor_jobs", "gauge"),
        ("codesearch_index_governor_paused_jobs", "gauge"),
        ("codesearch_qos_info", "gauge"),
        ("codesearch_repos", "gauge"),
        ("codesearch_repo_info", "gauge"),
        ("codesearch_repo_last_indexed_timestamp_seconds", "gauge"),
        ("codesearch_index_events_total", "counter"),
    ] {
        assert_eq!(
            types.get(family).map(String::as_str),
            Some(kind),
            "{family}"
        );
    }
    let find = |name: &str, labels: &[(&str, &str)]| {
        samples.iter().find(|s| {
            s.name == name
                && labels
                    .iter()
                    .all(|(k, v)| s.labels.get(*k).map(String::as_str) == Some(*v))
        })
    };
    assert!(find(
        "codesearch_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))]
    )
    .is_some());
    assert!(find(
        "codesearch_tool_call_duration_seconds_bucket",
        &[("tool", "search"), ("le", "+Inf")]
    )
    .is_some());
    assert!(
        find("codesearch_tool_call_failures_total", &[("tool", "find")]).is_some_and(|s| s
            .value
            .parse::<f64>()
            .unwrap()
            >= 1.0)
    );
    assert!(find(
        "codesearch_tool_call_latency_seconds",
        &[("tool", "all"), ("window", "1h"), ("quantile", "0.95")]
    )
    .is_some());
    assert!(find(
        "codesearch_repo_info",
        &[("repo", "metrics-svc"), ("path", repo_path.as_str())]
    )
    .is_some());
    assert!(find(
        "codesearch_repo_last_index_duration_seconds",
        &[("repo", "metrics-svc")]
    )
    .is_some_and(|s| s.value == "0.25"));
    let statuses: f64 = samples
        .iter()
        .filter(|s| s.name == "codesearch_status")
        .map(|s| s.value.parse::<f64>().unwrap())
        .sum();
    assert_eq!(statuses, 1.0);
}
