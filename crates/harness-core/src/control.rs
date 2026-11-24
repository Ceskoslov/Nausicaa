use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Cooperative cancellation shared by the control plane and running tools.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    budget: Option<TaskBudget>,
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.generation_expired()
    }
    pub(crate) fn with_task_budget(mut self, budget: Option<TaskBudget>) -> Self {
        if budget.is_some() {
            self.budget = budget;
        }
        self
    }

    pub(crate) fn generation_expired(&self) -> bool {
        self.budget
            .as_ref()
            .is_some_and(|budget| budget.generation_remaining().is_zero())
            && !self.cancelled.load(Ordering::Acquire)
    }
}

/// A process-local budget shared across the generation attempts for one task.
#[derive(Clone)]
pub struct TaskBudget(Arc<TaskBudgetState>);

struct TaskBudgetState {
    now: Box<dyn Fn() -> Instant + Send + Sync>,
    generation_deadline: Instant,
    verification_deadline: Instant,
    remaining_tokens: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TaskBudgetError {
    #[error(
        "task budget needs positive tokens and a positive verification reserve shorter than the total duration"
    )]
    InvalidLimits,
    #[error("task generation time budget exhausted")]
    GenerationTime,
    #[error("task generation token budget exhausted")]
    GenerationTokens,
}

impl TaskBudget {
    pub fn new(
        total: Duration,
        verification: Duration,
        tokens: u64,
    ) -> Result<Self, TaskBudgetError> {
        Self::with_clock(total, verification, tokens, Instant::now)
    }

    /// The trusted host clock must be monotonic.
    pub fn with_clock(
        total: Duration,
        verification: Duration,
        tokens: u64,
        now: impl Fn() -> Instant + Send + Sync + 'static,
    ) -> Result<Self, TaskBudgetError> {
        if tokens == 0 || verification.is_zero() || verification >= total {
            return Err(TaskBudgetError::InvalidLimits);
        }
        let verification_deadline = now()
            .checked_add(total)
            .ok_or(TaskBudgetError::InvalidLimits)?;
        Ok(Self(Arc::new(TaskBudgetState {
            now: Box::new(now),
            generation_deadline: verification_deadline - verification,
            verification_deadline,
            remaining_tokens: AtomicU64::new(tokens),
        })))
    }

    pub fn generation_remaining(&self) -> Duration {
        self.0
            .generation_deadline
            .saturating_duration_since((self.0.now)())
    }

    pub fn verification_remaining(&self) -> Duration {
        self.0
            .verification_deadline
            .saturating_duration_since((self.0.now)())
    }

    pub fn remaining_tokens(&self) -> u64 {
        self.0.remaining_tokens.load(Ordering::Relaxed)
    }

    /// Conservatively charges the measured input plus the requested output cap.
    /// Once admitted, a request never receives a refund.
    pub fn charge_request(&self, input: u64, output_cap: u64) -> Result<(), TaskBudgetError> {
        if self.generation_remaining().is_zero() {
            return Err(TaskBudgetError::GenerationTime);
        }
        let charge = input
            .checked_add(output_cap)
            .ok_or(TaskBudgetError::GenerationTokens)?;
        self.0
            .remaining_tokens
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(charge)
            })
            .map_err(|_| TaskBudgetError::GenerationTokens)?;
        Ok(())
    }
}

impl std::fmt::Debug for TaskBudget {
    fn fmt(&self, format: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format
            .debug_struct("TaskBudget")
            .field("generation_deadline", &self.0.generation_deadline)
            .field("verification_deadline", &self.0.verification_deadline)
            .field("remaining_tokens", &self.remaining_tokens())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn shared_budget_reserves_time_and_charges_at_exact_token_boundary() {
        let start = Instant::now();
        let now = Arc::new(Mutex::new(start));
        let budget = TaskBudget::with_clock(Duration::from_secs(10), Duration::from_secs(3), 12, {
            let now = now.clone();
            move || *now.lock().unwrap()
        })
        .unwrap();
        budget.charge_request(5, 4).unwrap();
        budget.clone().charge_request(1, 2).unwrap();
        assert_eq!(
            budget.charge_request(1, 1),
            Err(TaskBudgetError::GenerationTokens)
        );
        let token = CancellationToken::new().with_task_budget(Some(budget.clone()));
        *now.lock().unwrap() = start + Duration::from_secs(7);
        assert!(token.is_cancelled());
        assert_eq!(budget.verification_remaining(), Duration::from_secs(3));
        assert_eq!(
            budget.charge_request(1, 0),
            Err(TaskBudgetError::GenerationTime)
        );
    }
}
