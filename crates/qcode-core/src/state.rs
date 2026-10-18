//! Turn state machine.
//!
//! Approval is a state, not a UI popup: the loop must be able to sit in
//! `AwaitingApproval` while the frontend collects a decision, and every other
//! transition has to remain legal in headless mode too.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Idle,
    Streaming,
    AwaitingApproval,
    RunningTool,
    Compacting,
    Cancelling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Submit,
    NeedApproval,
    ToolsAllowed,
    Approve,
    Deny,
    ToolsDone,
    StreamEnded,
    Compact,
    CompactionDone,
    Cancel,
    CleanupDone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid transition: {from:?} + {trigger:?}")]
pub struct TransitionError {
    pub from: TurnState,
    pub trigger: Trigger,
}

impl TurnState {
    pub fn next(self, trigger: Trigger) -> Result<TurnState, TransitionError> {
        use Trigger::*;
        use TurnState::*;

        let next = match (self, trigger) {
            (Idle, Submit) => Streaming,
            (Streaming, NeedApproval) => AwaitingApproval,
            (Streaming, ToolsAllowed) => RunningTool,
            (Streaming, StreamEnded) => Idle,
            (Streaming, Compact) => Compacting,
            (AwaitingApproval, Approve) => RunningTool,
            (AwaitingApproval, Deny) => Streaming,
            (RunningTool, ToolsDone) => Streaming,
            (Compacting, CompactionDone) => Streaming,
            (Cancelling, CleanupDone) => Idle,
            (_, Cancel) if self != Idle && self != Cancelling => Cancelling,
            _ => {
                return Err(TransitionError {
                    from: self,
                    trigger,
                });
            }
        };
        Ok(next)
    }

    pub fn is_idle(self) -> bool {
        self == TurnState::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path() {
        let s = TurnState::Idle;
        let s = s.next(Trigger::Submit).unwrap();
        assert_eq!(s, TurnState::Streaming);
        let s = s.next(Trigger::ToolsAllowed).unwrap();
        assert_eq!(s, TurnState::RunningTool);
        let s = s.next(Trigger::ToolsDone).unwrap();
        assert_eq!(s, TurnState::Streaming);
        let s = s.next(Trigger::StreamEnded).unwrap();
        assert_eq!(s, TurnState::Idle);
    }

    #[test]
    fn approval_branch() {
        let s = TurnState::Streaming.next(Trigger::NeedApproval).unwrap();
        assert_eq!(s, TurnState::AwaitingApproval);
        assert_eq!(s.next(Trigger::Approve).unwrap(), TurnState::RunningTool);
        assert_eq!(s.next(Trigger::Deny).unwrap(), TurnState::Streaming);
    }

    #[test]
    fn cancellation_from_every_live_state() {
        for state in [
            TurnState::Streaming,
            TurnState::AwaitingApproval,
            TurnState::RunningTool,
            TurnState::Compacting,
        ] {
            assert_eq!(state.next(Trigger::Cancel).unwrap(), TurnState::Cancelling);
        }
        assert_eq!(
            TurnState::Cancelling.next(Trigger::CleanupDone).unwrap(),
            TurnState::Idle
        );
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        assert!(TurnState::Idle.next(Trigger::Approve).is_err());
        assert!(TurnState::Idle.next(Trigger::Cancel).is_err());
        assert!(TurnState::RunningTool.next(Trigger::Submit).is_err());
        assert!(TurnState::Cancelling.next(Trigger::Submit).is_err());
    }
}
