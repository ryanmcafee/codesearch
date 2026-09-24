//! Per-thread scheduling priority so the read path outranks indexing.
//!
//! macOS: `pthread_set_qos_class_self_np`. Child threads do NOT inherit it, so
//! indexing work must run on threads that set it themselves (see `index::executor`).
//! Linux: per-thread nice via `setpriority(PRIO_PROCESS, tid, n)`.
//! Elsewhere: no-op.

use anyhow::Result;

/// Scheduling class for a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadQos {
    /// Serving tool calls: highest non-UI class.
    UserInitiated,
    /// Long-running work the user asked for but is not waiting on.
    Utility,
    /// Best-effort maintenance (indexing): runs when the machine is otherwise idle.
    Background,
}

impl ThreadQos {
    /// Stable label used in `/status` and env values.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserInitiated => "user-initiated",
            Self::Utility => "utility",
            Self::Background => "background",
        }
    }

    /// Parse `utility` / `background` / `user-initiated` (case-insensitive).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "user-initiated" | "user_initiated" | "interactive" => Some(Self::UserInitiated),
            "utility" => Some(Self::Utility),
            "background" => Some(Self::Background),
            _ => None,
        }
    }
}

/// Apply `qos` to the calling thread.
pub fn set_current_thread(qos: ThreadQos) -> Result<()> {
    platform::set_current_thread(qos)
}

/// The calling thread's class, or `None` when unset or unsupported here.
pub fn current_thread() -> Option<ThreadQos> {
    platform::current_thread()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::ThreadQos;
    use anyhow::{anyhow, Result};

    const QOS_CLASS_USER_INITIATED: u32 = 0x19;
    const QOS_CLASS_UTILITY: u32 = 0x11;
    const QOS_CLASS_BACKGROUND: u32 = 0x09;

    extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        fn qos_class_self() -> u32;
    }

    pub(super) fn class_of(qos: ThreadQos) -> u32 {
        match qos {
            ThreadQos::UserInitiated => QOS_CLASS_USER_INITIATED,
            ThreadQos::Utility => QOS_CLASS_UTILITY,
            ThreadQos::Background => QOS_CLASS_BACKGROUND,
        }
    }

    pub(super) fn set_current_thread(qos: ThreadQos) -> Result<()> {
        // SAFETY: plain libSystem call on the current thread; no pointers involved.
        let rc = unsafe { pthread_set_qos_class_self_np(class_of(qos), 0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(anyhow!(
                "pthread_set_qos_class_self_np({qos:?}) failed with errno {rc}"
            ))
        }
    }

    pub(super) fn current_class() -> u32 {
        // SAFETY: reads the calling thread's QoS class; no arguments.
        unsafe { qos_class_self() }
    }

    pub(super) fn current_thread() -> Option<ThreadQos> {
        let class = current_class();
        [
            ThreadQos::UserInitiated,
            ThreadQos::Utility,
            ThreadQos::Background,
        ]
        .into_iter()
        .find(|qos| class_of(*qos) == class)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::ThreadQos;
    use anyhow::{anyhow, Result};

    pub(super) fn nice_of(qos: ThreadQos) -> i32 {
        match qos {
            ThreadQos::UserInitiated => 0,
            ThreadQos::Utility => 10,
            ThreadQos::Background => 19,
        }
    }

    pub(super) fn set_current_thread(qos: ThreadQos) -> Result<()> {
        // SAFETY: gettid has no arguments; setpriority on our own tid only
        // lowers (or keeps) this thread's priority.
        let rc = unsafe {
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            libc::setpriority(libc::PRIO_PROCESS, tid, nice_of(qos))
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(anyhow!(
                "setpriority({qos:?}) failed: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    pub(super) fn current_thread() -> Option<ThreadQos> {
        // SAFETY: reading our own thread's priority.
        let nice = unsafe {
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            libc::getpriority(libc::PRIO_PROCESS, tid)
        };
        [
            ThreadQos::UserInitiated,
            ThreadQos::Utility,
            ThreadQos::Background,
        ]
        .into_iter()
        .find(|qos| nice_of(*qos) == nice)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::ThreadQos;
    use anyhow::Result;

    pub(super) fn set_current_thread(_qos: ThreadQos) -> Result<()> {
        Ok(())
    }

    pub(super) fn current_thread() -> Option<ThreadQos> {
        None
    }
}

#[cfg(test)]
#[path = "qos_tests.rs"]
mod tests;
