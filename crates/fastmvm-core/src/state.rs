use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    New,
    Preparing,
    Starting,
    Ready,
    Running,
    Stopping,
    Failed,
    TimedOut,
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    Prepare,
    Start,
    AgentReady,
    CommandStarted,
    CommandExited,
    StopRequested,
    Timeout,
    Fail,
    CleanupComplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    Exited,
    Cancelled,
    TimedOut,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lifecycle {
    state: SessionState,
    termination_reason: Option<TerminationReason>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            state: SessionState::New,
            termination_reason: None,
        }
    }
}

impl Lifecycle {
    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn termination_reason(&self) -> Option<TerminationReason> {
        self.termination_reason
    }

    pub fn apply(&mut self, event: LifecycleEvent) -> Result<SessionState, LifecycleError> {
        let next = match (self.state, event) {
            (SessionState::New, LifecycleEvent::Prepare) => SessionState::Preparing,
            (SessionState::Preparing, LifecycleEvent::Start) => SessionState::Starting,
            (SessionState::Starting, LifecycleEvent::AgentReady) => SessionState::Ready,
            (SessionState::Ready, LifecycleEvent::CommandStarted) => SessionState::Running,
            (SessionState::Running, LifecycleEvent::CommandExited) => {
                self.termination_reason = Some(TerminationReason::Exited);
                SessionState::Stopping
            }
            (SessionState::Ready | SessionState::Running, LifecycleEvent::StopRequested) => {
                self.termination_reason = Some(TerminationReason::Cancelled);
                SessionState::Stopping
            }
            (
                SessionState::Preparing
                | SessionState::Starting
                | SessionState::Ready
                | SessionState::Running,
                LifecycleEvent::Timeout,
            ) => {
                self.termination_reason = Some(TerminationReason::TimedOut);
                SessionState::TimedOut
            }
            (
                SessionState::New
                | SessionState::Preparing
                | SessionState::Starting
                | SessionState::Ready
                | SessionState::Running
                | SessionState::Stopping,
                LifecycleEvent::Fail,
            ) => {
                self.termination_reason = Some(TerminationReason::Failed);
                SessionState::Failed
            }
            (
                SessionState::Stopping | SessionState::Failed | SessionState::TimedOut,
                LifecycleEvent::CleanupComplete,
            ) => SessionState::Dead,
            _ => {
                return Err(LifecycleError::InvalidTransition {
                    state: self.state,
                    event,
                });
            }
        };
        self.state = next;
        Ok(next)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    #[error("invalid lifecycle transition: {state:?} + {event:?}")]
    InvalidTransition {
        state: SessionState,
        event: LifecycleEvent,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_happy_path_reaches_dead() {
        let mut lifecycle = Lifecycle::default();
        for event in [
            LifecycleEvent::Prepare,
            LifecycleEvent::Start,
            LifecycleEvent::AgentReady,
            LifecycleEvent::CommandStarted,
            LifecycleEvent::CommandExited,
            LifecycleEvent::CleanupComplete,
        ] {
            lifecycle.apply(event).unwrap();
        }
        assert_eq!(lifecycle.state(), SessionState::Dead);
        assert_eq!(
            lifecycle.termination_reason(),
            Some(TerminationReason::Exited)
        );
    }

    #[test]
    fn timeout_requires_cleanup() {
        let mut lifecycle = Lifecycle::default();
        lifecycle.apply(LifecycleEvent::Prepare).unwrap();
        lifecycle.apply(LifecycleEvent::Timeout).unwrap();
        assert_eq!(lifecycle.state(), SessionState::TimedOut);
        lifecycle.apply(LifecycleEvent::CleanupComplete).unwrap();
        assert_eq!(lifecycle.state(), SessionState::Dead);
    }

    #[test]
    fn invalid_transition_is_rejected() {
        let mut lifecycle = Lifecycle::default();
        assert!(lifecycle.apply(LifecycleEvent::CommandStarted).is_err());
        assert_eq!(lifecycle.state(), SessionState::New);
    }
}
