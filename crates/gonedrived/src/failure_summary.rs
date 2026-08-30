//! Bounded summaries for failures in large filesystem batches.

const SAMPLE_LIMIT: usize = 3;
const SAMPLE_CHAR_LIMIT: usize = 512;

#[derive(Default)]
pub(crate) struct FailureSummary {
    total: usize,
    samples: Vec<String>,
}

impl FailureSummary {
    pub(crate) fn record(&mut self, sample: impl FnOnce() -> String) {
        self.total += 1;
        if self.samples.len() < SAMPLE_LIMIT {
            self.samples.push(truncate(sample()));
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub(crate) fn total(&self) -> usize {
        self.total
    }

    pub(crate) fn describe(&self) -> String {
        let mut description = self.samples.join("; ");
        let omitted = self.total.saturating_sub(self.samples.len());
        if omitted > 0 {
            if !description.is_empty() {
                description.push_str("; ");
            }
            description.push_str(&format!("... and {omitted} more"));
        }
        description
    }
}

fn truncate(value: String) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(SAMPLE_CHAR_LIMIT).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn failure_samples_are_bounded() {
        let formatted = Cell::new(0);
        let mut failures = FailureSummary::default();
        for index in 0..10 {
            failures.record(|| {
                formatted.set(formatted.get() + 1);
                format!("failure {index}")
            });
        }

        assert_eq!(failures.total(), 10);
        assert_eq!(formatted.get(), SAMPLE_LIMIT);
        assert_eq!(
            failures.describe(),
            "failure 0; failure 1; failure 2; ... and 7 more"
        );
    }

    #[test]
    fn individual_samples_are_truncated_on_character_boundaries() {
        let mut failures = FailureSummary::default();
        failures.record(|| "文".repeat(SAMPLE_CHAR_LIMIT + 1));

        let description = failures.describe();
        assert_eq!(description.chars().count(), SAMPLE_CHAR_LIMIT + 3);
        assert!(description.ends_with("..."));
    }
}
