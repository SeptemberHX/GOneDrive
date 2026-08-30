//! Latest local placeholder observation waiting for HSM reconciliation.

use std::path::PathBuf;
use std::sync::Arc;

pub(crate) type PlaceholderObservation = (PathBuf, u64, u64);
pub(crate) type PlaceholderSnapshot = Arc<[PlaceholderObservation]>;

/// Keeps only the newest full-scan result. A scan can finish before the HSM
/// connects, and dropping that result leaves placeholders moved outside their
/// database path readable as sparse zeroes until the next verification pass.
#[derive(Default)]
pub(crate) struct PendingPlaceholderReports {
    latest: Option<PlaceholderSnapshot>,
}

impl PendingPlaceholderReports {
    pub(crate) fn remember(
        &mut self,
        observations: Vec<PlaceholderObservation>,
    ) -> PlaceholderSnapshot {
        let snapshot: PlaceholderSnapshot = observations.into();
        self.latest = Some(snapshot.clone());
        snapshot
    }

    pub(crate) fn snapshot(&self) -> Option<PlaceholderSnapshot> {
        self.latest.clone()
    }

    pub(crate) fn clear_if_current(&mut self, completed: &PlaceholderSnapshot) {
        if self
            .latest
            .as_ref()
            .is_some_and(|latest| Arc::ptr_eq(latest, completed))
        {
            self.latest = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completing_an_old_snapshot_does_not_drop_a_newer_report() {
        let mut pending = PendingPlaceholderReports::default();
        let old = pending.remember(vec![(PathBuf::from("/old"), 1, 2)]);
        let current = pending.remember(vec![(PathBuf::from("/new"), 3, 4)]);

        pending.clear_if_current(&old);
        assert!(Arc::ptr_eq(&pending.snapshot().unwrap(), &current));

        pending.clear_if_current(&current);
        assert!(pending.snapshot().is_none());
    }
}
