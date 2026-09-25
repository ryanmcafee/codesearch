//! Indexing admission control: at most `max_jobs` index jobs run at once,
//! higher-priority work goes first, and running jobs pause at yield points
//! while tool calls are slow, memory is under pressure, or other work needs
//! the CPU. Every wait is capped so indexing can never starve.

use serde::Serialize;
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::health::EventLevel;

/// Who asked for the work; higher runs first and yields later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexPriority {
    /// Startup warmup, idle refreshes, lazy opens.
    Background,
    /// File-watcher batches and branch switches.
    Watcher,
    /// A user or API asked for it (`POST /repos`, reindex, TUI).
    Explicit,
}

impl IndexPriority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Watcher => "watcher",
            Self::Explicit => "explicit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    Normal,
    Warn,
    Critical,
}

impl MemoryPressure {
    pub const ALL: [Self; 3] = [Self::Normal, Self::Warn, Self::Critical];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }
}

/// System and read-path signals sampled for a gate decision.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GateInputs {
    /// Slowest tool call in the protect window.
    pub recent_read_max_ms: Option<u64>,
    pub memory_pressure: Option<MemoryPressure>,
    /// CPU used by other processes, as a percent of all cores.
    pub other_cpu_percent: Option<f32>,
    pub on_battery: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GateLimits {
    pub read_target_ms: u64,
    pub max_other_cpu_percent: f32,
    pub pause_on_battery: bool,
}

/// `None` = go; `Some(reason)` = wait.
pub fn evaluate(
    inputs: &GateInputs,
    limits: &GateLimits,
    priority: IndexPriority,
) -> Option<String> {
    if let Some(ms) = inputs
        .recent_read_max_ms
        .filter(|&ms| ms > limits.read_target_ms)
    {
        return Some(format!(
            "a tool call took {ms}ms (target {}ms)",
            limits.read_target_ms
        ));
    }
    match inputs.memory_pressure {
        Some(MemoryPressure::Critical) => return Some("memory pressure critical".to_string()),
        Some(MemoryPressure::Warn) if priority < IndexPriority::Explicit => {
            return Some("memory pressure warn".to_string())
        }
        _ => {}
    }
    if priority == IndexPriority::Background {
        if let Some(cpu) = inputs
            .other_cpu_percent
            .filter(|&cpu| cpu > limits.max_other_cpu_percent)
        {
            return Some(format!(
                "other processes using {cpu:.0}% CPU (limit {:.0}%)",
                limits.max_other_cpu_percent
            ));
        }
        if limits.pause_on_battery && inputs.on_battery == Some(true) {
            return Some("on battery".to_string());
        }
    }
    None
}

/// Longest a job waits at one yield point (or for admission) before proceeding anyway.
#[derive(Debug, Clone, Copy)]
pub struct MaxWaits {
    pub explicit: Duration,
    pub watcher: Duration,
    pub background: Duration,
}

impl Default for MaxWaits {
    fn default() -> Self {
        Self {
            explicit: Duration::from_secs(30),
            watcher: Duration::from_secs(120),
            background: Duration::from_secs(600),
        }
    }
}

impl MaxWaits {
    fn of(&self, priority: IndexPriority) -> Duration {
        match priority {
            IndexPriority::Explicit => self.explicit,
            IndexPriority::Watcher => self.watcher,
            IndexPriority::Background => self.background,
        }
    }
}

type InputSource = Box<dyn Fn(bool) -> GateInputs + Send + Sync>;

#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub label: String,
    pub priority: IndexPriority,
    pub secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GovernorStatus {
    pub max_jobs: usize,
    pub running: Vec<JobView>,
    pub waiting: Vec<JobView>,
    pub limits: GateLimits,
    pub inputs: GateInputs,
}

struct Job {
    id: u64,
    label: String,
    priority: IndexPriority,
    since: Instant,
    paused: Option<String>,
    paused_since: Option<Instant>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    running: Vec<Job>,
    waiting: Vec<Job>,
}

