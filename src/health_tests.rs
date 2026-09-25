use super::*;
use crate::testing::EnvRestore;
use serial_test::serial;

fn input<'a>(alias: &'a str, path: &'a str, state: &'a str) -> RepoInput<'a> {
    RepoInput {
        alias,
        path,
        state,
        changes: 0,
    }
}

fn failed(error: &str) -> IndexOutcome {
    IndexOutcome {
        failures: 1,
        error: Some(error.to_string()),
        ..IndexOutcome::default()
    }
}

#[test]
fn job_finished_tracks_consecutive_failures_and_resets_on_success() {
    let log = HealthLog::default();
    log.job_finished("/r", Duration::from_millis(5), Err("disk full".into()));
    log.job_finished("/r", Duration::from_millis(7), Err("disk full".into()));
    let outcome = &log.outcomes()["/r"];
    assert_eq!(outcome.failures, 2);
    assert_eq!(outcome.error.as_deref(), Some("disk full"));
    assert_eq!(outcome.duration_ms, Some(7));
    assert_eq!(outcome.indexed_at_ms, None);

    log.job_finished("/r", Duration::from_millis(9), Ok(()));
    let outcome = &log.outcomes()["/r"];
    assert_eq!((outcome.failures, outcome.error.as_deref()), (0, None));
    assert!(outcome.indexed_at_ms.is_some());

    let events = log.recent(10);
    let levels: Vec<EventLevel> = events.iter().map(|e| e.level).collect();
    assert_eq!(
        levels,
        [EventLevel::Info, EventLevel::Error, EventLevel::Error]
    );
    assert_eq!(events[1].msg, "index job failed: disk full");
    assert_eq!(events[0].repo.as_deref(), Some("/r"));
}

#[test]
fn events_are_bounded_and_newest_first() {
    let log = HealthLog::default();
    for i in 0..MAX_EVENTS + 5 {
        log.event(EventLevel::Info, format!("e{i}"), None);
    }
    let all = log.recent(usize::MAX);
    assert_eq!(all.len(), MAX_EVENTS);
    assert_eq!(all[0].msg, format!("e{}", MAX_EVENTS + 4));
    assert_eq!(all.last().unwrap().msg, "e5");
    assert_eq!(log.recent(2).len(), 2);
}

#[test]
fn repo_rows_merge_outcomes_and_queue_and_rank_problems_first() {
    let repos = [
        input("zeta", "/z", "open"),
        input("alpha", "/a", "closed"),
        input("beta", "/b", "indexing"),
        input("gamma", "/g", "open"),
        input("delta", "/d", "warm"),
    ];
    let outcomes = HashMap::from([("/g".to_string(), failed("boom"))]);
    let waiting = vec!["/d".to_string()];

    let rows = repo_rows(&repos, &outcomes, &waiting);

    let order: Vec<(&str, &str, bool)> = rows
        .iter()
        .map(|r| (r.alias.as_str(), r.status.as_str(), r.queued))
        .collect();
    assert_eq!(
        order,
        [
            ("gamma", "failing", false),
            ("beta", "indexing", false),
            ("delta", "warm", true),
            ("alpha", "closed", false),
            ("zeta", "open", false),
        ]
    );
    assert_eq!(rows[0].outcome.error.as_deref(), Some("boom"));
}

#[test]
fn filter_repos_by_query_and_status() {
    let rows = repo_rows(
        &[
            input("api", "/src/api", "open"),
            input("api-v2", "/src/api-v2", "closed"),
            input("web", "/src/web", "open"),
        ],
        &HashMap::new(),
        &["/src/web".to_string()],
    );
    let aliases = |rows: Vec<RepoHealth>| rows.into_iter().map(|r| r.alias).collect::<Vec<_>>();

    let cases: [(Option<&str>, Option<&str>, &[&str]); 6] = [
        (Some("api"), None, &["api"]),
        (Some("src/api"), None, &["api", "api-v2"]),
        (None, Some("open"), &["web", "api"]),
        (None, Some("queued"), &["web"]),
        (Some("src"), Some("closed"), &["api-v2"]),
        (Some(""), Some(""), &["web", "api", "api-v2"]),
    ];
    for (query, status, want) in cases {
        assert_eq!(
            aliases(filter_repos(rows.clone(), query, status)),
            want,
            "query={query:?} status={status:?}"
        );
    }
}

#[test]
fn overall_is_ok_without_problems_and_lists_each_problem() {
    let quiet = LatencyStats {
        count: 10,
        p95: Some(200),
        ..LatencyStats::default()
    };
    assert_eq!(
        overall(&quiet, 5_000, &[]),
        Overall {
            status: "ok",
            reasons: vec![]
        }
    );

    let slow = LatencyStats {
        count: 10,
        failures: 2,
        p95: Some(9_000),
        ..LatencyStats::default()
    };
    let repos = repo_rows(
        &[input("web", "/w", "open")],
        &HashMap::from([("/w".to_string(), failed("x"))]),
        &[],
    );
    let result = overall(&slow, 5_000, &repos);
    assert_eq!(result.status, "degraded");
    assert_eq!(
        result.reasons,
        [
            "tool-call p95 is 9000ms over the last hour (SLO 5000ms)",
            "2 of 10 tool call(s) failed in the last hour",
            "1 repo(s) failing to index: web",
        ]
    );
}

#[test]
#[serial]
fn slo_reads_the_env_and_ignores_invalid_values() {
    let key = crate::constants::SLO_MS_ENV;
    {
        let _env = EnvRestore::remove(&[key]);
        assert_eq!(slo_ms(), DEFAULT_SLO_MS);
    }
    let cases = [
        ("2500", 2_500),
        ("0", DEFAULT_SLO_MS),
        ("fast", DEFAULT_SLO_MS),
    ];
    for (value, want) in cases {
        let _env = EnvRestore::set(&[(key, value)]);
        assert_eq!(slo_ms(), want, "{value:?}");
    }
}

#[test]
fn event_totals_count_every_level_past_the_ring_buffer() {
    let log = HealthLog::default();
    for _ in 0..MAX_EVENTS + 5 {
        log.event(EventLevel::Info, "tick", None);
    }
    log.job_finished("/r", Duration::from_millis(1), Err("boom".to_string()));

    assert_eq!(
        log.event_totals(),
        [
            (EventLevel::Info, MAX_EVENTS as u64 + 5),
            (EventLevel::Warn, 0),
            (EventLevel::Error, 1),
        ]
    );
    assert_eq!(EventLevel::Warn.as_str(), "warn");
}
