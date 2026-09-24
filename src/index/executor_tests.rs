use super::*;
use crate::testing::EnvRestore;
use serial_test::serial;

#[tokio::test]
async fn run_returns_the_closure_value() {
    let executor = IndexExecutor::new(2, ThreadQos::Background).unwrap();
    assert_eq!(executor.run(|| 40 + 2).await.unwrap(), 42);
}

#[tokio::test]
async fn a_panicking_task_is_an_error_not_a_crash() {
    let executor = IndexExecutor::new(1, ThreadQos::Background).unwrap();
    let err = executor
        .run(|| -> u32 { panic!("chunker exploded") })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("chunker exploded"), "{err}");
    assert_eq!(
        executor.run(|| 7).await.unwrap(),
        7,
        "pool survives a panic"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn pool_threads_and_their_rayon_work_run_at_the_pool_qos() {
    let executor = std::sync::Arc::new(IndexExecutor::new(2, ThreadQos::Background).unwrap());
    let pool = std::sync::Arc::clone(&executor);
    let classes = executor
        .run(move || {
            let own = qos::current_thread();
            let nested: Vec<Option<ThreadQos>> = pool.pool.install(|| {
                (0..16)
                    .into_par_iter()
                    .map(|_| qos::current_thread())
                    .collect()
            });
            (own, nested)
        })
        .await
        .unwrap();

    assert_eq!(classes.0, Some(ThreadQos::Background));
    assert!(
        classes.1.iter().all(|c| *c == Some(ThreadQos::Background)),
        "arroy's rayon build must stay on demoted threads: {:?}",
        classes.1
    );
}

#[test]
#[serial]
fn configured_threads_honours_the_env_and_rejects_zero() {
    let _env = EnvRestore::set(&[(crate::constants::INDEX_THREADS_ENV, "3")]);
    assert_eq!(configured_threads(), 3);
    drop(_env);

    let _env = EnvRestore::set(&[(crate::constants::INDEX_THREADS_ENV, "0")]);
    let fallback = configured_threads();
    assert!((1..=4).contains(&fallback), "{fallback}");
}

#[test]
#[serial]
fn configured_qos_defaults_to_utility_and_parses_background() {
    let _env = EnvRestore::remove(&[crate::constants::INDEX_QOS_ENV]);
    assert_eq!(configured_qos(), ThreadQos::Utility);
    drop(_env);

    let _env = EnvRestore::set(&[(crate::constants::INDEX_QOS_ENV, "background")]);
    assert_eq!(configured_qos(), ThreadQos::Background);
    drop(_env);

    let _env = EnvRestore::set(&[(crate::constants::INDEX_QOS_ENV, "turbo")]);
    assert_eq!(configured_qos(), ThreadQos::Utility);
}
