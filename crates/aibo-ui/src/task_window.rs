//! Agent steps, diffs, approvals and transcript; outlives the panel (§6).
//!
//! §6 settles the window model and this window is the reason it needed
//! settling: "'it never has a main window' is true for the panel and **false
//! for the Do surface**. A transient overlay cannot host a ten-minute agent run
//! with step scrollback, file diffs, and blocking approval prompts."
//!
//! The lifetime rules that follow, all from §6 and §13:
//!
//! * An agent run **outlives the panel**. Dismissing the panel never cancels a
//!   run — the run continues here and the tray indicates activity.
//! * Pressing the hotkey during a run **does not interrupt** it. A fresh panel
//!   opens; this window keeps going.
//! * Approval is **blocking and happens before the write** (§11): by the time
//!   there is a diff, the side effects already happened.
//!
//! Approvals also carry provenance. §5 rule 3 requires the user to be able to
//! see that a request came from their own instruction and not from a selection
//! or a tool result, so [`view`] renders the originating instruction above
//! every prompt.

use aibo_core::types::{
    AgentOutcome, AgentStatus, AgentStep, ApprovalDecision, ApprovalRequest, ToolTier, Usage,
};
use iced::widget::{column, container, row, scrollable, text, text_input};
use iced::{Element, Length};
use uuid::Uuid;

use crate::i18n::{self, Key};
use crate::theme::{self, Severity, space, type_scale};
use crate::widgets::{self, Action};

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
}

/// State for one agent run.
#[derive(Debug, Clone)]
pub struct TaskState {
    /// Correlation id, matching [`aibo_core::types::AgentTask::id`].
    pub id: Uuid,
    /// The user's instruction. Shown as the subject and as approval provenance.
    pub instruction: String,
    /// Scrollback.
    pub entries: Vec<Entry>,
    /// The approval currently blocking the run, if any.
    pub pending_approval: Option<ApprovalRequest>,
    /// Text typed into a destructive-command confirmation box (§11).
    pub typed_confirmation: String,
    /// Terminal outcome once the run ends.
    pub outcome: Option<AgentOutcome>,
    /// Running token accounting for the spend meter (§14).
    pub usage: Usage,
    /// Steps executed, against [`aibo_core::types::AgentLimits::max_steps`].
    pub steps: u32,
}

impl TaskState {
    /// A new run, before its first step arrives.
    pub fn new(id: Uuid, instruction: String) -> Self {
        Self {
            id,
            instruction,
            entries: Vec::new(),
            pending_approval: None,
            typed_confirmation: String::new(),
            outcome: None,
            usage: Usage::default(),
            steps: 0,
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
            }
            other => {
                // Reasoning arrives on its own channel and stays collapsed
                // until the user asks for it (§7).
                let collapsed = matches!(other, AgentStep::Thought(_));
                self.steps = self.steps.saturating_add(1);
                self.entries.push(Entry {
                    step: other,
                    collapsed,
                });
            }
        }
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
}

/// What the task window emits.
#[derive(Debug, Clone)]
pub enum Message {
    /// Expand or collapse an entry.
    ToggleEntry(usize),
    /// The typed-confirmation field changed.
    ConfirmationChanged(String),
    /// Answer the pending approval.
    Decide(ApprovalDecision),
    /// Cancel the run.
    Cancel,
    /// Close the window. Does not cancel the run (§6).
    Close,
    /// Copy the whole transcript.
    CopyTranscript,
}

/// Render a task window.
pub fn view(state: &TaskState) -> Element<'_, Message> {
    let body = column![
        header(state),
        scrollable(steps(state))
            .style(theme::scroller)
            .height(Length::Fill),
        footer(state),
    ]
    .spacing(space(3.0));

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(space(4.0))
        .style(theme::panel_surface)
        .into()
}

