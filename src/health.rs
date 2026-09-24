//! Serve health: a bounded in-process event log, per-repo index outcomes, and
//! the rules that turn latency, repo and governor state into ok / degraded.
//! Feeds the dashboard, the `/api/*` routes and `status(kind="health")`.

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::telemetry::{unix_ms_now, LatencyStats};

/// Newest events kept in memory.
pub const MAX_EVENTS: usize = 500;

/// Default search latency SLO (ms) the dashboard and health status judge p95 against.
pub const DEFAULT_SLO_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub ts_ms: u64,
    pub level: EventLevel,
    pub msg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Last index result for one repo, keyed by the governor job label (repo path).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IndexOutcome {
    /// Unix ms of the last successful index job.
    pub indexed_at_ms: Option<u64>,
    /// Duration of the last finished job, successful or not.
    pub duration_ms: Option<u64>,
    /// Consecutive failed jobs; reset by a success.
    pub failures: u32,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct HealthLog {
    events: Mutex<VecDeque<Event>>,
    outcomes: Mutex<HashMap<String, IndexOutcome>>,
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl HealthLog {
    pub fn event(&self, level: EventLevel, msg: impl Into<String>, repo: Option<&str>) {
        self.push(Event {
            ts_ms: unix_ms_now(),
            level,
            msg: msg.into(),
            repo: repo.map(str::to_string),
            duration_ms: None,
        });
    }

    fn push(&self, event: Event) {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        if events.len() >= MAX_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// Record a finished index job for `repo` and log it.
    pub fn job_finished(&self, repo: &str, elapsed: Duration, result: Result<(), String>) {
        let duration_ms = millis(elapsed);
        let (level, msg) = {
            let mut outcomes = self.outcomes.lock().unwrap_or_else(|e| e.into_inner());
            let outcome = outcomes.entry(repo.to_string()).or_default();
            outcome.duration_ms = Some(duration_ms);
            match result {
                Ok(()) => {
                    outcome.indexed_at_ms = Some(unix_ms_now());
                    outcome.failures = 0;
                    outcome.error = None;
                    (EventLevel::Info, "index job finished".to_string())
                }
                Err(error) => {
                    outcome.failures += 1;
                    let msg = format!("index job failed: {error}");
                    outcome.error = Some(error);
                    (EventLevel::Error, msg)
                }
            }
        };
        self.push(Event {
            ts_ms: unix_ms_now(),
            level,
            msg,
            repo: Some(repo.to_string()),
            duration_ms: Some(duration_ms),
        });
    }

    /// Newest first, at most `limit`.
    pub fn recent(&self, limit: usize) -> Vec<Event> {
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.iter().rev().take(limit).cloned().collect()
    }

    pub fn outcomes(&self) -> HashMap<String, IndexOutcome> {
        self.outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The process-wide health log.
pub fn log() -> &'static HealthLog {
    static LOG: OnceLock<HealthLog> = OnceLock::new();
    LOG.get_or_init(HealthLog::default)
}

/// Search latency SLO from `CODESEARCH_SLO_MS`, else [`DEFAULT_SLO_MS`].
pub fn slo_ms() -> u64 {
    std::env::var(crate::constants::SLO_MS_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(DEFAULT_SLO_MS)
}

/// One registered repo as the dashboard and `status(kind="repos")` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoHealth {
    pub alias: String,
    pub path: String,
    /// `failing` when the last index job failed, else the serve state
    /// (`open`, `warm`, `readonly`, `closed`, `indexing`, `error`, `no_index`).
    pub status: String,
    /// Pending file changes the watcher has not indexed yet.
    pub changes: u64,
    /// Waiting for an indexing governor slot.
    pub queued: bool,
    #[serde(flatten)]
    pub outcome: IndexOutcome,
}

/// Serve's view of one repo, the input to [`repo_rows`].
pub struct RepoInput<'a> {
    pub alias: &'a str,
    pub path: &'a str,
    pub state: &'a str,
    pub changes: u64,
}

/// Merge serve state, index outcomes and the governor queue; failing repos first,
/// then indexing, then queued, then by alias.
pub fn repo_rows(
    repos: &[RepoInput<'_>],
    outcomes: &HashMap<String, IndexOutcome>,
    waiting: &[String],
) -> Vec<RepoHealth> {
    let mut rows: Vec<RepoHealth> = repos
        .iter()
        .map(|r| {
            let outcome = outcomes.get(r.path).cloned().unwrap_or_default();
            RepoHealth {
                alias: r.alias.to_string(),
                path: r.path.to_string(),
                status: if outcome.failures > 0 {
                    "failing".to_string()
                } else {
                    r.state.to_string()
                },
                changes: r.changes,
                queued: waiting.iter().any(|label| label == r.path),
                outcome,
            }
        })
        .collect();
    let rank = |r: &RepoHealth| match r.status.as_str() {
        "failing" => 0,
        "indexing" => 1,
        _ if r.queued => 2,
        _ => 3,
    };
    rows.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.alias.cmp(&b.alias)));
    rows
}

/// Exact alias match if any, else repos whose path contains `query`;
/// then keep only `status` (`queued` selects queued repos).
pub fn filter_repos(
    rows: Vec<RepoHealth>,
    query: Option<&str>,
    status: Option<&str>,
) -> Vec<RepoHealth> {
    let rows = match query.filter(|q| !q.is_empty()) {
        Some(q) if rows.iter().any(|r| r.alias == q) => {
            rows.into_iter().filter(|r| r.alias == q).collect()
        }
        Some(q) => rows.into_iter().filter(|r| r.path.contains(q)).collect(),
        None => rows,
    };
    match status.filter(|s| !s.is_empty()) {
        Some("queued") => rows.into_iter().filter(|r| r.queued).collect(),
        Some(s) => rows.into_iter().filter(|r| r.status == s).collect(),
        None => rows,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Overall {
    /// `ok` or `degraded`. A client that cannot reach serve reports `down` itself.
    pub status: &'static str,
    pub reasons: Vec<String>,
}

/// Judge the last hour of tool calls and the repo list against the SLO.
pub fn overall(latency: &LatencyStats, slo_ms: u64, repos: &[RepoHealth]) -> Overall {
    let mut reasons = Vec::new();
    if let Some(p95) = latency.p95.filter(|&ms| ms > slo_ms) {
        reasons.push(format!(
            "tool-call p95 is {p95}ms over the last hour (SLO {slo_ms}ms)"
        ));
    }
    if latency.failures > 0 {
        reasons.push(format!(
            "{} of {} tool call(s) failed in the last hour",
            latency.failures, latency.count
        ));
    }
    let failing: Vec<&str> = repos
        .iter()
        .filter(|r| r.status == "failing")
        .map(|r| r.alias.as_str())
        .collect();
    if !failing.is_empty() {
        reasons.push(format!(
            "{} repo(s) failing to index: {}",
            failing.len(),
            failing.join(", ")
        ));
    }
    Overall {
        status: if reasons.is_empty() { "ok" } else { "degraded" },
        reasons,
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
