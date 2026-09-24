use super::*;
use crate::db_discovery::repos::ReposConfig;
use rmcp::handler::server::wrapper::Parameters;
use std::path::PathBuf;

/// Serve state with `repos` registered at unique temp paths (no DBs, so `no_index`).
fn state_with(repos: &[&str]) -> (tempfile::TempDir, Arc<ServeState>) {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = ReposConfig::default();
    for alias in repos {
        config
            .repos
            .insert(alias.to_string(), tmp.path().join(alias));
    }
    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();
    let state = Arc::new(ServeState::new(config, Some(config_file)));
    (tmp, state)
}

fn repo_path(tmp: &tempfile::TempDir, alias: &str) -> String {
    PathBuf::from(tmp.path()).join(alias).display().to_string()
}

fn status_request(kind: &str) -> crate::mcp::types::StatusRequest {
    crate::mcp::types::StatusRequest {
        kind: Some(kind.to_string()),
        hours: None,
        bucket_minutes: None,
        query: None,
        repo_status: None,
        limit: None,
        project: None,
        group: None,
    }
}

async fn status_json(state: Arc<ServeState>, request: crate::mcp::types::StatusRequest) -> Value {
    let service = crate::mcp::CodesearchService::new_for_serve(state).unwrap();
    let result = service.status(Parameters(request)).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.clone();
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

#[tokio::test]
async fn a_failed_index_job_marks_the_repo_failing_and_degrades_the_summary() {
    let (tmp, state) = state_with(&["healthy", "broken"]);
    let broken = repo_path(&tmp, "broken");
    health::log().job_finished(
        &broken,
        Duration::from_millis(12),
        Err("tokenizer crashed".to_string()),
    );

    let repos = repo_health(&state);
    assert_eq!(repos[0].alias, "broken");
    assert_eq!(repos[0].status, "failing");
    assert_eq!(repos[0].outcome.error.as_deref(), Some("tokenizer crashed"));
    assert_eq!(repos[1].status, "no_index");

    let s = summary(Some(&state));
    assert_eq!(s["status"], "degraded");
    let reasons = s["reasons"].as_array().unwrap();
    assert!(
        reasons
            .iter()
            .any(|r| r == "1 repo(s) failing to index: broken"),
        "{reasons:?}"
    );
    assert_eq!(s["repos"]["total"], 2);
    assert_eq!(s["repos"]["failing"], 1);
    for key in [
        "latency_5m",
        "latency_1h",
        "index_governor",
        "qos",
        "slo_ms",
    ] {
        assert!(s.get(key).is_some(), "summary lacks {key}: {s}");
    }
}

#[tokio::test]
async fn repos_handler_filters_by_query_and_status() {
    let (_tmp, state) = state_with(&["api", "web"]);
    let rows = |q: Option<&str>, status: Option<&str>| {
        let state = Arc::clone(&state);
        let query = ReposQuery {
            q: q.map(str::to_string),
            status: status.map(str::to_string),
        };
        async move {
            repos_handler(State(state), Query(query))
                .await
                .0
                .into_iter()
                .map(|r| r.alias)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(rows(None, None).await, ["api", "web"]);
    assert_eq!(rows(Some("web"), None).await, ["web"]);
    assert_eq!(rows(None, Some("no_index")).await, ["api", "web"]);
    assert!(rows(None, Some("failing")).await.is_empty());
}

#[test]
fn latency_series_clamps_the_window_and_bucket() {
    let cases = [
        (None, None, 6, 10, 36),
        (Some(1), Some(2), 1, 2, 30),
        (Some(500), Some(0), 24, 1, 1440),
        (Some(0), Some(9_999), 1, 240, 1),
    ];
    for (hours, bucket, want_hours, want_bucket, want_len) in cases {
        let v = latency_series(hours, bucket);
        assert_eq!(v["hours"], want_hours, "{hours:?}");
        assert_eq!(v["bucket_minutes"], want_bucket, "{bucket:?}");
        assert_eq!(v["buckets"].as_array().unwrap().len(), want_len);
    }
}

#[test]
fn events_are_clamped_to_the_log_size() {
    health::log().event(health::EventLevel::Info, "dashboard-test event", None);
    assert_eq!(events(Some(0)).len(), 1);
    assert!(events(Some(usize::MAX)).len() <= health::MAX_EVENTS);
}

#[tokio::test]
async fn dashboard_page_uses_every_api_route() {
    let body = dashboard_handler().await.into_response().into_body();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let html = String::from_utf8(bytes.to_vec()).unwrap();
    for route in [
        crate::constants::API_SUMMARY_PATH,
        crate::constants::API_LATENCY_PATH,
        crate::constants::API_REPOS_PATH,
        crate::constants::API_EVENTS_PATH,
    ] {
        assert!(html.contains(route), "dashboard never calls {route}");
    }
}

#[tokio::test]
async fn mcp_status_exposes_health_latency_repos_and_events() {
    let (tmp, state) = state_with(&["svc"]);
    let svc = repo_path(&tmp, "svc");
    health::log().job_finished(&svc, Duration::from_millis(3), Ok(()));

    let health = status_json(Arc::clone(&state), status_request("health")).await;
    assert!(health["status"].is_string(), "{health}");
    assert_eq!(health["repos"]["total"], 1);

    let mut latency = status_request("latency");
    latency.hours = Some(2);
    latency.bucket_minutes = Some(30);
    let latency = status_json(Arc::clone(&state), latency).await;
    assert_eq!(latency["buckets"].as_array().unwrap().len(), 4);

    let mut repos = status_request("repos");
    repos.query = Some("svc".to_string());
    let repos = status_json(Arc::clone(&state), repos).await;
    assert_eq!(repos[0]["alias"], "svc");
    assert!(repos[0]["indexed_at_ms"].is_u64(), "{repos}");

    let mut events = status_request("events");
    events.limit = Some(500);
    let events = status_json(Arc::clone(&state), events).await;
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["repo"] == svc.as_str() && e["msg"] == "index job finished"),
        "{events}"
    );
}
