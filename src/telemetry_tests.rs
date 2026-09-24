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

    let s = recorder.summary_at(now, SUMMARY_WINDOW).overall;
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

    let all = recorder.summary_at(later, SUMMARY_WINDOW);
    assert_eq!(all.by_tool["search"].p100, Some(15_000));
}

#[test]
fn an_empty_window_has_no_percentiles() {
    let s = LatencyRecorder::default().summary_at(Instant::now(), SUMMARY_WINDOW);
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
        .summary_at(Instant::now(), SUMMARY_WINDOW)
        .by_tool
        .get("timed-test")
        .map_or(0, |s| s.failures);
    let _ = timed("timed-test", async { Err::<(), _>("boom") }).await;
    let after = reads().summary_at(Instant::now(), SUMMARY_WINDOW).by_tool["timed-test"].failures;
    assert_eq!(after, before + 1);
}

#[test]
fn series_buckets_samples_by_age_oldest_first() {
    let recorder = LatencyRecorder::default();
    let now = Instant::now() + Duration::from_secs(3600);
    recorder.record_at(
        now - Duration::from_secs(25 * 60),
        "search",
        ms(9_000),
        false,
    );
    recorder.record_at(now - Duration::from_secs(5 * 60), "search", ms(40), true);
    recorder.record_at(now - Duration::from_secs(4 * 60), "find", ms(60), true);
    recorder.record_at(now - Duration::from_secs(45 * 60), "search", ms(1), true);

    let now_unix_ms = 10_000_000;
    let series = recorder.series_at(
        now,
        now_unix_ms,
        Duration::from_secs(30 * 60),
        Duration::from_secs(10 * 60),
    );

    let starts: Vec<u64> = series.iter().map(|b| b.start_ms).collect();
    assert_eq!(starts, [8_200_000, 8_800_000, 9_400_000]);
    let counts: Vec<(usize, usize, Option<u64>)> = series
        .iter()
        .map(|b| (b.stats.count, b.stats.failures, b.stats.p100))
        .collect();
    assert_eq!(
        counts,
        [(1, 1, Some(9_000)), (0, 0, None), (2, 0, Some(60))]
    );
}

#[test]
fn series_serializes_stats_flat_beside_the_start() {
    let bucket = LatencyBucket {
        start_ms: 7,
        stats: LatencyStats {
            count: 1,
            p50: Some(3),
            ..LatencyStats::default()
        },
    };
    let json = serde_json::to_value(bucket).unwrap();
    assert_eq!(json["start_ms"], 7);
    assert_eq!(json["p50"], 3);
    assert_eq!(json["count"], 1);
}