fn header(state: &TaskState) -> Element<'_, Message> {
    let status = match (&state.outcome, state.is_blocked()) {
        (Some(outcome), _) => match &outcome.status {
            AgentStatus::Completed => (Severity::Success, i18n::t(Key::TaskCompleted).to_owned()),
            AgentStatus::Cancelled => (Severity::Info, i18n::t(Key::TaskCancelled).to_owned()),
            // §12/§13: the message is already redacted for display by the
            // backend; the UI does not re-derive it from an error.
            AgentStatus::Failed(message) => (Severity::Danger, message.clone()),
            AgentStatus::BudgetExceeded(_) => (
                Severity::Warning,
                i18n::t(Key::ErrBudgetExceeded).to_owned(),
            ),
        },
        (None, true) => (
            Severity::Warning,
            i18n::t(Key::TaskAwaitingApproval).to_owned(),
        ),
        (None, false) => (Severity::Info, i18n::t(Key::StateLoading).to_owned()),
    };

    column![
        text(widgets::elide(&state.instruction, 160))
            .size(type_scale::HEADING)
            .style(theme::text_primary),
        text(status.1)
            .size(type_scale::META)
            .style(theme::text_severity(status.0)),
    ]
    .spacing(space(1.0))
    .into()
}

fn steps(state: &TaskState) -> Element<'_, Message> {
    if state.entries.is_empty() && state.pending_approval.is_none() {
        return widgets::state_block(Severity::Info, i18n::t(Key::TaskEmpty), None, Vec::new());
    }

    let mut list = column![widgets::section::<Message>(Key::TaskSteps)].spacing(space(2.0));

    for (index, entry) in state.entries.iter().enumerate() {
        list = list.push(step_view(index, entry));
    }

    if let Some(request) = &state.pending_approval {
        list = list.push(approval_view(state, request));
    }

    list.into()
}

fn step_view(index: usize, entry: &Entry) -> Element<'_, Message> {
    match &entry.step {
        AgentStep::Thought(body) => {
            let label = row![
                text(i18n::t(Key::TaskThinking))
                    .size(type_scale::META)
                    .style(theme::text_dim),
            ];
            let mut stack = column![
                iced::widget::button(label)
                    .style(theme::action_button)
                    .padding([space(1.0), space(2.0)])
                    .on_press(Message::ToggleEntry(index)),
            ]
            .spacing(space(1.0));
            if !entry.collapsed {
                stack = stack.push(
                    text(body.clone())
                        .size(type_scale::META)
                        .style(theme::text_dim),
                );
            }
            stack.into()
        }

        AgentStep::ToolUse { name, tier, .. } => container(
            row![
                text(i18n::t1(Key::TaskRunningTool, name))
                    .size(type_scale::META)
                    .style(theme::text_primary),
                text(tier_label(*tier))
                    .size(type_scale::META)
                    .style(theme::text_faint),
            ]
            .spacing(space(2.0)),
        )
        .padding(space(2.0))
        .width(Length::Fill)
        .style(theme::raised)
        .into(),

        AgentStep::FileDiff { path, unified_diff } => {
            widgets::diff_view(&path.to_string_lossy(), unified_diff)
        }

        AgentStep::Message(body) => text(body.clone())
            .size(type_scale::BODY)
            .style(theme::text_primary)
            .into(),

        // Folded into `pending_approval` / `outcome` by `TaskState::push`, so
        // these are unreachable in practice; rendering nothing beats panicking
        // in a view.
        AgentStep::AwaitingApproval(_) | AgentStep::Done(_) => iced::widget::Space::new().into(),
    }
}

const fn tier_label(tier: ToolTier) -> &'static str {
    // Tier names are developer-facing identifiers from §11, not prose, so they
    // are intentionally not in the string catalogue.
    match tier {
        ToolTier::Builtin => "builtin",
        ToolTier::Sandboxed => "sandboxed",
        ToolTier::Mcp => "mcp",
        ToolTier::ShellFs => "shell/fs",
        ToolTier::Delegate => "delegate",
    }
}

