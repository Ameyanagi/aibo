//! Agent-run state, rendered *inside the panel* (owner redesign, 2026-08-02).
//!
//! This began life as a separate task window, and §6's original argument for
//! that window was blocking approvals: "a transient overlay cannot host a
//! ten-minute agent run with … blocking approval prompts". The consent model
//! has since moved to agent self-confirmation — nothing blocks on the user in
//! a native run — which left the window a read-only progress display stealing
//! focus from the panel. The owner's ruling: one surface. A run renders as an
//! activity card in its session's conversation, and every run (whatever its
//! session) is reachable through the panel's ⌘T tasks overlay.
//!
//! What §6 still guarantees is unchanged and lives in the runtime: a run
//! outlives the panel, dismissing the panel cancels nothing, and the hotkey
//! during a run interrupts nothing.

use aibo_core::types::{
    AgentOutcome, AgentStatus, AgentStep, ApprovalDecision, ApprovalRequest, Usage,
};
use uuid::Uuid;

use crate::bridge::SessionId;
use crate::i18n::{self, Key};

/// One entry in the scrollback.
///
/// A flattened mirror of [`AgentStep`] rather than the enum itself: the UI
/// needs a per-step collapsed/expanded flag and a stable position, and steps
/// arrive interleaved with approvals that resolve out of band.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The step as the backend reported it.
    pub step: AgentStep,
    /// Reasoning and long tool output start collapsed (§7: reasoning renders
    /// collapsed and is never inserted).
    pub collapsed: bool,
    /// The outcome of a [`AgentStep::ToolUse`] entry, attached when its
    /// [`AgentStep::ToolResult`] arrives. `None` on a tool row means the call
    /// is still running — which is exactly what the timeline renders.
    pub outcome: Option<ToolOutcome>,
}

/// What a running task is doing at this moment, derived from the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity<'a> {
    /// The last tool call has no result yet.
    RunningTool(&'a str),
    /// Between tool calls — the model is reading results or writing.
    WaitingModel,
}

/// A finished tool call's display outcome.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// Bounded output excerpt from the backend.
    pub excerpt: String,
    /// Whether the tool reported failure.
    pub is_error: bool,
}

/// State for one agent run.
#[derive(Debug, Clone)]
pub struct TaskState {
    /// Correlation id, matching [`aibo_core::types::AgentTask::id`].
    pub id: Uuid,
    /// The panel session the run was started from. The card renders inline
    /// in this session's conversation; other sessions reach the run through
    /// the tasks overlay.
    pub session: SessionId,
    /// The user's instruction. Shown as the subject and as approval provenance.
    pub instruction: String,
    /// Scrollback.
    pub entries: Vec<Entry>,
    /// The approval currently blocking the run, if any. Native runs no longer
    /// raise these; MCP first-use and delegates still can.
    pub pending_approval: Option<ApprovalRequest>,
    /// Text typed into a destructive-command confirmation box (§11).
    pub typed_confirmation: String,
    /// Terminal outcome once the run ends.
    pub outcome: Option<AgentOutcome>,
    /// Running token accounting for the spend meter (§14).
    pub usage: Usage,
    /// Steps executed, against [`aibo_core::types::AgentLimits::max_steps`].
    pub steps: u32,
    /// [`Self::final_message`], split once into markdown and typeset math
    /// when the run finishes — the view must borrow, not parse.
    pub final_segments: Vec<crate::math::Segment>,
}

impl TaskState {
    /// A new run, before its first step arrives.
    pub fn new(id: Uuid, session: SessionId, instruction: String) -> Self {
        Self {
            id,
            session,
            instruction,
            entries: Vec::new(),
            pending_approval: None,
            typed_confirmation: String::new(),
            outcome: None,
            usage: Usage::default(),
            steps: 0,
            final_segments: Vec::new(),
        }
    }

    /// Whether the run is still going.
    pub fn is_running(&self) -> bool {
        self.outcome.is_none()
    }

    /// Whether the run is blocked on the user (§11).
    pub fn is_blocked(&self) -> bool {
        self.pending_approval.is_some()
    }

