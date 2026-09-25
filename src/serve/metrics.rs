//! `GET /metrics`: serve telemetry in the Prometheus text exposition format 0.0.4.
//! Built from the same sources as the dashboard's `/api/*` routes.

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::ServeState;
use crate::health::{self, RepoHealth};
use crate::index::governor::{self, GovernorStatus, MemoryPressure};
use crate::telemetry::{self, Histogram, LatencyStats, TOOL_CALL_BUCKETS_SECS};

pub(crate) const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Windows of the percentile gauges, matching the dashboard tiles and chart ranges.
pub(crate) const WINDOWS: [(&str, Duration); 4] = [
    ("5m", Duration::from_secs(5 * 60)),
    ("1h", Duration::from_secs(60 * 60)),
    ("6h", Duration::from_secs(6 * 60 * 60)),
    ("24h", Duration::from_secs(24 * 60 * 60)),
];

/// Categories a governor pause reason is reduced to, so labels stay bounded.
pub(crate) const PAUSE_REASONS: [&str; 5] =
    ["read_latency", "memory_pressure", "cpu", "battery", "other"];

#[derive(Debug, Clone, Copy)]
pub(crate) enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

type Labels<'a> = [(&'a str, &'a str)];
type RepoValue = fn(&RepoHealth) -> Option<f64>;

/// Hand-written text-format encoder; callers write one family header, then its samples.
#[derive(Default)]
pub(crate) struct Exposition {
    out: String,
}

impl Exposition {
    pub(crate) fn family(&mut self, name: &str, kind: Kind, help: &str) -> &mut Self {
        let help = help.replace('\\', "\\\\").replace('\n', "\\n");
        let _ = writeln!(self.out, "# HELP {name} {help}");
        let _ = writeln!(self.out, "# TYPE {name} {}", kind.as_str());
        self
    }

    pub(crate) fn sample(&mut self, name: &str, labels: &Labels<'_>, value: f64) -> &mut Self {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (key, value)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                let _ = write!(self.out, "{key}=\"{}\"", escape_label(value));
            }
            self.out.push('}');
        }
        let _ = writeln!(self.out, " {}", format_value(value));
        self
    }

    /// One labelled histogram: cumulative `_bucket` lines through `+Inf`, then `_sum` and `_count`.
    pub(crate) fn histogram(
        &mut self,
        name: &str,
        labels: &Labels<'_>,
        h: &Histogram,
    ) -> &mut Self {
        let bucket = format!("{name}_bucket");
        for (bound, count) in TOOL_CALL_BUCKETS_SECS.iter().zip(h.cumulative()) {
            let le = format_value(*bound);
            self.sample(&bucket, &with(labels, ("le", &le)), count as f64);
        }
        self.sample(&bucket, &with(labels, ("le", "+Inf")), h.count as f64);
        self.sample(&format!("{name}_sum"), labels, h.sum);
        self.sample(&format!("{name}_count"), labels, h.count as f64)
    }

    /// A family with a single unlabelled sample.
    fn single(&mut self, name: &str, kind: Kind, help: &str, value: f64) -> &mut Self {
        self.family(name, kind, help).sample(name, &[], value)
    }

    pub(crate) fn finish(self) -> String {
        self.out
    }
}

fn with<'a>(labels: &Labels<'a>, extra: (&'a str, &'a str)) -> Vec<(&'a str, &'a str)> {
    labels.iter().copied().chain([extra]).collect()
}

pub(crate) fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn format_value(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value.is_infinite() {
        if value > 0.0 { "+Inf" } else { "-Inf" }.to_string()
    } else {
        value.to_string()
    }
}

fn secs(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

fn flag(on: bool) -> f64 {
    f64::from(u8::from(on))
}

/// Reduce a free-text governor pause reason (see `governor::evaluate`) to a [`PAUSE_REASONS`] entry.
pub(crate) fn pause_reason(reason: &str) -> &'static str {
    if reason.starts_with("a tool call took") {
        "read_latency"
    } else if reason.starts_with("memory pressure") {
        "memory_pressure"
    } else if reason.starts_with("other processes") {
        "cpu"
    } else if reason == "on battery" {
        "battery"
    } else {
        "other"
    }
}