/// The blocking approval prompt (§11).
fn approval_view<'a>(state: &'a TaskState, request: &'a ApprovalRequest) -> Element<'a, Message> {
    let mut stack = column![
        text(i18n::t(Key::TaskApprovalProvenance))
            .size(type_scale::META)
            .style(theme::text_dim),
        // §5 rule 3: show the request traces back to the user's instruction and
        // not to a selection a web page controlled.
        text(widgets::elide(&request.originating_instruction, 200))
            .size(type_scale::META)
            .font(theme::MONO_FONT)
            .style(theme::text_primary),
        text(request.summary.clone())
            .size(type_scale::BODY)
            .style(theme::text_primary),
    ]
    .spacing(space(1.5));

    if let Some(command) = &request.command {
        stack = stack.push(
            container(
                text(command.clone())
                    .size(type_scale::META)
                    .font(theme::MONO_FONT)
                    .style(theme::text_primary),
            )
            .width(Length::Fill)
            .padding(space(2.0))
            .style(theme::raised),
        );
    }

    for path in &request.paths {
        stack = stack.push(
            text(path.to_string_lossy().into_owned())
                .size(type_scale::META)
                .font(theme::MONO_FONT)
                .style(theme::text_dim),
        );
    }

    if request.requires_typed_confirmation {
        stack = stack.push(
            text_input("", &state.typed_confirmation)
                .on_input(Message::ConfirmationChanged)
                .size(type_scale::META)
                .font(theme::MONO_FONT)
                .padding(space(2.0))
                .style(theme::input),
        );
    }

    let mut approve = Action::new(
        Key::ActionApprove,
        "⏎",
        Message::Decide(ApprovalDecision::Approve),
    )
    .primary();
    if !state.approval_is_ready() {
        approve = approve.disabled();
    }

    stack = stack.push(widgets::action_list(vec![
        approve,
        Action::new(
            Key::ActionApproveSession,
            "⇧⏎",
            Message::Decide(ApprovalDecision::ApproveForSession),
        ),
        Action::new(
            Key::ActionDeny,
            "esc",
            Message::Decide(ApprovalDecision::Deny),
        )
        .destructive(),
    ]));

    container(stack)
        .width(Length::Fill)
        .padding(space(3.0))
        .style(theme::banner(Severity::Warning))
        .into()
}

fn footer(state: &TaskState) -> Element<'_, Message> {
    let mut actions = vec![Action::new(Key::ActionCopy, "⌘C", Message::CopyTranscript)];
    if state.is_running() {
        actions.push(Action::new(Key::ActionCancel, "⌘.", Message::Cancel).destructive());
    }
    actions.push(Action::new(Key::ActionDismiss, "esc", Message::Close));

    column![
        widgets::meta_line::<Message>(
            i18n::t(Key::TaskSteps),
            &state.steps.to_string(),
            None,
            Some(&state.usage.total().to_string()),
        ),
        widgets::action_list(actions),
    ]
    .spacing(space(1.5))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aibo_core::types::{AgentStatus, ApprovalKind};

    fn task() -> TaskState {
        TaskState::new(Uuid::from_u128(7), "rename the feature flag".to_owned())
    }

    fn approval(typed: bool) -> ApprovalRequest {
        ApprovalRequest {
            id: "a1".to_owned(),
            kind: ApprovalKind::Command,
            summary: "delete the build directory".to_owned(),
            command: Some("rm -rf ./build".to_owned()),
            paths: Vec::new(),
            originating_instruction: "rename the feature flag".to_owned(),
            requires_typed_confirmation: typed,
        }
    }

    #[test]
    fn a_pending_approval_blocks_the_run() {
        let mut state = task();
        assert!(!state.is_blocked());
        state.push(AgentStep::AwaitingApproval(approval(false)));
        assert!(state.is_blocked());
        assert!(state.approval_is_ready());
    }

    #[test]
    fn destructive_commands_require_the_exact_typed_confirmation() {
        let mut state = task();
        state.push(AgentStep::AwaitingApproval(approval(true)));
        assert!(!state.approval_is_ready());
        state.typed_confirmation = "rm -rf ./bui".to_owned();
        assert!(!state.approval_is_ready());
        state.typed_confirmation = "rm -rf ./build".to_owned();
        assert!(state.approval_is_ready());
    }

    #[test]
    fn reasoning_starts_collapsed_and_messages_do_not() {
        let mut state = task();
        state.push(AgentStep::Thought("chain of thought".to_owned()));
        state.push(AgentStep::Message("done".to_owned()));
        assert!(state.entries[0].collapsed);
        assert!(!state.entries[1].collapsed);
    }

    #[test]
    fn a_terminal_step_clears_the_pending_approval() {
        let mut state = task();
        state.push(AgentStep::AwaitingApproval(approval(false)));
        state.push(AgentStep::Done(AgentOutcome {
            status: AgentStatus::Cancelled,
            usage: Usage::default(),
            steps: 3,
        }));
        assert!(!state.is_blocked());
        assert!(!state.is_running());
        assert_eq!(state.steps, 3);
    }
}
