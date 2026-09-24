use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn summary_reports_nearest_rank_percentiles() {
    let recorder = LatencyRecorder::default();
    let now = Instant::now();
    for i in 1..=100 {
        recorder.record_at(now, "search", ms(i * 10), i != 1);
    }

    let s = recorder.summary_at(now, RETENTION).overall;
    assert_eq!(
        s,
        LatencyStats {
            count: 100,
            failures: 1,
            p50: Some(500),
            p95: Some(950),
            p98: Some(980),
            p99: Some(990),
            p100: Some(1000),
        }
    );
}

#[test]
fn summary_splits_by_tool_and_honours_the_window() {
    let recorder = LatencyRecorder::default();
    let start = Instant::now();
    recorder.record_at(start, "search", ms(15_000), true);
    let later = start + Duration::from_secs(120);
    recorder.record_at(later, "search", ms(40), true);
    recorder.record_at(later, "get_chunk", ms(5), true);

    let last_minute = recorder.summary_at(later, Duration::from_secs(60));
    assert_eq!(last_minute.overall.count, 2);
    assert_eq!(last_minute.by_tool["search"].p100, Some(40));
    assert_eq!(last_minute.by_tool["get_chunk"].count, 1);

    let all = recorder.summary_at(later, RETENTION);
    assert_eq!(all.by_tool["search"].p100, Some(15_000));
}

#[test]
fn an_empty_window_has_no_percentiles() {
    let s = LatencyRecorder::default().summary_at(Instant::now(), RETENTION);
    assert_eq!(s.overall, LatencyStats::default());
    assert!(s.by_tool.is_empty());
}

#[test]
fn samples_older_than_retention_are_dropped() {
    let recorder = LatencyRecorder::default();
    let start = Instant::now();
    recorder.record_at(start, "search", ms(9_000), true);
    let later = start + RETENTION + Duration::from_secs(1);
    recorder.record_at(later, "search", ms(10), true);

    let s = recorder.summary_at(later, RETENTION * 2).overall;
    assert_eq!((s.count, s.p100), (1, Some(10)));
}

#[test]
fn recent_max_sees_only_interactive_calls_inside_the_window() {
    let recorder = LatencyRecorder::default();
    let start = Instant::now();
    recorder.record_at(start, "search", ms(4_000), true);
    let later = start + Duration::from_secs(45);
    recorder.record_at(later, "search", ms(80), true);
    recorder.record_at(later, "find_impact", ms(30_000), true);

    let tools = &INTERACTIVE_TOOLS;
    assert_eq!(
        recorder.recent_max_at(later, Duration::from_secs(30), tools),
        Some(80)
    );
    assert_eq!(
        recorder.recent_max_at(later, Duration::from_secs(60), tools),
        Some(4_000)
    );
    assert_eq!(
        LatencyRecorder::default().recent_max_at(later, Duration::from_secs(30), tools),
        None
    );
}

#[tokio::test]
async fn timed_records_failures() {
    let before = reads()
        .summary_at(Instant::now(), RETENTION)
        .by_tool
        .get("timed-test")
        .map_or(0, |s| s.failures);
    let _ = timed("timed-test", async { Err::<(), _>("boom") }).await;
    let after = reads().summary_at(Instant::now(), RETENTION).by_tool["timed-test"].failures;
    assert_eq!(after, before + 1);
}