fn write_tool_calls(m: &mut Exposition) -> LatencyStats {
    let reads = telemetry::reads();
    let totals = reads.totals();
    m.family(
        "codesearch_tool_call_duration_seconds",
        Kind::Histogram,
        "Tool-call duration since serve started.",
    );
    for (tool, t) in &totals {
        m.histogram(
            "codesearch_tool_call_duration_seconds",
            &[("tool", tool)],
            &t.histogram,
        );
    }
    m.family(
        "codesearch_tool_call_failures_total",
        Kind::Counter,
        "Tool calls that returned an error since serve started.",
    );
    for (tool, t) in &totals {
        m.sample(
            "codesearch_tool_call_failures_total",
            &[("tool", tool)],
            t.failures as f64,
        );
    }

    let now = Instant::now();
    let summaries: Vec<_> = WINDOWS
        .iter()
        .map(|&(window, span)| (window, reads.summary_at(now, span)))
        .collect();
    let rows: Vec<(&str, &str, &LatencyStats)> = summaries
        .iter()
        .flat_map(|(window, s)| {
            std::iter::once(("all", *window, &s.overall)).chain(
                s.by_tool
                    .iter()
                    .filter(|(tool, _)| telemetry::KNOWN_TOOLS.contains(&tool.as_str()))
                    .map(|(tool, stats)| (tool.as_str(), *window, stats)),
            )
        })
        .collect();
    m.family(
        "codesearch_tool_call_latency_seconds",
        Kind::Gauge,
        "Nearest-rank tool-call latency percentile over a trailing window; tool=\"all\" is every call.",
    );
    for (tool, window, stats) in &rows {
        let percentiles = [
            ("50", stats.p50),
            ("95", stats.p95),
            ("98", stats.p98),
            ("99", stats.p99),
            ("100", stats.p100),
        ];
        for (percentile, ms) in percentiles {
            if let Some(ms) = ms {
                m.sample(
                    "codesearch_tool_call_latency_seconds",
                    &[
                        ("tool", tool),
                        ("window", window),
                        ("percentile", percentile),
                    ],
                    secs(ms),
                );
            }
        }
    }
    m.family(
        "codesearch_tool_call_window_calls",
        Kind::Gauge,
        "Tool calls finished within a trailing window.",
    );
    for (tool, window, stats) in &rows {
        m.sample(
            "codesearch_tool_call_window_calls",
            &[("tool", tool), ("window", window)],
            stats.count as f64,
        );
    }
    m.family(
        "codesearch_tool_call_window_failures",
        Kind::Gauge,
        "Failed tool calls within a trailing window.",
    );
    for (tool, window, stats) in &rows {
        m.sample(
            "codesearch_tool_call_window_failures",
            &[("tool", tool), ("window", window)],
            stats.failures as f64,
        );
    }
    summaries
        .into_iter()
        .find(|(window, _)| *window == "1h")
        .map(|(_, s)| s.overall)
        .unwrap_or_default()
}