    /// Fold one step into the state.
    pub fn push(&mut self, step: AgentStep) {
        match step {
            AgentStep::AwaitingApproval(request) => {
                self.typed_confirmation.clear();
                self.pending_approval = Some(request);
            }
            AgentStep::Done(outcome) => {
                self.pending_approval = None;
                self.usage = outcome.usage;
                self.steps = outcome.steps;
                self.outcome = Some(outcome);
                if let Some(message) = self.final_message().map(str::to_owned) {
                    self.final_segments = crate::math::segments(&message);
                }
            }
            AgentStep::ToolResult {
                id,
                name,
                excerpt,
                is_error,
            } => {
                let outcome = ToolOutcome { excerpt, is_error };
                // Attach to the call it finishes; the row flips from
                // "running" to its outcome without growing the timeline.
                let matching = self.entries.iter_mut().rev().find(|entry| {
                    matches!(&entry.step, AgentStep::ToolUse { id: call, .. } if *call == id)
                });
                match matching {
                    Some(entry) => entry.outcome = Some(outcome),
                    // A result whose call was never shown (unknown tool):
                    // render it standalone rather than dropping it.
                    None => self.entries.push(Entry {
                        step: AgentStep::ToolResult {
                            id,
                            name,
                            excerpt: outcome.excerpt.clone(),
                            is_error: outcome.is_error,
                        },
                        collapsed: false,
                        outcome: Some(outcome),
                    }),
                }
            }
            other => {
                // Reasoning arrives on its own channel and stays collapsed
                // until the user asks for it (§7).
                let collapsed = matches!(other, AgentStep::Thought(_));
                self.steps = self.steps.saturating_add(1);
                self.entries.push(Entry {
                    step: other,
                    collapsed,
                    outcome: None,
                });
            }
        }
    }

