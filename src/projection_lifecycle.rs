//! Projection sessions and the availability of published history.
//!
//! Availability survives an ordinary catch-up pass. A failed pass withholds
//! readiness until a later pass covers its captured target successfully.

use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectionPhase {
    Idle,
    Discovering,
    ReconcilingDuplicates,
    WaitingForDiscovery,
    RepairingCalldata,
    ResolvingInputs,
    ValidatingLet,
    ProjectingBlock { block: u64 },
    CheckingEmission { block: u64 },
    PublishingBlock { block: u64 },
    PersistingCursor { block: u64 },
    Auditing,
}

#[derive(Debug, Default)]
enum Availability {
    #[default]
    Pending,
    Ready,
    Blocked {
        error: String,
        at: ProjectionPhase,
    },
}

#[derive(Debug, Clone, Copy)]
struct Pass {
    target_tip: u64,
    phase: ProjectionPhase,
}

#[derive(Debug)]
enum PassState {
    Idle { last_target_tip: Option<u64> },
    Running(Pass),
    Failed(Pass),
}

impl Default for PassState {
    fn default() -> Self {
        Self::Idle {
            last_target_tip: None,
        }
    }
}

impl PassState {
    fn target_tip(&self) -> Option<u64> {
        match self {
            Self::Idle { last_target_tip } => *last_target_tip,
            Self::Running(pass) | Self::Failed(pass) => Some(pass.target_tip),
        }
    }

    fn phase(&self) -> ProjectionPhase {
        match self {
            Self::Idle { .. } => ProjectionPhase::Idle,
            Self::Running(pass) | Self::Failed(pass) => pass.phase,
        }
    }
}

/// Existing health fields remain available; booleans are derived views of the
/// lifecycle and are never independently mutated.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectionHealthSnapshot {
    pub ready: bool,
    pub running: bool,
    pub projector_cursor: u64,
    pub reconcile_cursor: u64,
    pub target_tip: Option<u64>,
    pub last_error: Option<String>,
    pub availability: &'static str,
    pub phase: ProjectionPhase,
    pub blocked_at: Option<ProjectionPhase>,
}

#[derive(Default)]
struct HealthState {
    availability: Availability,
    pass: PassState,
    projector_cursor: u64,
    reconcile_cursor: u64,
}

#[derive(Default)]
pub struct ProjectionHealth {
    state: Mutex<HealthState>,
}

impl ProjectionHealth {
    pub fn snapshot(&self) -> ProjectionHealthSnapshot {
        let state = self.state.lock().expect("projection health");
        let (availability, last_error, blocked_at) = match &state.availability {
            Availability::Pending => ("pending", None, None),
            Availability::Ready => ("ready", None, None),
            Availability::Blocked { error, at } => ("blocked", Some(error.clone()), Some(*at)),
        };
        ProjectionHealthSnapshot {
            ready: matches!(state.availability, Availability::Ready),
            running: !matches!(state.pass, PassState::Idle { .. }),
            projector_cursor: state.projector_cursor,
            reconcile_cursor: state.reconcile_cursor,
            target_tip: state.pass.target_tip(),
            last_error,
            availability,
            phase: state.pass.phase(),
            blocked_at,
        }
    }