fn write_governor(m: &mut Exposition, g: &GovernorStatus, aliases: &HashMap<&str, &str>) {
    m.single(
        "codesearch_index_governor_max_jobs",
        Kind::Gauge,
        "Index jobs allowed to run at once.",
        g.max_jobs as f64,
    );
    m.family(
        "codesearch_index_governor_jobs",
        Kind::Gauge,
        "Index jobs running or waiting for a slot.",
    )
    .sample(
        "codesearch_index_governor_jobs",
        &[("state", "running")],
        g.running.len() as f64,
    )
    .sample(
        "codesearch_index_governor_jobs",
        &[("state", "waiting")],
        g.waiting.len() as f64,
    );
    m.family(
        "codesearch_index_governor_job_seconds",
        Kind::Gauge,
        "Seconds each index job has been running or waiting; repo is the alias.",
    );
    for (state, jobs) in [("running", &g.running), ("waiting", &g.waiting)] {
        for job in jobs {
            let repo = aliases
                .get(job.label.as_str())
                .copied()
                .unwrap_or("unregistered");
            m.sample(
                "codesearch_index_governor_job_seconds",
                &[
                    ("repo", repo),
                    ("priority", job.priority.as_str()),
                    ("state", state),
                ],
                job.secs as f64,
            );
        }
    }
    m.family(
        "codesearch_index_governor_paused_jobs",
        Kind::Gauge,
        "Running index jobs paused at a yield point, by reason.",
    );
    for reason in PAUSE_REASONS {
        let paused = g
            .running
            .iter()
            .filter_map(|j| j.paused.as_deref())
            .filter(|r| pause_reason(r) == reason)
            .count();
        m.sample(
            "codesearch_index_governor_paused_jobs",
            &[("reason", reason)],
            paused as f64,
        );
    }

    let inputs = &g.inputs;
    if let Some(ms) = inputs.recent_read_max_ms {
        m.single(
            "codesearch_index_governor_recent_read_max_seconds",
            Kind::Gauge,
            "Slowest interactive tool call in the governor protect window.",
            secs(ms),
        );
    }
    if let Some(cpu) = inputs.other_cpu_percent {
        m.single(
            "codesearch_index_governor_other_cpu_ratio",
            Kind::Gauge,
            "CPU used by other processes as a fraction of all cores.",
            f64::from(cpu) / 100.0,
        );
    }
    if let Some(level) = inputs.memory_pressure {
        m.family(
            "codesearch_index_governor_memory_pressure",
            Kind::Gauge,
            "System memory pressure level; the current level is 1.",
        );
        for candidate in MemoryPressure::ALL {
            m.sample(
                "codesearch_index_governor_memory_pressure",
                &[("level", candidate.as_str())],
                flag(candidate == level),
            );
        }
    }
    if let Some(on_battery) = inputs.on_battery {
        m.single(
            "codesearch_index_governor_on_battery",
            Kind::Gauge,
            "1 when the machine is running on battery.",
            flag(on_battery),
        );
    }
}

fn write_repos(m: &mut Exposition, repos: &[RepoHealth]) {
    let count =
        |pred: &dyn Fn(&RepoHealth) -> bool| repos.iter().filter(|r| pred(r)).count() as f64;
    m.single(
        "codesearch_repos",
        Kind::Gauge,
        "Registered repos.",
        repos.len() as f64,
    );
    m.single(
        "codesearch_repos_indexing",
        Kind::Gauge,
        "Repos being indexed now.",
        count(&|r| r.status == "indexing"),
    );
    m.single(
        "codesearch_repos_failing",
        Kind::Gauge,
        "Repos whose last index job failed.",
        count(&|r| r.status == "failing"),
    );
    m.single(
        "codesearch_repos_queued",
        Kind::Gauge,
        "Repos waiting for an indexing governor slot.",
        count(&|r| r.queued),
    );

    m.family(
        "codesearch_repo_info",
        Kind::Gauge,
        "Always 1; carries the repo path and status (failing, open, warm, indexing, ...).",
    );
    for repo in repos {
        m.sample(
            "codesearch_repo_info",
            &[
                ("repo", &repo.alias),
                ("path", &repo.path),
                ("status", &repo.status),
            ],
            1.0,
        );
    }
    let per_repo: [(&str, &str, RepoValue); 6] = [
        (
            "codesearch_repo_last_indexed_timestamp_seconds",
            "Unix time of the last successful index job.",
            |r| r.outcome.indexed_at_ms.map(secs),
        ),
        (
            "codesearch_repo_last_index_duration_seconds",
            "Duration of the last finished index job, successful or not.",
            |r| r.outcome.duration_ms.map(secs),
        ),
        (
            "codesearch_repo_index_consecutive_failures",
            "Failed index jobs since the last success.",
            |r| Some(f64::from(r.outcome.failures)),
        ),
        (
            "codesearch_repo_pending_changes",
            "File changes the watcher has not indexed yet.",
            |r| Some(r.changes as f64),
        ),
        (
            "codesearch_repo_indexing",
            "1 while the repo is being indexed.",
            |r| Some(flag(r.status == "indexing")),
        ),
        (
            "codesearch_repo_queued",
            "1 while the repo waits for an indexing governor slot.",
            |r| Some(flag(r.queued)),
        ),
    ];
    for (name, help, value) in per_repo {
        m.family(name, Kind::Gauge, help);
        for repo in repos {
            if let Some(v) = value(repo) {
                m.sample(name, &[("repo", &repo.alias)], v);
            }
        }
    }
}

