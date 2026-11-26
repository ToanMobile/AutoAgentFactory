//! Runner wrapper capturing infra failures, quota limit hits, failed log tails,
//! and final output text for auditor and proposer runs.

use std::sync::{Mutex, PoisonError};

use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, InfraFailure, LimitHit, PortResult,
};

/// Passes a run on to the real runner and captures any infra failure, usage
/// limit, failed log tail, and final text: wrappers like audit and subtask
/// proposer only report their own result.
pub(in crate::factory) struct InfraWatch<'a> {
    runner: &'a dyn AgentRunner,
    seen: Mutex<Option<InfraFailure>>,
    limit_hit: Mutex<Option<LimitHit>>,
    failed_tail: Mutex<Option<String>>,
    last_succeeded: Mutex<bool>,
    last_timed_out: Mutex<bool>,
    last_final_text: Mutex<String>,
}

impl<'a> InfraWatch<'a> {
    pub(in crate::factory) fn new(runner: &'a dyn AgentRunner) -> Self {
        Self {
            runner,
            seen: Mutex::new(None),
            limit_hit: Mutex::new(None),
            failed_tail: Mutex::new(None),
            last_succeeded: Mutex::new(false),
            last_timed_out: Mutex::new(false),
            last_final_text: Mutex::new(String::new()),
        }
    }

    pub(in crate::factory) fn take(self) -> (Option<InfraFailure>, Option<LimitHit>) {
        let (infra, hit, _, _, _, _) = self.take_all();
        (infra, hit)
    }

    pub(in crate::factory) fn take_all(
        self,
    ) -> (
        Option<InfraFailure>,
        Option<LimitHit>,
        Option<String>,
        bool,
        bool,
        String,
    ) {
        (
            self.seen
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.limit_hit
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.failed_tail
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.last_succeeded
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.last_timed_out
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.last_final_text
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

impl AgentRunner for InfraWatch<'_> {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let outcome = self.runner.run(request)?;
        *self
            .last_succeeded
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = outcome.succeeded();
        *self
            .last_timed_out
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = outcome.timed_out;
        *self
            .last_final_text
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = outcome.final_text.clone();
        if let Some(infra) = &outcome.infra {
            *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(infra.clone());
        }
        if let Some(hit) = &outcome.limit_hit {
            *self
                .limit_hit
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(hit.clone());
        }
        if !outcome.succeeded()
            && !outcome.timed_out
            && outcome.infra.is_none()
            && outcome.limit_hit.is_none()
            && !outcome.log_tail.trim().is_empty()
        {
            *self
                .failed_tail
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(outcome.log_tail.clone());
        }
        Ok(outcome)
    }

    fn reports_tokens(&self) -> bool {
        self.runner.reports_tokens()
    }
}
