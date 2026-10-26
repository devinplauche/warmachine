//! Small daemon primitives shared by the daemon process and tests.

use std::collections::HashSet;
use std::sync::Mutex;

/// Per-trigger concurrency guard (AC-3.4): a trigger that is already
/// running cannot start a second concurrent run. Acquiring returns a
/// permit; the trigger is released when the permit drops.
pub struct TriggerGuard {
    running: Mutex<HashSet<String>>,
}

impl Default for TriggerGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl TriggerGuard {
    pub fn new() -> Self {
        Self {
            running: Mutex::new(HashSet::new()),
        }
    }

    /// Take the trigger's slot, or return None if it is already running.
    pub fn try_acquire(self: &std::sync::Arc<Self>, trigger: &str) -> Option<TriggerPermit> {
        let mut running = self.running.lock().expect("trigger guard poisoned");
        if !running.insert(trigger.to_string()) {
            return None;
        }
        Some(TriggerPermit {
            guard: self.clone(),
            trigger: trigger.to_string(),
        })
    }

    pub fn is_running(&self, trigger: &str) -> bool {
        self.running
            .lock()
            .expect("trigger guard poisoned")
            .contains(trigger)
    }

    fn release(&self, trigger: &str) {
        self.running
            .lock()
            .expect("trigger guard poisoned")
            .remove(trigger);
    }
}

/// RAII slot for one running trigger.
pub struct TriggerPermit {
    guard: std::sync::Arc<TriggerGuard>,
    trigger: String,
}

impl Drop for TriggerPermit {
    fn drop(&mut self) {
        self.guard.release(&self.trigger);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn guard_blocks_concurrent_run_and_releases_on_drop() {
        let guard = Arc::new(TriggerGuard::new());
        let permit = guard.try_acquire("ci-watch");
        assert!(permit.is_some());
        assert!(guard.is_running("ci-watch"));
        // Second fire while running: no duplicate run (AC-3.4).
        assert!(guard.try_acquire("ci-watch").is_none());
        // A different trigger is unaffected.
        assert!(guard.try_acquire("esbd-scan").is_some());
        drop(permit);
        assert!(!guard.is_running("ci-watch"));
        assert!(guard.try_acquire("ci-watch").is_some());
    }
}