    pub(crate) fn begin_pass(
        &self,
        target_tip: u64,
        projector_cursor: u64,
        reconcile_cursor: u64,
    ) -> anyhow::Result<ProjectionPassGuard<'_>> {
        let mut state = self.state.lock().expect("projection health");
        anyhow::ensure!(
            matches!(state.pass, PassState::Idle { .. }),
            "projection pass already running"
        );
        state.pass = PassState::Running(Pass {
            target_tip,
            phase: ProjectionPhase::Discovering,
        });
        state.projector_cursor = projector_cursor;
        state.reconcile_cursor = reconcile_cursor;
        Ok(ProjectionPassGuard { health: Some(self) })
    }

    pub(crate) fn enter_phase(&self, phase: ProjectionPhase) {
        let mut state = self.state.lock().expect("projection health");
        if let PassState::Running(pass) | PassState::Failed(pass) = &mut state.pass {
            pass.phase = phase;
        }
    }

    pub(crate) fn record_failure(&self, error: &anyhow::Error) {
        let mut state = self.state.lock().expect("projection health");
        state.availability = Availability::Blocked {
            error: format!("{error:#}"),
            at: state.pass.phase(),
        };
        if let PassState::Running(pass) = state.pass {
            state.pass = PassState::Failed(pass);
        }
    }

    pub(crate) fn record_progress(&self, projector_cursor: u64, reconcile_cursor: u64) {
        let mut state = self.state.lock().expect("projection health");
        state.projector_cursor = projector_cursor;
        state.reconcile_cursor = reconcile_cursor;
    }

    fn finish_pass(&self, projector_cursor: u64, reconcile_cursor: u64) {
        let mut state = self.state.lock().expect("projection health");
        state.projector_cursor = projector_cursor;
        state.reconcile_cursor = reconcile_cursor;
        if let PassState::Running(pass) = state.pass
            && projector_cursor >= pass.target_tip
            && reconcile_cursor >= pass.target_tip
        {
            state.availability = Availability::Ready;
        }
        state.pass = PassState::Idle {
            last_target_tip: state.pass.target_tip(),
        };
    }

    pub(crate) fn reset(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().expect("projection health");
        anyhow::ensure!(
            matches!(state.pass, PassState::Idle { .. }),
            "cannot reset a running projection pass"
        );
        *state = HealthState::default();
        Ok(())
    }
}

/// A cancelled/unwinding pass cannot leave a healthy or permanently-running
/// observation behind. Finishing consumes the guard and closes the pass.
pub(crate) struct ProjectionPassGuard<'a> {
    health: Option<&'a ProjectionHealth>,
}

impl ProjectionPassGuard<'_> {
    pub(crate) fn finish(mut self, projector_cursor: u64, reconcile_cursor: u64) {
        self.health
            .take()
            .expect("active projection pass")
            .finish_pass(projector_cursor, reconcile_cursor);
    }
}

impl Drop for ProjectionPassGuard<'_> {
    fn drop(&mut self) {
        if let Some(health) = self.health.take() {
            health.record_failure(&anyhow::anyhow!(
                "projection pass interrupted before completion"
            ));
            let snapshot = health.snapshot();
            health.finish_pass(snapshot.projector_cursor, snapshot.reconcile_cursor);
        }
    }
}

/// Restore's pinned target and LET frontier exist together. A live projector
/// cannot accidentally retain a partial restore's privileged replay posture.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectionSession {
    #[default]
    Live,
    Restore {
        target_tip: u64,
        next_deposit_index: u64,
    },
}

impl ProjectionSession {
    pub(crate) fn start_restore(
        self,
        cursor: u64,
        target_tip: u64,
        baseline: u64,
    ) -> anyhow::Result<Self> {
        match self {
            Self::Live => {
                anyhow::ensure!(
                    cursor == 0,
                    "restore must start from genesis to establish the canonical LET frontier"
                );
                Ok(Self::Restore {
                    target_tip,
                    next_deposit_index: baseline,
                })
            }
            Self::Restore {
                target_tip: pinned, ..
            } => {
                anyhow::ensure!(
                    pinned == target_tip,
                    "restore session target changed during replay"
                );
                Ok(self)
            }
        }
    }

