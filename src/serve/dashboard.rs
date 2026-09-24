//! Health dashboard (`GET /dashboard`) and its read-only JSON API:
//! `/api/summary`, `/api/latency`, `/api/repos` and `/api/events`.
//! The same builders back the MCP `status` kinds `health`, `latency`, `repos` and `events`.

use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::ServeState;
use crate::health::{self, Event, RepoHealth, RepoInput};
use crate::telemetry::{self, unix_ms_now};

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

pub(crate) const DEFAULT_HOURS: u64 = 6;
pub(crate) const MAX_HOURS: u64 = 24;
pub(crate) const DEFAULT_BUCKET_MINUTES: u64 = 10;
pub(crate) const MAX_BUCKET_MINUTES: u64 = 240;
pub(crate) const DEFAULT_EVENT_LIMIT: usize = 50;

/// Every registered repo with its serve state, last index outcome and queue position.
pub(crate) fn repo_health(state: &ServeState) -> Vec<RepoHealth> {
    let config = state.config_snapshot();
    let statuses = state.repo_statuses_lightweight();
    let paths: Vec<(String, String)> = statuses
        .iter()
        .map(|(alias, _)| {
            let path = config
                .repos
                .get(alias)
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            (alias.clone(), path)
        })
        .collect();
    let inputs: Vec<RepoInput<'_>> = statuses
        .iter()
        .zip(&paths)
        .map(|((_, info), (alias, path))| RepoInput {
            alias,
            path,
            state: info.status.as_str(),
            changes: info.changes,
        })
        .collect();
    let waiting: Vec<String> = crate::index::governor::global()
        .status()
        .waiting
        .into_iter()
        .map(|job| job.label)
        .collect();
    health::repo_rows(&inputs, &health::log().outcomes(), &waiting)
}

/// Overall status with reasons, tool-call latency, governor, QoS and repo counts.
pub(crate) fn summary(state: Option<&ServeState>) -> Value {
    let reads = telemetry::reads();
    let now = Instant::now();
    let latency_1h = reads.summary_at(now, telemetry::SUMMARY_WINDOW);
    let repos = state.map(repo_health).unwrap_or_default();
    let slo_ms = health::slo_ms();
    let overall = health::overall(&latency_1h.overall, slo_ms, &repos);
    let count = |status: &str| repos.iter().filter(|r| r.status == status).count();
    json!({
        "generated_at_ms": unix_ms_now(),
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.map(|s| s.started_at().elapsed().as_secs()),
        "status": overall.status,
        "reasons": overall.reasons,
        "slo_ms": slo_ms,
        "read_target_ms": crate::index::governor::configured().1.read_target_ms,
        "latency_5m": reads.summary_at(now, Duration::from_secs(300)),
        "latency_1h": latency_1h,
        "index_governor": crate::index::governor::global().status(),
        "qos": super::qos_status_json(),
        "repos": {
            "total": repos.len(),
            "indexing": count("indexing"),
            "failing": count("failing"),
            "queued": repos.iter().filter(|r| r.queued).count(),
        },
    })
}

/// Tool-call latency over the last `hours`, in `bucket_minutes` buckets (both clamped).
pub(crate) fn latency_series(hours: Option<u64>, bucket_minutes: Option<u64>) -> Value {
    let hours = hours.unwrap_or(DEFAULT_HOURS).clamp(1, MAX_HOURS);
    let bucket_minutes = bucket_minutes
        .unwrap_or(DEFAULT_BUCKET_MINUTES)
        .clamp(1, MAX_BUCKET_MINUTES);
    let buckets = telemetry::reads().series_at(
        Instant::now(),
        unix_ms_now(),
        Duration::from_secs(hours * 3600),
        Duration::from_secs(bucket_minutes * 60),
    );
    json!({
        "slo_ms": health::slo_ms(),
        "read_target_ms": crate::index::governor::configured().1.read_target_ms,
        "hours": hours,
        "bucket_minutes": bucket_minutes,
        "buckets": buckets,
    })
}

/// Newest index and governor events first, `limit` clamped to the log size.
pub(crate) fn events(limit: Option<usize>) -> Vec<Event> {
    health::log().recent(
        limit
            .unwrap_or(DEFAULT_EVENT_LIMIT)
            .clamp(1, health::MAX_EVENTS),
    )
}

pub(super) async fn dashboard_handler() -> impl IntoResponse {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Html(DASHBOARD_HTML),
    )
}

pub(super) async fn summary_handler(State(state): State<Arc<ServeState>>) -> Json<Value> {
    Json(summary(Some(&state)))
}

#[derive(Deserialize)]
pub(super) struct LatencyQuery {
    hours: Option<u64>,
    bucket_minutes: Option<u64>,
}

pub(super) async fn latency_handler(Query(q): Query<LatencyQuery>) -> Json<Value> {
    Json(latency_series(q.hours, q.bucket_minutes))
}

#[derive(Deserialize)]
pub(super) struct ReposQuery {
    q: Option<String>,
    status: Option<String>,
}

pub(super) async fn repos_handler(
    State(state): State<Arc<ServeState>>,
    Query(q): Query<ReposQuery>,
) -> Json<Vec<RepoHealth>> {
    Json(health::filter_repos(
        repo_health(&state),
        q.q.as_deref(),
        q.status.as_deref(),
    ))
}

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    limit: Option<usize>,
}

pub(super) async fn events_handler(Query(q): Query<EventsQuery>) -> Json<Vec<Event>> {
    Json(events(q.limit))
}

#[cfg(test)]
#[path = "dashboard_tests.rs"]
mod tests;
