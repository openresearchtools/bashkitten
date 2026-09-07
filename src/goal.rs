//! Explicit user-created session goals; no additional model tool or scheduler.
use crate::agent::{SessionEntry, SessionEntryKind};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Set {
        objective: String,
    },
    Complete {
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        goal_id: Option<String>,
    },
    Pause,
    Resume,
    Clear,
}
#[derive(Default)]
pub struct State {
    pub current: Option<Goal>,
    pub pending: Vec<Option<Goal>>,
}
impl State {
    pub fn active(&self) -> bool {
        self.current.as_ref().is_some_and(|g| g.status == "active")
    }
    pub fn apply(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Set { objective } => {
                let objective = objective.trim();
                if objective.is_empty() {
                    bail!("A goal needs an objective");
                }
                self.current = Some(Goal {
                    id: uuid::Uuid::now_v7().to_string(),
                    objective: objective.into(),
                    status: "active".into(),
                    summary: None,
                });
            }
            Action::Clear => self.current = None,
            Action::Complete { summary, goal_id } => {
                let goal = self
                    .current
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("No goal has been set"))?;
                if goal_id.as_ref().is_some_and(|id| id != &goal.id) {
                    bail!("The active goal changed; inspect it before completing it");
                }
                goal.status = "complete".into();
                goal.summary = summary;
            }
            Action::Pause => {
                if !self.active() {
                    return Ok(());
                }
                self.current.as_mut().unwrap().status = "paused".into();
            }
            Action::Resume => {
                let goal = self
                    .current
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("No goal has been set"))?;
                if goal.status == "complete" {
                    bail!("This goal is complete. Set a new goal to continue.");
                }
                goal.status = "active".into();
            }
        }
        self.pending.push(self.current.clone());
        Ok(())
    }
}
pub fn restore(initial: Option<Goal>, entries: &[SessionEntry]) -> Option<Goal> {
    let mut goal = initial;
    for entry in entries {
        if let SessionEntryKind::Custom {
            custom_type,
            data: Some(data),
        } = &entry.kind
            && custom_type == "bashkitten.goal"
        {
            goal = serde_json::from_value(data.clone()).unwrap_or(None);
        }
    }
    goal
}
pub fn reminder(goal: &Goal) -> String {
    format!(
        "Continue working toward the user-set goal:\n\n{}\n\nKeep working across turns until the goal is fully complete. Use the existing bash tool to run `bashkitten goal complete --goal-id {} --summary 'what was completed'` only when it is fully achieved. If progress requires user input, run `bashkitten goal pause` and explain what is needed. The BASHKITTEN_SESSION_ID environment variable identifies this session. Do not mark the goal complete merely because this turn ended.",
        goal.objective, goal.id
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicit_goals_continue_and_completed_goals_cannot_resume() {
        let mut state = State::default();
        assert!(!state.active());
        assert!(state.apply(Action::Resume).is_err());
        state
            .apply(Action::Set {
                objective: "Build the requested site".into(),
            })
            .unwrap();
        assert!(state.active());
        state.apply(Action::Pause).unwrap();
        assert!(!state.active());
        state.apply(Action::Resume).unwrap();
        assert!(state.active());
        state
            .apply(Action::Complete {
                summary: Some("Verified site".into()),
                goal_id: None,
            })
            .unwrap();
        assert!(!state.active());
        assert!(state.apply(Action::Resume).is_err());
        assert_eq!(
            state.current.unwrap().summary.as_deref(),
            Some("Verified site")
        );
    }
}