    pub(crate) fn require_live(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self, Self::Live),
            "live projection cannot reuse an active restore session; reset first"
        );
        Ok(())
    }

    pub(crate) fn next_deposit_index(self) -> Option<u64> {
        match self {
            Self::Live => None,
            Self::Restore {
                next_deposit_index, ..
            } => Some(next_deposit_index),
        }
    }

    pub(crate) fn target_tip(self) -> Option<u64> {
        match self {
            Self::Live => None,
            Self::Restore { target_tip, .. } => Some(target_tip),
        }
    }

    pub(crate) fn after_publication(
        self,
        block: u64,
        frontier: Option<u64>,
    ) -> anyhow::Result<Self> {
        match (self, frontier) {
            (Self::Live, None) => Ok(self),
            (
                Self::Restore {
                    target_tip,
                    next_deposit_index,
                },
                Some(frontier),
            ) => {
                anyhow::ensure!(
                    block <= target_tip,
                    "publication exceeds the pinned restore target"
                );
                anyhow::ensure!(
                    frontier >= next_deposit_index,
                    "restore LET frontier moved backwards"
                );
                Ok(Self::Restore {
                    target_tip,
                    next_deposit_index: frontier,
                })
            }
            _ => anyhow::bail!("published block does not belong to the current projection session"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_readiness_requires_a_completed_pass_and_latches_failures() {
        let health = ProjectionHealth::default();
        assert!(!health.snapshot().ready);
        health.begin_pass(100, 0, 0).unwrap().finish(0, 50);
        assert!(
            !health.snapshot().ready,
            "discovery alone cannot establish readiness"
        );
        health.begin_pass(100, 0, 50).unwrap().finish(100, 100);
        assert!(health.snapshot().ready);
        let pass = health.begin_pass(110, 100, 100).unwrap();
        assert!(
            health.snapshot().ready,
            "new blocks do not withdraw readiness"
        );
        pass.finish(100, 105);
        assert!(health.snapshot().ready);
        let pass = health.begin_pass(110, 100, 105).unwrap();
        health.enter_phase(ProjectionPhase::CheckingEmission { block: 102 });
        health.record_failure(&anyhow::anyhow!("reserved LET leaf has no BridgeEvent"));
        pass.finish(101, 110);
        assert!(!health.snapshot().ready);
        assert_eq!(
            health.snapshot().blocked_at,
            Some(ProjectionPhase::CheckingEmission { block: 102 })
        );
        let pass = health.begin_pass(112, 101, 110).unwrap();
        assert!(!health.snapshot().ready, "retry start retains the blocker");
        pass.finish(102, 112);
        assert!(
            !health.snapshot().ready,
            "partial recovery retains the blocker"
        );
        health.begin_pass(112, 102, 112).unwrap().finish(112, 112);
        assert!(health.snapshot().ready);
        assert!(health.snapshot().last_error.is_none());
    }

    #[test]
    fn swallowed_discovery_failure_cannot_be_cleared_by_the_same_pass() {
        let health = ProjectionHealth::default();
        health.begin_pass(10, 10, 10).unwrap().finish(10, 10);
        let pass = health.begin_pass(10, 10, 10).unwrap();
        health.record_failure(&anyhow::anyhow!("note discovery unavailable"));
        pass.finish(10, 10);
        assert!(!health.snapshot().ready);
        assert!(!health.snapshot().running);
        health.begin_pass(10, 10, 10).unwrap().finish(10, 10);
        assert!(health.snapshot().ready);
    }

    #[test]
    fn interrupted_pass_is_closed_and_cannot_be_reported_healthy() {
        let health = ProjectionHealth::default();
        health.begin_pass(10, 10, 10).unwrap().finish(10, 10);
        let pass = health.begin_pass(11, 10, 10).unwrap();
        assert!(
            health.begin_pass(12, 10, 10).is_err(),
            "overlapping owners are rejected"
        );
        assert!(health.reset().is_err());
        drop(pass);
        assert!(!health.snapshot().ready);
        assert!(!health.snapshot().running);
        health.begin_pass(11, 10, 10).unwrap().finish(11, 11);
        assert!(health.snapshot().ready);
    }

    #[test]
    fn restore_target_is_immutable_and_frontier_advances_only_within_its_session() {
        let live = ProjectionSession::Live;
        assert!(live.start_restore(1, 20, 0).is_err());
        let restore = live.start_restore(0, 20, 3).unwrap();
        assert!(restore.start_restore(0, 21, 3).is_err());
        assert!(restore.require_live().is_err());
        let progressed = restore.after_publication(10, Some(5)).unwrap();
        assert_eq!(
            progressed.start_restore(10, 20, 0).unwrap(),
            progressed,
            "retry cannot reset the replay frontier"
        );
        assert!(progressed.after_publication(21, Some(6)).is_err());
        assert!(progressed.after_publication(11, Some(4)).is_err());
        assert!(progressed.after_publication(11, None).is_err());
        assert!(live.after_publication(1, Some(0)).is_err());
        assert_eq!(
            progressed
                .after_publication(20, Some(6))
                .unwrap()
                .next_deposit_index(),
            Some(6)
        );
    }
}