    /// What the run is doing right now, for the running timeline's live
    /// status line. `None` once settled or while blocked on an approval —
    /// those states have their own, louder rendering.
    pub fn activity(&self) -> Option<Activity<'_>> {
        if !self.is_running() || self.is_blocked() {
            return None;
        }
        let last_tool =
            self.entries
                .iter()
                .rev()
                .find_map(|entry| match (&entry.step, &entry.outcome) {
                    (AgentStep::ToolUse { name, .. }, outcome) => Some((name, outcome.is_none())),
                    _ => None,
                });
        match last_tool {
            Some((name, true)) => Some(Activity::RunningTool(name)),
            _ => Some(Activity::WaitingModel),
        }
    }

    /// The run's final message, if it produced one — rendered as the
    /// assistant's reply under the activity card.
    pub fn final_message(&self) -> Option<&str> {
        self.outcome.as_ref()?;
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.step {
                AgentStep::Message(body) => Some(body.as_str()),
                _ => None,
            })
    }

    /// Whether the pending approval can be granted right now.
    ///
    /// §11: the destructive command class requires typed confirmation. The
    /// gate lives here so no view can accidentally offer Approve without it.
    pub fn approval_is_ready(&self) -> bool {
        match &self.pending_approval {
            None => false,
            Some(request) if !request.requires_typed_confirmation => true,
            Some(request) => {
                let expected = request.command.as_deref().unwrap_or(&request.summary);
                self.typed_confirmation.trim() == expected.trim()
            }
        }
    }

    /// Whether `decision` may be sent for the current prompt.
    ///
    /// Denying is always safe while a prompt exists. A one-shot approval must
    /// pass the typed-confirmation gate, while destructive prompts never offer
    /// the broader "approve for session" decision.
    pub fn decision_is_ready(&self, decision: ApprovalDecision) -> bool {
        let Some(request) = &self.pending_approval else {
            return false;
        };
        match decision {
            ApprovalDecision::Deny => true,
            ApprovalDecision::Approve => self.approval_is_ready(),
            // Destructive approvals are intentionally one-shot even after the
            // command has been typed exactly. A generic session grant is too
            // broad for an operation classified as destructive.
            ApprovalDecision::ApproveForSession => {
                !request.requires_typed_confirmation && self.approval_is_ready()
            }
        }
    }

    /// The complete user-visible transcript, in display order.
    pub fn transcript(&self) -> String {
        use std::fmt::Write as _;

        let mut out = self.instruction.clone();
        for entry in &self.entries {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
            match &entry.step {
                AgentStep::Thought(body) => {
                    let _ = writeln!(out, "{}:", i18n::t(Key::TaskThinking));
                    out.push_str(body);
                }
                AgentStep::ToolUse { name, .. } => {
                    out.push_str(&i18n::t1(Key::TaskRunningTool, name));
                    if let Some(outcome) = &entry.outcome {
                        out.push('\n');
                        out.push_str(&outcome.excerpt);
                    }
                }
                AgentStep::ToolResult { name, excerpt, .. } => {
                    let _ = writeln!(out, "{name}:");
                    out.push_str(excerpt);
                }
                AgentStep::FileDiff { path, unified_diff } => {
                    let _ = writeln!(
                        out,
                        "{} {}",
                        i18n::t(Key::TaskFileChanged),
                        path.to_string_lossy()
                    );
                    out.push_str(unified_diff);
                }
                AgentStep::Message(body) => out.push_str(body),
                AgentStep::Steered(body) => {
                    let _ = writeln!(out, "→ {body}");
                }
                // Folded into `pending_approval` / `outcome` by `push`.
                AgentStep::AwaitingApproval(_) | AgentStep::Done(_) => {}
            }
        }

        if let Some(request) = &self.pending_approval {
            let _ = write!(
                out,
                "\n\n{}\n{}",
                i18n::t(Key::TaskAwaitingApproval),
                request.summary
            );
            if let Some(command) = &request.command {
                let _ = write!(out, "\n{command}");
            }
            for path in &request.paths {
                let _ = write!(out, "\n{}", path.to_string_lossy());
            }
        }

        if let Some(outcome) = &self.outcome {
            let status = match &outcome.status {
                AgentStatus::Completed => i18n::t(Key::TaskCompleted).to_owned(),
                AgentStatus::Cancelled => i18n::t(Key::TaskCancelled).to_owned(),
                AgentStatus::Failed(message) => message.clone(),
                AgentStatus::BudgetExceeded(_) => i18n::t(Key::ErrBudgetExceeded).to_owned(),
            };
            let _ = write!(out, "\n\n{status}");
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::types::ToolTier;

    fn task() -> TaskState {
        TaskState::new(
            Uuid::from_u128(7),
            SessionId::from_u128(1),
            "tidy the downloads folder".to_owned(),
        )
    }

    #[test]
    fn steps_fold_into_entries_and_done_closes_the_run() {
        let mut state = task();
        state.push(AgentStep::ToolUse {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "ls"}),
            tier: ToolTier::ShellFs,
        });
        state.push(AgentStep::Message("done looking".into()));
        assert_eq!(state.entries.len(), 2);
        assert!(state.is_running());

        state.push(AgentStep::Done(AgentOutcome {
            status: AgentStatus::Completed,
            usage: Usage::default(),
            steps: 2,
        }));
        assert!(!state.is_running());
        assert_eq!(state.final_message(), Some("done looking"));
    }

    #[test]
    fn tool_results_attach_to_their_call_and_drive_activity() {
        let mut state = task();
        assert_eq!(
            state.activity(),
            Some(Activity::WaitingModel),
            "a fresh run is waiting on the model, visibly"
        );

        state.push(AgentStep::ToolUse {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "ls"}),
            tier: ToolTier::ShellFs,
        });
        assert!(state.entries[0].outcome.is_none());
        assert_eq!(state.activity(), Some(Activity::RunningTool("bash")));

        state.push(AgentStep::ToolResult {
            id: "c1".into(),
            name: "bash".into(),
            excerpt: "src".into(),
            is_error: false,
        });
        assert_eq!(state.entries.len(), 1, "the result joins its call's row");
        assert!(
            state.entries[0]
                .outcome
                .as_ref()
                .is_some_and(|outcome| !outcome.is_error && outcome.excerpt == "src")
        );
        assert_eq!(state.activity(), Some(Activity::WaitingModel));

        state.push(AgentStep::Done(AgentOutcome {
            status: AgentStatus::Completed,
            usage: Usage::default(),
            steps: 1,
        }));
        assert_eq!(state.activity(), None, "settled runs have no live status");
    }

    #[test]
    fn an_orphan_tool_result_still_renders() {
        let mut state = task();
        state.push(AgentStep::ToolResult {
            id: "never-shown".into(),
            name: "mystery".into(),
            excerpt: "no such tool".into(),
            is_error: true,
        });
        assert_eq!(state.entries.len(), 1);
    }

    #[test]
    fn thoughts_start_collapsed() {
        let mut state = task();
        state.push(AgentStep::Thought("hmm".into()));
        assert!(state.entries[0].collapsed);
    }

    #[test]
    fn destructive_approval_requires_the_exact_command() {
        let mut state = task();
        state.push(AgentStep::AwaitingApproval(ApprovalRequest {
            id: "c1".into(),
            kind: aibo_core::types::ApprovalKind::Command,
            summary: "remove build output".into(),
            command: Some("rm -rf build".into()),
            paths: Vec::new(),
            originating_instruction: "clean".into(),
            requires_typed_confirmation: true,
        }));
        assert!(state.is_blocked());
        assert!(!state.approval_is_ready());
        assert!(!state.decision_is_ready(ApprovalDecision::ApproveForSession));
        assert!(state.decision_is_ready(ApprovalDecision::Deny));

        state.typed_confirmation = "rm -rf build".to_owned();
        assert!(state.approval_is_ready());
        assert!(
            !state.decision_is_ready(ApprovalDecision::ApproveForSession),
            "destructive consent is one-shot even typed"
        );
    }
}
