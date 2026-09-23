use super::*;

#[test]
fn parse_accepts_known_classes_case_insensitively() {
    let cases = [
        ("background", Some(ThreadQos::Background)),
        ("Utility", Some(ThreadQos::Utility)),
        (" user-initiated ", Some(ThreadQos::UserInitiated)),
        ("interactive", Some(ThreadQos::UserInitiated)),
        ("fast", None),
        ("", None),
    ];
    for (input, expected) in cases {
        assert_eq!(ThreadQos::parse(input), expected, "input {input:?}");
    }
}

#[test]
fn set_current_thread_succeeds_for_every_class() {
    for qos in [
        ThreadQos::Background,
        ThreadQos::Utility,
        ThreadQos::UserInitiated,
    ] {
        std::thread::spawn(move || set_current_thread(qos))
            .join()
            .unwrap()
            .unwrap_or_else(|e| panic!("{qos:?}: {e:#}"));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn threads_spawned_by_a_demoted_thread_do_not_inherit_its_class() {
    let (own, child) = std::thread::spawn(|| {
        set_current_thread(ThreadQos::Background).unwrap();
        let child = std::thread::spawn(platform::current_class).join().unwrap();
        (platform::current_class(), child)
    })
    .join()
    .unwrap();

    let background = platform::class_of(ThreadQos::Background);
    assert_eq!(own, background);
    assert_ne!(
        child, background,
        "macOS now propagates QoS to child threads; indexing could drop intra_threads=1"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn background_sets_the_thread_nice_value() {
    let nice = std::thread::spawn(|| {
        set_current_thread(ThreadQos::Background).unwrap();
        // SAFETY: reading our own thread's priority.
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            libc::getpriority(libc::PRIO_PROCESS, tid)
        }
    })
    .join()
    .unwrap();
    assert_eq!(nice, platform::nice_of(ThreadQos::Background));
}

#[test]
fn as_str_round_trips_through_parse() {
    for qos in [
        ThreadQos::UserInitiated,
        ThreadQos::Utility,
        ThreadQos::Background,
    ] {
        assert_eq!(ThreadQos::parse(qos.as_str()), Some(qos));
    }
}
