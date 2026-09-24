//! In-process read-path latency: every tool call is recorded so `/status` can
//! report p50..p100 against the SLO and the indexing governor can yield to reads.

use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Samples older than this are dropped; also the longest dashboard window.
pub const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// Window for the headline percentiles in `/status` and `status(kind="health")`.
pub const SUMMARY_WINDOW: Duration = Duration::from_secs(60 * 60);
const MAX_SAMPLES: usize = 100_000;
const PERCENTILES: [u8; 5] = [50, 95, 98, 99, 100];

/// Grep-style tools whose latency the indexing governor protects. `find_impact`
/// (SCIP analysis, up to its budget) and `status` are recorded but not gated.
pub const INTERACTIVE_TOOLS: [&str; 4] = ["search", "find", "explore", "get_chunk"];

#[derive(Debug, Clone)]
struct Sample {
    at: Instant,
    tool: String,
    ms: u64,
    ok: bool,
}

/// Nearest-rank percentiles over a set of tool calls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LatencyStats {
    pub count: usize,
    pub failures: usize,
    pub p50: Option<u64>,
    pub p95: Option<u64>,
    pub p98: Option<u64>,
    pub p99: Option<u64>,
    pub p100: Option<u64>,
}

impl LatencyStats {
    fn from_samples<'a>(samples: impl Iterator<Item = &'a Sample>) -> Self {
        let mut ms = Vec::new();
        let mut failures = 0;
        for sample in samples {
            ms.push(sample.ms);
            failures += usize::from(!sample.ok);
        }
        ms.sort_unstable();
        let rank =
            |p: u8| (!ms.is_empty()).then(|| ms[(ms.len() * p as usize).div_ceil(100).max(1) - 1]);
        let [p50, p95, p98, p99, p100] = PERCENTILES.map(rank);
        Self {
            count: ms.len(),
            failures,
            p50,
            p95,
            p98,
            p99,
            p100,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencySummary {
    pub window_secs: u64,
    pub overall: LatencyStats,
    pub by_tool: BTreeMap<String, LatencyStats>,
}

/// One time bucket of a latency series; `start_ms` is Unix epoch milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatencyBucket {
    pub start_ms: u64,
    #[serde(flatten)]
    pub stats: LatencyStats,
}

/// Current wall clock as Unix epoch milliseconds.
pub fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Default)]
pub struct LatencyRecorder {
    samples: Mutex<VecDeque<Sample>>,
}

impl LatencyRecorder {
    pub fn record_at(&self, at: Instant, tool: &str, elapsed: Duration, ok: bool) {
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        while samples
            .front()
            .is_some_and(|s| at.saturating_duration_since(s.at) > RETENTION)
            || samples.len() >= MAX_SAMPLES
        {
            samples.pop_front();
        }
        samples.push_back(Sample {
            at,
            tool: tool.to_string(),
            ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            ok,
        });
    }

    pub fn summary_at(&self, now: Instant, window: Duration) -> LatencySummary {
        let samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        let recent: Vec<&Sample> = samples
            .iter()
            .filter(|s| now.saturating_duration_since(s.at) <= window)
            .collect();
        let mut tools: BTreeMap<&str, Vec<&Sample>> = BTreeMap::new();
        for sample in &recent {
            tools.entry(sample.tool.as_str()).or_default().push(sample);
        }
        LatencySummary {
            window_secs: window.as_secs(),
            overall: LatencyStats::from_samples(recent.iter().copied()),
            by_tool: tools
                .into_iter()
                .map(|(tool, s)| (tool.to_string(), LatencyStats::from_samples(s.into_iter())))
                .collect(),
        }
    }

    /// `window` split into `bucket`-wide slices ending at `now`, oldest first.
    /// `now_unix_ms` is the wall clock at `now`, used to label bucket starts.
    pub fn series_at(
        &self,
        now: Instant,
        now_unix_ms: u64,
        window: Duration,
        bucket: Duration,
    ) -> Vec<LatencyBucket> {
        let bucket_ms = u64::try_from(bucket.as_millis()).unwrap_or(u64::MAX).max(1);
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        let count = usize::try_from(window_ms.div_ceil(bucket_ms)).unwrap_or(usize::MAX);
        let span_ms = bucket_ms.saturating_mul(count as u64);
        let mut slices: Vec<Vec<&Sample>> = vec![Vec::new(); count];
        let samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        for sample in samples.iter() {
            let age_ms = u64::try_from(now.saturating_duration_since(sample.at).as_millis())
                .unwrap_or(u64::MAX);
            if age_ms < span_ms {
                slices[count - 1 - usize::try_from(age_ms / bucket_ms).unwrap_or(0)].push(sample);
            }
        }
        let origin = now_unix_ms.saturating_sub(span_ms);
        slices
            .into_iter()
            .enumerate()
            .map(|(i, slice)| LatencyBucket {
                start_ms: origin + bucket_ms * i as u64,
                stats: LatencyStats::from_samples(slice.into_iter()),
            })
            .collect()
    }

    /// Slowest call to one of `tools` finishing within `within` of `now`, if any.
    pub fn recent_max_at(&self, now: Instant, within: Duration, tools: &[&str]) -> Option<u64> {
        let samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        samples
            .iter()
            .rev()
            .take_while(|s| now.saturating_duration_since(s.at) <= within)
            .filter(|s| tools.contains(&s.tool.as_str()))
            .map(|s| s.ms)
            .max()
    }
}

/// The process-wide recorder for tool calls.
pub fn reads() -> &'static LatencyRecorder {
    static READS: OnceLock<LatencyRecorder> = OnceLock::new();
    READS.get_or_init(LatencyRecorder::default)
}

/// Record one finished tool call.
pub fn record_tool_call(tool: &str, elapsed: Duration, ok: bool) {
    reads().record_at(Instant::now(), tool, elapsed, ok);
}

/// Await `call`, recording its latency under `tool`.
pub async fn timed<T, E>(
    tool: &str,
    call: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let started = Instant::now();
    let result = call.await;
    record_tool_call(tool, started.elapsed(), result.is_ok());
    result
}

#[cfg(test)]
#[path = "telemetry_tests.rs"]
mod tests;
