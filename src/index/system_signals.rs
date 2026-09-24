//! Cached samples of the signals the index governor gates on.

use super::governor::{GateInputs, MemoryPressure};
use std::time::{Duration, Instant};

/// A read slower than the target within this window pauses indexing.
pub const READ_PROTECT_WINDOW: Duration = Duration::from_secs(30);
const RESAMPLE_EVERY: Duration = Duration::from_secs(2);
const BATTERY_EVERY: Duration = Duration::from_secs(60);

pub struct Sampler {
    system: sysinfo::System,
    cpu_primed: bool,
    last: Option<(Instant, GateInputs)>,
    battery: Option<(Instant, Option<bool>)>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self {
            system: sysinfo::System::new(),
            cpu_primed: false,
            last: None,
            battery: None,
        }
    }
}

impl Sampler {
    pub fn sample(&mut self, want_battery: bool) -> GateInputs {
        let now = Instant::now();
        let recent_read_max_ms = crate::telemetry::reads().recent_max_at(
            now,
            READ_PROTECT_WINDOW,
            &crate::telemetry::INTERACTIVE_TOOLS,
        );
        if let Some((at, cached)) = &self.last {
            if now.duration_since(*at) < RESAMPLE_EVERY {
                return GateInputs {
                    recent_read_max_ms,
                    ..cached.clone()
                };
            }
        }
        let inputs = GateInputs {
            recent_read_max_ms,
            memory_pressure: self.memory_pressure(),
            other_cpu_percent: self.other_cpu_percent(),
            on_battery: if want_battery {
                self.on_battery(now)
            } else {
                None
            },
        };
        self.last = Some((now, inputs.clone()));
        inputs
    }

    /// CPU used by everything except this process; `None` until two refreshes exist.
    fn other_cpu_percent(&mut self) -> Option<f32> {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
        let pid = sysinfo::get_current_pid().ok()?;
        self.system.refresh_cpu_usage();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu(),
        );
        if !self.cpu_primed {
            self.cpu_primed = true;
            return None;
        }
        let cores = self.system.cpus().len().max(1) as f32;
        let own = self
            .system
            .process(pid)
            .map_or(0.0, |p| p.cpu_usage() / cores);
        Some((self.system.global_cpu_usage() - own).max(0.0))
    }

    #[cfg(target_os = "macos")]
    fn memory_pressure(&mut self) -> Option<MemoryPressure> {
        memory_pressure()
    }

    #[cfg(not(target_os = "macos"))]
    fn memory_pressure(&mut self) -> Option<MemoryPressure> {
        self.system.refresh_memory();
        pressure_from_available(self.system.available_memory(), self.system.total_memory())
    }

    fn on_battery(&mut self, now: Instant) -> Option<bool> {
        if let Some((at, value)) = self.battery {
            if now.duration_since(at) < BATTERY_EVERY {
                return value;
            }
        }
        let value = on_battery();
        self.battery = Some((now, value));
        value
    }
}

#[cfg(target_os = "macos")]
fn memory_pressure() -> Option<MemoryPressure> {
    let mut level: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let name = c"kern.memorystatus_vm_pressure_level";
    // SAFETY: `level`/`size` describe a valid c_int out-buffer; no new value is written.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut level as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then(|| pressure_from_level(level))
}

/// macOS `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn, 4 critical.
#[cfg(any(target_os = "macos", test))]
pub fn pressure_from_level(level: i32) -> MemoryPressure {
    match level {
        l if l >= 4 => MemoryPressure::Critical,
        2 | 3 => MemoryPressure::Warn,
        _ => MemoryPressure::Normal,
    }
}

/// Available-memory share below 10% warns and below 5% is critical.
#[cfg(any(not(target_os = "macos"), test))]
pub fn pressure_from_available(available: u64, total: u64) -> Option<MemoryPressure> {
    if total == 0 {
        return None;
    }
    let percent = available.saturating_mul(100) / total;
    Some(match percent {
        p if p < 5 => MemoryPressure::Critical,
        p if p < 10 => MemoryPressure::Warn,
        _ => MemoryPressure::Normal,
    })
}

#[cfg(target_os = "macos")]
fn on_battery() -> Option<bool> {
    let out = std::process::Command::new("pmset")
        .args(["-g", "batt"])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).contains("'Battery Power'"))
}

#[cfg(not(target_os = "macos"))]
fn on_battery() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_levels_map_to_classes() {
        let cases = [
            (0, MemoryPressure::Normal),
            (1, MemoryPressure::Normal),
            (2, MemoryPressure::Warn),
            (4, MemoryPressure::Critical),
        ];
        for (level, expected) in cases {
            assert_eq!(pressure_from_level(level), expected, "level {level}");
        }
    }

    #[test]
    fn available_memory_share_maps_to_classes() {
        let cases = [
            (0, 0, None),
            (4, 100, Some(MemoryPressure::Critical)),
            (5, 100, Some(MemoryPressure::Warn)),
            (9, 100, Some(MemoryPressure::Warn)),
            (10, 100, Some(MemoryPressure::Normal)),
            (100, 100, Some(MemoryPressure::Normal)),
        ];
        for (available, total, expected) in cases {
            assert_eq!(
                pressure_from_available(available, total),
                expected,
                "{available}/{total}"
            );
        }
    }

    #[test]
    fn sampler_reports_memory_pressure() {
        assert!(Sampler::default().memory_pressure().is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn memory_pressure_is_readable_on_macos() {
        assert!(memory_pressure().is_some());
    }

    #[test]
    fn cpu_signal_needs_two_samples() {
        let mut sampler = Sampler::default();
        assert_eq!(sampler.other_cpu_percent(), None);
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let other = sampler
            .other_cpu_percent()
            .expect("second sample has a value");
        assert!((0.0..=100.0).contains(&other), "{other}");
    }
}