impl State {
    /// Waiter `id` may start: a slot is free, its repo is not already being
    /// indexed, and no startable waiter outranks it.
    fn may_start(&self, id: u64, max_jobs: usize) -> bool {
        if self.running.len() >= max_jobs {
            return false;
        }
        let busy = |job: &Job| self.running.iter().any(|r| r.label == job.label);
        let best = self
            .waiting
            .iter()
            .filter(|job| !busy(job))
            .max_by(|a, b| a.priority.cmp(&b.priority).then(b.since.cmp(&a.since)));
        best.is_some_and(|job| job.id == id)
    }
}

pub struct IndexGovernor {
    max_jobs: usize,
    limits: GateLimits,
    waits: MaxWaits,
    state: Mutex<State>,
    changed: Notify,
    inputs: InputSource,
}

tokio::task_local! {
    static PRIORITY: IndexPriority;
    static JOB_ID: u64;
}

/// Run `fut` with `priority` for every governed job it starts.
pub async fn with_priority<F: Future>(priority: IndexPriority, fut: F) -> F::Output {
    PRIORITY.scope(priority, fut).await
}

fn current_priority() -> IndexPriority {
    PRIORITY
        .try_with(|p| *p)
        .unwrap_or(IndexPriority::Background)
}

impl IndexGovernor {
    /// Governor fed by live system signals and tool-call latency.
    pub fn new(max_jobs: usize, limits: GateLimits) -> Self {
        let sampler = Mutex::new(crate::index::system_signals::Sampler::default());
        Self::with_inputs(
            max_jobs,
            limits,
            MaxWaits::default(),
            Box::new(move |want_battery| {
                sampler
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sample(want_battery)
            }),
        )
    }

