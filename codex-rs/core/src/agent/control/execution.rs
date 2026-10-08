//! Tracks local running capacity and releases reservations through the shared guard.
//! The local permit owns the running count; root and MAv1 turns remain unrestricted.

use super::LocalAgentControl;
use crate::agent::types::AgentExecutionGuard;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tracing::debug;
use tracing::warn;

#[derive(Default)]
pub(super) struct AgentExecutionLimiter {
    active: AtomicUsize,
    max_threads: OnceLock<usize>,
}

struct LocalExecutionPermit {
    limiter: Arc<AgentExecutionLimiter>,
}

impl Drop for LocalExecutionPermit {
    fn drop(&mut self) {
        let active = self
            .limiter
            .active
            .fetch_sub(1, Ordering::AcqRel)
            .saturating_sub(1);
        debug!(
            limit_kind = "execution",
            active, "released subagent execution capacity"
        );
    }
}

impl LocalAgentControl {
    pub(crate) fn ensure_execution_capacity(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> CodexResult<()> {
        if !is_execution_limited(multi_agent_version, session_source) {
            return Ok(());
        }
        let max_threads = self.runtime.agent_execution_limiter.max_threads();
        let active = self.runtime.agent_execution_limiter.active_count();
        if active < max_threads {
            Ok(())
        } else {
            let (resident, pending) = self.runtime.residency.counts();
            warn!(
                limit_kind = "execution",
                active,
                resident,
                pending,
                capacity = max_threads,
                "subagent turn rejected by execution capacity"
            );
            Err(
                CodexErr::new(CodexErrorDetails::AgentLimitReached { max_threads })
                    .with_agent_context(AgentErrorContext::ExecutionCapacity),
            )
        }
    }

    pub(crate) fn execution_guard(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> Option<AgentExecutionGuard> {
        is_execution_limited(multi_agent_version, session_source)
            .then(|| Arc::clone(&self.runtime.agent_execution_limiter).guard())
    }
}

impl AgentExecutionLimiter {
    pub(super) fn initialize(&self, max_threads: usize) {
        self.max_threads.get_or_init(|| max_threads);
    }

    fn max_threads(&self) -> usize {
        self.max_threads.get().copied().unwrap_or(usize::MAX)
    }

    pub(super) fn active_count(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    fn guard(self: Arc<Self>) -> AgentExecutionGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        AgentExecutionGuard::new(LocalExecutionPermit { limiter: self })
    }
}

fn is_execution_limited(
    multi_agent_version: MultiAgentVersion,
    session_source: &SessionSource,
) -> bool {
    multi_agent_version == MultiAgentVersion::V2
        && matches!(session_source, SessionSource::SubAgent(_))
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
