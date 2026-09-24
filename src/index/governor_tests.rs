use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn limits() -> GateLimits {
    GateLimits {
        read_target_ms: 1_000,
        max_other_cpu_percent: 70.0,
        pause_on_battery: true,
    }
}

fn quick_waits() -> MaxWaits {
    let w = Duration::from_millis(1_500);
    MaxWaits {
        explicit: w,
        watcher: w,
        background: w,
    }
}

fn calm() -> GateInputs {
    GateInputs {
        recent_read_max_ms: Some(40),
        memory_pressure: Some(MemoryPressure::Normal),
        other_cpu_percent: Some(10.0),
        on_battery: Some(false),
    }
}

#[test]
fn evaluate_gates_by_signal_and_priority() {
    use IndexPriority::*;
    let cases: Vec<(&str, GateInputs, IndexPriority, bool)> = vec![
        ("calm machine", calm(), Background, true),
        (
            "slow read pauses even explicit work",
            GateInputs {
                recent_read_max_ms: Some(4_000),
                ..calm()
            },
            Explicit,
            false,
        ),
        (
            "critical memory pauses everything",
            GateInputs {
                memory_pressure: Some(MemoryPressure::Critical),
                ..calm()
            },
            Explicit,
            false,
        ),
        (
            "warn memory pauses watcher",
            GateInputs {
                memory_pressure: Some(MemoryPressure::Warn),
                ..calm()
            },
            Watcher,
            false,
        ),
        (
            "warn memory lets explicit run",
            GateInputs {
                memory_pressure: Some(MemoryPressure::Warn),
                ..calm()
            },
            Explicit,
            true,
        ),
        (
            "busy CPU pauses background only",
            GateInputs {
                other_cpu_percent: Some(90.0),
                ..calm()
            },
            Background,
            false,
        ),
        (
            "busy CPU lets the watcher run",
            GateInputs {
                other_cpu_percent: Some(90.0),
                ..calm()
            },
            Watcher,
            true,
        ),
        (
            "battery pauses background",
            GateInputs {
                on_battery: Some(true),
                ..calm()
            },
            Background,
            false,
        ),
        (
            "unknown signals do not block",
            GateInputs::default(),
            Background,
            true,
        ),
    ];
    for (name, inputs, priority, open) in cases {
        assert_eq!(
            evaluate(&inputs, &limits(), priority).is_none(),
            open,
            "{name}"
        );
    }
}

fn governor(inputs: impl Fn() -> GateInputs + Send + Sync + 'static) -> Arc<IndexGovernor> {
    Arc::new(IndexGovernor::with_inputs(
        1,
        limits(),
        quick_waits(),
        Box::new(move |_| inputs()),
    ))
}

#[tokio::test]
async fn one_job_at_a_time_and_explicit_work_goes_first() {
    let gov = governor(calm);
    let order = Arc::new(Mutex::new(Vec::new()));
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

    let first = {
        let gov = Arc::clone(&gov);
        tokio::spawn(async move {
            gov.run_exclusive("first", async {
                release_rx.await.ok();
            })
            .await
        })
    };
    while gov.status().running.is_empty() {
        tokio::task::yield_now().await;
    }

    let spawn_job = |label: &'static str, priority| {
        let gov = Arc::clone(&gov);
        let order = Arc::clone(&order);
        tokio::spawn(with_priority(priority, async move {
            gov.run_exclusive(label, async {
                order.lock().unwrap().push(label);
            })
            .await
        }))
    };
    let background = spawn_job("background", IndexPriority::Background);
    while gov.status().waiting.is_empty() {
        tokio::task::yield_now().await;
    }
    let explicit = spawn_job("explicit", IndexPriority::Explicit);
    while gov.status().waiting.len() < 2 {
        tokio::task::yield_now().await;
    }
    assert_eq!(gov.status().running.len(), 1, "max_jobs = 1");

    release_tx.send(()).unwrap();
    first.await.unwrap();
    explicit.await.unwrap();
    background.await.unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["explicit", "background"]);
}

#[tokio::test]
async fn nested_jobs_in_one_task_reuse_the_outer_slot() {
    let gov = governor(calm);
    let inner = Arc::clone(&gov);
    let value = tokio::time::timeout(
        Duration::from_secs(5),
        gov.run_exclusive("outer", async move {
            inner.run_exclusive("inner", async { 7 }).await
        }),
    )
    .await
    .expect("a nested run_exclusive must not deadlock on its own slot");
    assert_eq!(value, 7);
}

#[tokio::test]
async fn yield_waits_while_reads_are_slow_then_resumes() {
    let slow_until = Arc::new(AtomicU64::new(2));
    let polls = Arc::clone(&slow_until);
    let gov = governor(move || {
        let remaining = polls.load(Ordering::SeqCst);
        if remaining > 0 {
            polls.fetch_sub(1, Ordering::SeqCst);
            GateInputs {
                recent_read_max_ms: Some(9_000),
                ..calm()
            }
        } else {
            calm()
        }
    });

    let started = Instant::now();
    gov.run_exclusive("job", async {}).await;
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "must pause for slow reads"
    );
    assert_eq!(slow_until.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_closed_gate_never_starves_the_job() {
    let gov = governor(|| GateInputs {
        recent_read_max_ms: Some(9_000),
        ..calm()
    });
    let finished = tokio::time::timeout(
        Duration::from_secs(10),
        gov.run_exclusive("job", async { true }),
    )
    .await
    .expect("max wait must let the job proceed");
    assert!(finished);
}

#[tokio::test]
async fn status_shows_why_a_running_job_is_paused() {
    let gov = governor(|| GateInputs {
        memory_pressure: Some(MemoryPressure::Critical),
        ..calm()
    });
    let job = {
        let gov = Arc::clone(&gov);
        tokio::spawn(async move { gov.run_exclusive("repo-a", async {}).await })
    };
    let paused = loop {
        if let Some(reason) = gov.status().running.first().and_then(|j| j.paused.clone()) {
            break reason;
        }
        tokio::task::yield_now().await;
    };
    assert!(paused.contains("memory pressure critical"), "{paused}");
    job.await.unwrap();
    assert!(gov.status().running.is_empty());
}

#[tokio::test]
async fn jobs_for_the_same_repo_never_overlap() {
    let gov = Arc::new(IndexGovernor::with_inputs(
        4,
        limits(),
        quick_waits(),
        Box::new(|_| calm()),
    ));
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let first = {
        let gov = Arc::clone(&gov);
        tokio::spawn(async move {
            gov.run_exclusive("/repo", async {
                release_rx.await.ok();
            })
            .await
        })
    };
    while gov.status().running.is_empty() {
        tokio::task::yield_now().await;
    }
    let other = {
        let gov = Arc::clone(&gov);
        tokio::spawn(async move { gov.run_exclusive("/other", async {}).await })
    };
    other.await.unwrap();
    let same = {
        let gov = Arc::clone(&gov);
        tokio::spawn(async move { gov.run_exclusive("/repo", async {}).await })
    };
    while gov.status().waiting.is_empty() {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        gov.status().running.len(),
        1,
        "same repo waits although slots are free"
    );
    release_tx.send(()).unwrap();
    first.await.unwrap();
    same.await.unwrap();
}