fn write_process(m: &mut Exposition) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
    let Ok(pid) = sysinfo::get_current_pid() else {
        return;
    };
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory().with_cpu(),
    );
    let Some(process) = system.process(pid) else {
        return;
    };
    m.single(
        "process_resident_memory_bytes",
        Kind::Gauge,
        "Resident memory size in bytes.",
        process.memory() as f64,
    );
    m.single(
        "process_cpu_seconds_total",
        Kind::Counter,
        "User and system CPU time spent in seconds.",
        secs(process.accumulated_cpu_time()),
    );
}

/// The full `/metrics` body.
pub(crate) fn render(state: &ServeState) -> String {
    let mut m = Exposition::default();
    m.family(
        "codesearch_build_info",
        Kind::Gauge,
        "Always 1; carries the codesearch version.",
    )
    .sample(
        "codesearch_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );
    m.single(
        "codesearch_uptime_seconds",
        Kind::Gauge,
        "Seconds since serve started.",
        state.started_at().elapsed().as_secs_f64(),
    );
    let slo_ms = health::slo_ms();
    m.single(
        "codesearch_slo_seconds",
        Kind::Gauge,
        "Tool-call p95 latency SLO.",
        secs(slo_ms),
    );
    m.single(
        "codesearch_read_target_seconds",
        Kind::Gauge,
        "Tool-call latency above which indexing yields.",
        secs(governor::configured().1.read_target_ms),
    );

    let latency_1h = write_tool_calls(&mut m);
    let repos = super::dashboard::repo_health(state);
    let overall = health::overall(&latency_1h, slo_ms, &repos);
    m.family(
        "codesearch_status",
        Kind::Gauge,
        "Overall health; the current status is 1. An unreachable serve is down (up == 0).",
    );
    for status in ["ok", "degraded"] {
        m.sample(
            "codesearch_status",
            &[("status", status)],
            flag(overall.status == status),
        );
    }
    m.single(
        "codesearch_degraded_reasons",
        Kind::Gauge,
        "Reasons the overall status is degraded; the text is in /api/summary.",
        overall.reasons.len() as f64,
    );

    let aliases: HashMap<&str, &str> = repos
        .iter()
        .map(|r| (r.path.as_str(), r.alias.as_str()))
        .collect();
    write_governor(&mut m, &governor::global().status(), &aliases);

    let qos = super::qos_status_json();
    m.family(
        "codesearch_qos_info",
        Kind::Gauge,
        "Always 1; carries the read-path and index-pool QoS classes.",
    )
    .sample(
        "codesearch_qos_info",
        &[
            ("read", qos["read"].as_str().unwrap_or("unset")),
            ("index", qos["index"].as_str().unwrap_or("unset")),
        ],
        1.0,
    );
    m.single(
        "codesearch_index_threads",
        Kind::Gauge,
        "Threads in the index executor pool.",
        qos["index_threads"].as_f64().unwrap_or(0.0),
    );

    write_repos(&mut m, &repos);

    m.family(
        "codesearch_index_events_total",
        Kind::Counter,
        "Index and governor events logged since serve started, by level.",
    );
    for (level, total) in health::log().event_totals() {
        m.sample(
            "codesearch_index_events_total",
            &[("level", level.as_str())],
            total as f64,
        );
    }

    write_process(&mut m);
    m.finish()
}

pub(super) async fn metrics_handler(State(state): State<Arc<ServeState>>) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, CONTENT_TYPE),
            (header::CACHE_CONTROL, "no-store"),
        ],
        render(&state),
    )
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