    pub fn with_inputs(
        max_jobs: usize,
        limits: GateLimits,
        waits: MaxWaits,
        inputs: InputSource,
    ) -> Self {
        Self {
            max_jobs: max_jobs.max(1),
            limits,
            waits,
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            inputs,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn inputs(&self) -> GateInputs {
        (self.inputs)(self.limits.pause_on_battery)
    }

    /// Run `fut` as one governed index job. Re-entrant: nested calls in the same
    /// task run directly under the outer job's slot.
    pub async fn run_exclusive<F: Future>(&self, label: &str, fut: F) -> F::Output {
        if JOB_ID.try_with(|_| ()).is_ok() {
            return fut.await;
        }
        let priority = current_priority();
        let id = self.admit(label, priority).await;
        let _slot = SlotGuard { governor: self, id };
        JOB_ID
            .scope(id, async {
                self.yield_to_reads().await;
                fut.await
            })
            .await
    }

    /// [`Self::run_exclusive`] for an index job: records its outcome in
    /// [`crate::health::log`], keyed by `label`. An error after `cancel` fired
    /// is logged as cancelled, not counted as a failure.
    pub async fn run_job<T, E: std::fmt::Display>(
        &self,
        label: &str,
        cancel: &CancellationToken,
        fut: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        if JOB_ID.try_with(|_| ()).is_ok() {
            return fut.await;
        }
        self.run_exclusive(label, async {
            let started = Instant::now();
            let result = fut.await;
            let log = crate::health::log();
            match &result {
                Err(_) if cancel.is_cancelled() => {
                    log.event(EventLevel::Info, "index job cancelled", Some(label))
                }
                _ => log.job_finished(
                    label,
                    started.elapsed(),
                    result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")),
                ),
            }
            result
        })
        .await
    }

    async fn admit(&self, label: &str, priority: IndexPriority) -> u64 {
        let id = {
            let mut state = self.lock();
            state.next_id += 1;
            let id = state.next_id;
            state.waiting.push(Job {
                id,
                label: label.to_string(),
                priority,
                since: Instant::now(),
                paused: None,
                paused_since: None,
            });
            id
        };
        let deadline = Instant::now() + self.waits.of(priority);
        loop {
            let notified = self.changed.notified();
            {
                let mut state = self.lock();
                let overdue = Instant::now() >= deadline;
                if state.may_start(id, self.max_jobs) || overdue {
                    if overdue {
                        let msg = format!(
                            "index job waited {:?} for a slot; starting anyway",
                            self.waits.of(priority)
                        );
                        tracing::warn!("{msg}: {label}");
                        crate::health::log().event(EventLevel::Warn, msg, Some(label));
                    }
                    let pos = state.waiting.iter().position(|j| j.id == id).unwrap_or(0);
                    let mut job = state.waiting.remove(pos);
                    job.since = Instant::now();
                    state.running.push(job);
                    break;
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), notified).await;
        }
        self.changed.notify_waiters();
        id
    }

    /// Pause the calling job while the gate is closed, up to its [`MaxWaits`] cap.
    pub async fn yield_to_reads(&self) {
        let priority = current_priority();
        let job = JOB_ID.try_with(|id| *id).ok();
        let started = Instant::now();
        loop {
            let reason = evaluate(&self.inputs(), &self.limits, priority);
            self.set_paused(job, reason.clone());
            let Some(reason) = reason else { return };
            if started.elapsed() >= self.waits.of(priority) {
                let msg = format!(
                    "indexing paused {:?} ({reason}); continuing to avoid starvation",
                    self.waits.of(priority)
                );
                tracing::warn!("{msg}");
                crate::health::log().event(EventLevel::Warn, msg, self.label_of(job).as_deref());
                self.set_paused(job, None);
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn label_of(&self, job: Option<u64>) -> Option<String> {
        let id = job?;
        let state = self.lock();
        state
            .running
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.label.clone())
    }

    /// Update a running job's pause reason, logging pause and resume transitions.
    fn set_paused(&self, job: Option<u64>, reason: Option<String>) {
        let Some(id) = job else { return };
        let transition = {
            let mut state = self.lock();
            let Some(j) = state.running.iter_mut().find(|j| j.id == id) else {
                return;
            };
            let transition = match (&j.paused_since, &reason) {
                (None, Some(reason)) => {
                    j.paused_since = Some(Instant::now());
                    Some((EventLevel::Warn, format!("indexing paused: {reason}")))
                }
                (Some(since), None) => {
                    let msg = format!("indexing resumed after {}s", since.elapsed().as_secs());
                    j.paused_since = None;
                    Some((EventLevel::Info, msg))
                }
                _ => None,
            };
            j.paused = reason;
            transition.map(|(level, msg)| (level, msg, j.label.clone()))
        };
        if let Some((level, msg, label)) = transition {
            crate::health::log().event(level, msg, Some(&label));
        }
    }

    pub fn status(&self) -> GovernorStatus {
        let view = |j: &Job| JobView {
            label: j.label.clone(),
            priority: j.priority,
            secs: j.since.elapsed().as_secs(),
            paused: j.paused.clone(),
        };
        let (running, waiting) = {
            let state = self.lock();
            (
                state.running.iter().map(view).collect(),
                state.waiting.iter().map(view).collect(),
            )
        };
        GovernorStatus {
            max_jobs: self.max_jobs,
            running,
            waiting,
            limits: self.limits.clone(),
            inputs: self.inputs(),
        }
    }
}

struct SlotGuard<'a> {
    governor: &'a IndexGovernor,
    id: u64,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.governor.lock().running.retain(|j| j.id != self.id);
        self.governor.changed.notify_waiters();
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Limits from the environment (see `constants::INDEX_*` / `READ_LATENCY_TARGET_MS_ENV`).
pub fn configured() -> (usize, GateLimits) {
    use crate::constants as c;
    let max_jobs = env_parse::<usize>(c::INDEX_MAX_JOBS_ENV)
        .filter(|&n| n > 0)
        .unwrap_or(1);
    let limits = GateLimits {
        read_target_ms: env_parse(c::READ_LATENCY_TARGET_MS_ENV).unwrap_or(1_000),
        max_other_cpu_percent: env_parse(c::INDEX_MAX_OTHER_CPU_ENV).unwrap_or(70.0),
        pause_on_battery: env_parse(c::INDEX_PAUSE_ON_BATTERY_ENV).unwrap_or(false),
    };
    (max_jobs, limits)
}

/// The process-wide governor.
pub fn global() -> &'static IndexGovernor {
    static GOVERNOR: OnceLock<IndexGovernor> = OnceLock::new();
    GOVERNOR.get_or_init(|| {
        let (max_jobs, limits) = configured();
        tracing::info!("index governor: {max_jobs} job(s) at a time, limits {limits:?}");
        IndexGovernor::new(max_jobs, limits)
    })
}

#[cfg(test)]
#[path = "governor_tests.rs"]
mod tests;
