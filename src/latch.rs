//! The confirmation latch.
//!
//! A gated command is not a decision, it is a proposal. `reboot alpha` arms
//! this; `reboot alpha ok` consumes it and runs. Without the second message,
//! a mistyped or relayed command reboots a machine.
//!
//! Three properties matter, and each has a test below:
//!
//! - **Keyed on the canonical command.** `reboot alpha`, `REBOOT  alpha` and
//!   `reboot alpha ok` all reduce to one key, so a user who caps the lock key
//!   does not arm a second, separate latch that never fires.
//! - **Single use.** `take` removes the entry as it reads it, so a replayed
//!   confirmation does not reboot twice.
//! - **A key mismatch never consumes anything.** `reboot beta ok` while
//!   `reboot alpha` is armed must neither run nor spend the `alpha` entry —
//!   otherwise a typo could be answered by rebooting the *other* machine.
//!
//! Expiry is lazy, on access, rather than a timer. The event loop blocks on the
//! MeshCore stream between messages, so a background expiry task would need a
//! `select!` and a shared handle for no benefit: the only thing that matters is
//! whether an entry is still valid at the moment it is consulted.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default arming window. Long enough to type a second message on a phone
/// keyboard, short enough that a forgotten prompt does not authorise a reboot
/// much later.
pub const CONFIRM_TTL_SECS: u64 = 30;

/// Armed commands, keyed by [`crate::parse::Context::canonical`].
#[derive(Debug, Default)]
pub struct Latch {
    armed: HashMap<String, Instant>,
    ttl: Duration,
}

impl Latch {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            armed: HashMap::new(),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Arm `key`, replacing any previous arming. Re-issuing `reboot alpha`
    /// refreshes the window rather than stacking a second entry.
    pub fn arm(&mut self, key: &str) {
        self.prune();
        self.armed
            .insert(key.to_string(), Instant::now() + self.ttl);
    }

    /// Consume `key` if it is armed and unexpired.
    ///
    /// Returns true exactly once per arming. A missing, expired or different
    /// key leaves the store untouched apart from pruning.
    pub fn take(&mut self, key: &str) -> bool {
        self.prune();
        self.armed.remove(key).is_some()
    }

    /// Whether `key` is armed, without spending it.
    #[cfg(test)]
    pub fn is_armed(&self, key: &str) -> bool {
        self.armed
            .get(key)
            .is_some_and(|deadline| Instant::now() < *deadline)
    }

    /// How many entries are held, expired or not. For asserting that a
    /// rejected message did not arm anything.
    #[cfg(test)]
    pub fn armed_count(&self) -> usize {
        self.armed.len()
    }

    /// Drop expired entries so the map cannot grow without bound.
    fn prune(&mut self) {
        let now = Instant::now();
        self.armed.retain(|_, deadline| now < *deadline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn key_of(input: &str) -> String {
        parse::parse(input).unwrap().canonical()
    }

    #[test]
    fn arming_then_taking_runs_once() {
        let mut latch = Latch::new(CONFIRM_TTL_SECS);
        latch.arm("reboot alpha");
        assert!(latch.take("reboot alpha"));
        // Single use: the same confirmation a second time does nothing.
        assert!(!latch.take("reboot alpha"));
    }

    #[test]
    fn taking_with_nothing_armed_does_nothing() {
        let mut latch = Latch::new(CONFIRM_TTL_SECS);
        assert!(!latch.take("reboot alpha"));
    }

    /// A different machine must not be reachable from an existing arming, and
    /// the attempt must not spend the entry that *was* armed.
    #[test]
    fn a_different_key_neither_runs_nor_consumes() {
        let mut latch = Latch::new(CONFIRM_TTL_SECS);
        latch.arm("reboot alpha");
        assert!(!latch.take("reboot beta"));
        assert!(latch.is_armed("reboot alpha"), "alpha was spent");
        assert!(latch.take("reboot alpha"));
    }

    #[test]
    fn re_arming_replaces_rather_than_stacks() {
        let mut latch = Latch::new(CONFIRM_TTL_SECS);
        latch.arm("reboot alpha");
        latch.arm("reboot alpha");
        assert_eq!(latch.armed_count(), 1);
    }

    /// The whole reason `canonical()` exists: three spellings, one key.
    #[test]
    fn the_confirmation_keys_the_entry_arming_it() {
        for spelling in ["reboot alpha", "REBOOT  alpha", "reboot  AlPhA"] {
            assert_eq!(key_of(spelling), "reboot alpha", "{spelling}");
        }
        assert_eq!(key_of("reboot alpha ok"), "reboot alpha");
    }

    #[test]
    fn an_expired_entry_is_not_armed() {
        let mut latch = Latch::new(0);
        latch.arm("reboot alpha");
        assert!(!latch.take("reboot alpha"));
        assert!(!latch.is_armed("reboot alpha"));
    }

    /// A zero window makes every entry already expired, so this shows the prune
    /// deterministically without a sleep: the previous entry is gone the moment
    /// the next access happens, and a lookup cannot resurrect it.
    #[test]
    fn expired_entries_are_pruned_on_access() {
        let mut latch = Latch::new(0);
        latch.arm("reboot alpha");
        latch.arm("reboot beta");
        assert_eq!(latch.armed_count(), 1, "the stale entry should be gone");
        assert!(!latch.is_armed("reboot beta"));

        // An unrelated lookup also prunes, so the map cannot grow unbounded
        // just because nobody ever confirms anything.
        assert!(!latch.take("alarm arm"));
        assert_eq!(latch.armed_count(), 0);
    }
}
