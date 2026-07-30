//! AccessKit semantic snapshots for aibo's custom-rendered iced surfaces.
//!
//! Iced 0.14 draws accessible-looking widgets but does not publish a native
//! semantic hierarchy. These builders mirror the small, stable product
//! vocabulary—not renderer internals—and route native actions back through the
//! same `Message` enums used by mouse and keyboard input.

use accesskit::{
    Action, ActionData, ActionRequest, Affine, Live, Node, NodeId, Rect, Role, Tree, TreeId,
    TreeUpdate,
};
use aibo_core::types::{ApprovalDecision, Health, Permission, PermissionStatus, ProviderId};

use crate::i18n::{self, Key, Lang};
use crate::panel::{self, PanelState, Phase};
use crate::settings::{self, CodexPhase, Section, SettingsState};
use crate::task_window::{self, TaskState};

pub const SETTINGS_ROOT: NodeId = NodeId(1);
pub const PANEL_ROOT: NodeId = NodeId(1_000);
pub const TASK_ROOT: NodeId = NodeId(2_000);

const SETTINGS_NAVIGATION: NodeId = NodeId(2);
const SETTINGS_CONTENT: NodeId = NodeId(3);
const SETTINGS_HEADING: NodeId = NodeId(4);
const SETTINGS_NAV_BASE: u64 = 10;
const ONBOARDING: NodeId = NodeId(40);
const ONBOARDING_STEP_BASE: u64 = 41;
const CODEX_GROUP: NodeId = NodeId(60);
const CODEX_TITLE: NodeId = NodeId(61);
const CODEX_DETAIL: NodeId = NodeId(62);
const CODEX_ACTION: NodeId = NodeId(63);
const CODEX_DISCLOSURE: NodeId = NodeId(64);
const CODEX_CONSENT: NodeId = NodeId(65);
const CODEX_DEVICE_CODE: NodeId = NodeId(66);
const CODEX_COPY_CODE: NodeId = NodeId(67);
const CODEX_OPEN_PAGE: NodeId = NodeId(68);
const SETTINGS_STATUS: NodeId = NodeId(80);
const SETTINGS_PRIMARY_ACTION: NodeId = NodeId(81);
const SETTINGS_PERMISSION_BASE: u64 = 100;
const SETTINGS_LANGUAGE_BASE: u64 = 120;

const PANEL_INPUT: NodeId = NodeId(1_001);
const PANEL_RESPONSE: NodeId = NodeId(1_002);
const PANEL_MODEL: NodeId = NodeId(1_003);
const PANEL_ACCEPT: NodeId = NodeId(1_010);
const PANEL_COPY: NodeId = NodeId(1_011);
const PANEL_DISMISS: NodeId = NodeId(1_012);
const PANEL_SHOW_TASK: NodeId = NodeId(1_013);

const TASK_TRANSCRIPT: NodeId = NodeId(2_001);
const TASK_CONFIRMATION: NodeId = NodeId(2_002);
const TASK_APPROVE: NodeId = NodeId(2_010);
const TASK_APPROVE_SESSION: NodeId = NodeId(2_011);
const TASK_DENY: NodeId = NodeId(2_012);
const TASK_CANCEL: NodeId = NodeId(2_013);
const TASK_CLOSE: NodeId = NodeId(2_014);
const TASK_COPY: NodeId = NodeId(2_015);

/// Build the current Settings semantic tree.
pub fn settings_tree(
    state: &SettingsState,
    size: (f32, f32),
    scale: f32,
    focus: NodeId,
) -> TreeUpdate {
    let width = size.0.max(640.0);
    let height = size.1.max(420.0);
    let mut nodes = Vec::new();

    let nav_ids = Section::VISIBLE.map(section_node_id);
    let mut root = semantic_node(
        Role::Window,
        i18n::t(Key::SettingsTitle),
        logical_rect(0.0, 0.0, width, height),
    );
    root.set_children([SETTINGS_NAVIGATION, SETTINGS_CONTENT]);
    root.set_transform(Affine::scale(f64::from(scale.max(0.5))));
    nodes.push((SETTINGS_ROOT, root));

    let mut navigation = semantic_node(
        Role::TabList,
        i18n::t(Key::SettingsTitle),
        logical_rect(12.0, 12.0, 180.0, height - 24.0),
    );
    navigation.set_children(nav_ids);
    nodes.push((SETTINGS_NAVIGATION, navigation));

    for (index, section) in Section::VISIBLE.into_iter().enumerate() {
        let mut tab = interactive_node(
            Role::Tab,
            i18n::t(section.title()),
            logical_rect(20.0, 20.0 + index as f64 * 48.0, 164.0, 44.0),
        );
        tab.set_selected(section == state.section);
        nodes.push((section_node_id(section), tab));
    }

    let content_x = 212.0;
    let content_width = (f64::from(width) - 224.0).max(0.0);
    let mut content = semantic_node(
        Role::TabPanel,
        i18n::t(state.section.title()),
        logical_rect(content_x, 12.0, content_width, f64::from(height) - 24.0),
    );
    let mut content_children = vec![SETTINGS_HEADING];

    let mut heading = semantic_node(
        Role::Heading,
        i18n::t(state.section.title()),
        logical_rect(content_x, 24.0, content_width, 28.0),
    );
    heading.set_level(1);
    nodes.push((SETTINGS_HEADING, heading));

    match state.section {
        Section::Providers => settings_provider_nodes(
            state,
            content_x,
            content_width,
            &mut nodes,
            &mut content_children,
        ),
        Section::Permissions => settings_permission_nodes(
            state,
            content_x,
            content_width,
            &mut nodes,
            &mut content_children,
        ),
        Section::Budgets => {
            let label = if state.spend_label.is_empty() {
                i18n::t(Key::SettingsBudgets).to_owned()
            } else {
                state.spend_label.clone()
            };
            nodes.push((
                SETTINGS_STATUS,
                semantic_node(
                    Role::Meter,
                    &label,
                    logical_rect(content_x, 68.0, content_width, 64.0),
                ),
            ));
            content_children.push(SETTINGS_STATUS);
        }
        Section::History => settings_history_nodes(
            state,
            content_x,
            content_width,
            &mut nodes,
            &mut content_children,
        ),
        Section::Language => {
            let ids = Lang::ALL
                .iter()
                .copied()
                .map(language_node_id)
                .collect::<Vec<_>>();
            for (index, language) in Lang::ALL.iter().copied().enumerate() {
                let mut option = interactive_node(
                    Role::RadioButton,
                    language.endonym(),
                    logical_rect(content_x, 68.0 + index as f64 * 48.0, content_width, 44.0),
                );
                option.set_selected(language == state.language);
                nodes.push((language_node_id(language), option));
            }
            content_children.extend(ids);
        }
        Section::About => {
            nodes.push((
                SETTINGS_STATUS,
                semantic_node(
                    Role::Label,
                    concat!("aibo ", env!("CARGO_PKG_VERSION")),
                    logical_rect(content_x, 68.0, content_width, 28.0),
                ),
            ));
            nodes.push((
                SETTINGS_PRIMARY_ACTION,
                interactive_node(
                    Role::Button,
                    i18n::t(Key::ActionCopyDiagnostics),
                    logical_rect(content_x, 108.0, 220.0, 44.0),
                ),
            ));
            content_children.extend([SETTINGS_STATUS, SETTINGS_PRIMARY_ACTION]);
        }
        // These sections are intentionally absent from `Section::VISIBLE`.
        Section::Roles | Section::Actions => {}
    }

    content.set_children(content_children);
    nodes.push((SETTINGS_CONTENT, content));
    full_tree(nodes, SETTINGS_ROOT, focus)
}

fn settings_provider_nodes(
    state: &SettingsState,
    x: f64,
    width: f64,
    nodes: &mut Vec<(NodeId, Node)>,
    children: &mut Vec<NodeId>,
) {
    let connected = state
        .providers
        .iter()
        .any(|provider| matches!(provider.health, Health::Ok { .. }));
    let permissions_ready = state.permissions.iter().any(|row| {
        row.permission == Permission::Accessibility && row.status == PermissionStatus::Granted
    });
    let mut y = 68.0;

    if state.onboarding {
        let step_ids = [
            NodeId(ONBOARDING_STEP_BASE),
            NodeId(ONBOARDING_STEP_BASE + 1),
            NodeId(ONBOARDING_STEP_BASE + 2),
        ];
        let mut group = semantic_node(
            Role::Group,
            i18n::t(Key::SettingsWelcomeTitle),
            logical_rect(x, y, width, 190.0),
        );
        group.set_description(i18n::t(Key::SettingsWelcomeBody));
        group.set_children(step_ids);
        nodes.push((ONBOARDING, group));
        for (index, (key, complete)) in [
            (Key::SettingsSetupConnect, connected),
            (
                Key::SettingsSetupPermissions,
                connected && permissions_ready,
            ),
            (Key::SettingsSetupTryHotkey, false),
        ]
        .into_iter()
        .enumerate()
        {
            let mut step = semantic_node(
                Role::ListItem,
                i18n::t(key),
                logical_rect(x + 16.0, y + 50.0 + index as f64 * 44.0, width - 32.0, 44.0),
            );
            step.set_selected(complete);
            nodes.push((step_ids[index], step));
        }
        children.push(ONBOARDING);
        y += 202.0;
    }

    let health = state
        .providers
        .iter()
        .find(|provider| provider.id == ProviderId::CODEX)
        .map(|provider| provider.health.clone())
        .unwrap_or(Health::Unknown);
    let (phase, detail) = CodexPhase::read(&health);
    let mut group_children = vec![CODEX_TITLE, CODEX_DETAIL, CODEX_ACTION, CODEX_DISCLOSURE];
    let card_height = if state.codex_details_expanded {
        232.0
    } else {
        180.0
    };
    let mut group = semantic_node(
        Role::Group,
        i18n::t(Key::SettingsCodexTitle),
        logical_rect(x, y, width, card_height),
    );

    nodes.push((
        CODEX_TITLE,
        semantic_node(
            Role::Heading,
            i18n::t(Key::SettingsCodexTitle),
            logical_rect(x + 16.0, y + 14.0, width - 32.0, 24.0),
        ),
    ));
    nodes.push((
        CODEX_DETAIL,
        semantic_node(
            Role::Label,
            &detail,
            logical_rect(x + 16.0, y + 42.0, width - 32.0, 36.0),
        ),
    ));
    let mut provider_action = interactive_node(
        if matches!(phase, CodexPhase::SignedOut | CodexPhase::Failed) {
            Role::DefaultButton
        } else {
            Role::Button
        },
        settings::codex_action_label(phase),
        logical_rect(x + 16.0, y + 82.0, 220.0, 44.0),
    );
    provider_action.set_description(detail.as_str());
    nodes.push((CODEX_ACTION, provider_action));

    let mut disclosure = interactive_node(
        Role::DisclosureTriangle,
        i18n::t(Key::SettingsCodexHowSignInWorks),
        logical_rect(x + 16.0, y + 130.0, 220.0, 44.0),
    );
    if state.codex_details_expanded {
        disclosure.set_expanded(true);
        group_children.push(CODEX_CONSENT);
        nodes.push((
            CODEX_CONSENT,
            semantic_node(
                Role::Note,
                i18n::t(Key::SettingsCodexConsentNote),
                logical_rect(x + 16.0, y + 178.0, width - 32.0, 44.0),
            ),
        ));
    }
    nodes.push((CODEX_DISCLOSURE, disclosure));

    if phase == CodexPhase::AwaitingApproval
        && let Some(code) = state.device_code()
    {
        group_children.extend([CODEX_DEVICE_CODE, CODEX_COPY_CODE, CODEX_OPEN_PAGE]);
        nodes.push((
            CODEX_DEVICE_CODE,
            semantic_node(
                Role::Code,
                code,
                logical_rect(x + 252.0, y + 82.0, 180.0, 44.0),
            ),
        ));
        nodes.push((
            CODEX_COPY_CODE,
            interactive_node(
                Role::Button,
                i18n::t(Key::SettingsCopyDeviceCode),
                logical_rect(x + 440.0, y + 82.0, 100.0, 44.0),
            ),
        ));
        nodes.push((
            CODEX_OPEN_PAGE,
            interactive_node(
                Role::Button,
                i18n::t(Key::SettingsOpenDevicePage),
                logical_rect(x + 548.0, y + 82.0, (width - 564.0).max(100.0), 44.0),
            ),
        ));
    }

    group.set_children(group_children);
    nodes.push((CODEX_GROUP, group));
    children.push(CODEX_GROUP);
}

fn settings_permission_nodes(
    state: &SettingsState,
    x: f64,
    width: f64,
    nodes: &mut Vec<(NodeId, Node)>,
    children: &mut Vec<NodeId>,
) {
    for (index, row) in state.permissions.iter().enumerate() {
        let id = permission_node_id(row.permission);
        let mut node = interactive_node(
            Role::Button,
            i18n::t(settings::permission_key(row.permission)),
            logical_rect(x, 68.0 + index as f64 * 56.0, width, 48.0),
        );
        node.set_description(permission_status_label(row.status));
        nodes.push((id, node));
        children.push(id);
    }
}

fn settings_history_nodes(
    state: &SettingsState,
    x: f64,
    width: f64,
    nodes: &mut Vec<(NodeId, Node)>,
    children: &mut Vec<NodeId>,
) {
    let label = if state.recovery_code.is_some() {
        i18n::t(Key::SettingsRecoveryTitle)
    } else if state.history_ready {
        i18n::t(Key::SettingsHistoryReady)
    } else if state.history_failed {
        i18n::t(Key::SettingsHistoryFailed)
    } else {
        i18n::t(Key::SettingsHistorySetupTitle)
    };
    nodes.push((
        SETTINGS_STATUS,
        semantic_node(
            if state.history_failed {
                Role::Alert
            } else {
                Role::Status
            },
            label,
            logical_rect(x, 68.0, width, 72.0),
        ),
    ));
    children.push(SETTINGS_STATUS);

    let action_label = if state.recovery_code.is_some() {
        Some(i18n::t(Key::ActionCopyRecoveryCode))
    } else if !state.history_ready && !state.history_initializing {
        Some(i18n::t(Key::ActionEnableHistory))
    } else {
        None
    };
    if let Some(label) = action_label {
        nodes.push((
            SETTINGS_PRIMARY_ACTION,
            interactive_node(
                Role::DefaultButton,
                label,
                logical_rect(x, 152.0, 240.0, 44.0),
            ),
        ));
        children.push(SETTINGS_PRIMARY_ACTION);
    }
}

/// Build the transient panel semantic tree.
pub fn panel_tree(state: &PanelState, size: (f32, f32), scale: f32, focus: NodeId) -> TreeUpdate {
    let width = size.0.max(420.0);
    let height = size.1.max(132.0);
    let mut children = vec![PANEL_INPUT];
    let mut nodes = Vec::new();

    let mut root = semantic_node(
        Role::Window,
        i18n::t(Key::AppName),
        logical_rect(0.0, 0.0, width, height),
    );
    root.set_transform(Affine::scale(f64::from(scale.max(0.5))));

    if !state.model_options.is_empty() {
        let mut model = semantic_node(
            Role::ComboBox,
            i18n::t(Key::PanelModel),
            logical_rect(f64::from(width) - 246.0, 8.0, 230.0, 40.0),
        );
        if let Some(selected) = &state.selected_model {
            model.set_value(selected.to_string());
        }
        model.set_description(
            state
                .model_options
                .iter()
                .map(|option| option.display_name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        model.add_action(Action::Focus);
        model.add_action(Action::SetValue);
        nodes.push((PANEL_MODEL, model));
        children.insert(0, PANEL_MODEL);
    }

    let mut input = semantic_node(
        Role::TextInput,
        i18n::t(Key::PanelPlaceholder),
        logical_rect(16.0, 48.0, f64::from(width) - 32.0, 44.0),
    );
    input.set_value(state.input.as_str());
    input.add_action(Action::Focus);
    input.add_action(Action::SetValue);
    nodes.push((PANEL_INPUT, input));

    if !state.response.is_empty() {
        let mut response = semantic_node(
            Role::Document,
            i18n::t(Key::PanelResponse),
            logical_rect(
                16.0,
                100.0,
                f64::from(width) - 32.0,
                (f64::from(height) - 160.0).max(72.0),
            ),
        );
        response.set_value(state.response.as_str());
        if matches!(state.phase, Phase::Loading | Phase::Streaming) {
            response.set_live(Live::Polite);
        }
        nodes.push((PANEL_RESPONSE, response));
        children.push(PANEL_RESPONSE);
    }

    let action_y = (f64::from(height) - 52.0).max(96.0);
    if state.handed_off_to_task {
        nodes.push((
            PANEL_SHOW_TASK,
            interactive_node(
                Role::Button,
                i18n::t(Key::ActionShowTask),
                logical_rect(16.0, action_y, 130.0, 44.0),
            ),
        ));
        children.push(PANEL_SHOW_TASK);
    }
    let mut accept = interactive_node(
        Role::DefaultButton,
        i18n::t(Key::ActionReplace),
        logical_rect(156.0, action_y, 120.0, 44.0),
    );
    if !state.can_accept() {
        accept.set_disabled();
    }
    nodes.push((PANEL_ACCEPT, accept));
    children.push(PANEL_ACCEPT);

    let mut copy = interactive_node(
        Role::Button,
        i18n::t(Key::ActionCopy),
        logical_rect(284.0, action_y, 100.0, 44.0),
    );
    if !state.can_copy() {
        copy.set_disabled();
    }
    nodes.push((PANEL_COPY, copy));
    children.push(PANEL_COPY);

    nodes.push((
        PANEL_DISMISS,
        interactive_node(
            Role::Button,
            if matches!(state.phase, Phase::Loading | Phase::Streaming) {
                i18n::t(Key::ActionCancel)
            } else {
                i18n::t(Key::ActionDismiss)
            },
            logical_rect(f64::from(width) - 116.0, action_y, 100.0, 44.0),
        ),
    ));
    children.push(PANEL_DISMISS);

    root.set_children(children);
    nodes.push((PANEL_ROOT, root));
    full_tree(nodes, PANEL_ROOT, focus)
}

/// Build one agent task window's semantic tree.
pub fn task_tree(state: &TaskState, size: (f32, f32), scale: f32, focus: NodeId) -> TreeUpdate {
    let width = size.0.max(480.0);
    let height = size.1.max(360.0);
    let mut nodes = Vec::new();
    let mut children = vec![TASK_TRANSCRIPT];
    let mut root = semantic_node(
        Role::Window,
        i18n::t(Key::TaskWindowTitle),
        logical_rect(0.0, 0.0, width, height),
    );
    root.set_transform(Affine::scale(f64::from(scale.max(0.5))));

    let mut transcript = semantic_node(
        Role::Document,
        i18n::t(Key::TaskWindowTitle),
        logical_rect(
            20.0,
            20.0,
            f64::from(width) - 40.0,
            f64::from(height) - 112.0,
        ),
    );
    transcript.set_value(state.transcript());
    if state.is_running() {
        transcript.set_live(Live::Polite);
    }
    nodes.push((TASK_TRANSCRIPT, transcript));

    if let Some(approval) = &state.pending_approval {
        if approval.requires_typed_confirmation {
            let mut confirmation = semantic_node(
                Role::TextInput,
                i18n::t(Key::TaskApprovalProvenance),
                logical_rect(
                    20.0,
                    f64::from(height) - 140.0,
                    f64::from(width) - 40.0,
                    44.0,
                ),
            );
            confirmation.set_value(state.typed_confirmation.as_str());
            confirmation.add_action(Action::Focus);
            confirmation.add_action(Action::SetValue);
            nodes.push((TASK_CONFIRMATION, confirmation));
            children.push(TASK_CONFIRMATION);
        }

        let mut approve = interactive_node(
            Role::DefaultButton,
            i18n::t(Key::ActionApprove),
            logical_rect(20.0, f64::from(height) - 76.0, 130.0, 44.0),
        );
        if !state.decision_is_ready(ApprovalDecision::Approve) {
            approve.set_disabled();
        }
        nodes.push((TASK_APPROVE, approve));
        children.push(TASK_APPROVE);

        if !approval.requires_typed_confirmation {
            let mut approve_session = interactive_node(
                Role::Button,
                i18n::t(Key::ActionApproveSession),
                logical_rect(158.0, f64::from(height) - 76.0, 170.0, 44.0),
            );
            if !state.decision_is_ready(ApprovalDecision::ApproveForSession) {
                approve_session.set_disabled();
            }
            nodes.push((TASK_APPROVE_SESSION, approve_session));
            children.push(TASK_APPROVE_SESSION);
        }
        nodes.push((
            TASK_DENY,
            interactive_node(
                Role::Button,
                i18n::t(Key::ActionDeny),
                logical_rect(
                    f64::from(width) - 126.0,
                    f64::from(height) - 76.0,
                    106.0,
                    44.0,
                ),
            ),
        ));
        children.push(TASK_DENY);
    } else if state.is_running() {
        nodes.push((
            TASK_CANCEL,
            interactive_node(
                Role::Button,
                i18n::t(Key::ActionCancel),
                logical_rect(20.0, f64::from(height) - 76.0, 120.0, 44.0),
            ),
        ));
        children.push(TASK_CANCEL);
    } else {
        nodes.push((
            TASK_COPY,
            interactive_node(
                Role::Button,
                i18n::t(Key::ActionCopy),
                logical_rect(20.0, f64::from(height) - 76.0, 120.0, 44.0),
            ),
        ));
        nodes.push((
            TASK_CLOSE,
            interactive_node(
                Role::Button,
                i18n::t(Key::ActionDismiss),
                logical_rect(
                    f64::from(width) - 126.0,
                    f64::from(height) - 76.0,
                    106.0,
                    44.0,
                ),
            ),
        ));
        children.extend([TASK_COPY, TASK_CLOSE]);
    }

    root.set_children(children);
    nodes.push((TASK_ROOT, root));
    full_tree(nodes, TASK_ROOT, focus)
}

/// Translate a native Settings action to the ordinary Settings message path.
pub fn settings_message(
    state: &SettingsState,
    request: &ActionRequest,
) -> Option<settings::Message> {
    if request.action != Action::Click {
        return None;
    }
    if let Some(section) = section_for_node(request.target_node) {
        return Some(settings::Message::Select(section));
    }
    match request.target_node {
        CODEX_ACTION => Some(settings::Message::SignIn(ProviderId::CODEX)),
        CODEX_DISCLOSURE => Some(settings::Message::ToggleCodexDetails),
        CODEX_COPY_CODE => state
            .device_code()
            .map(|code| settings::Message::CopyDeviceCode(code.to_owned())),
        CODEX_OPEN_PAGE => Some(settings::Message::OpenDeviceUrl),
        SETTINGS_PRIMARY_ACTION if state.section == Section::History => {
            if state.recovery_code.is_some() {
                Some(settings::Message::CopyRecoveryCode)
            } else if !state.history_ready && !state.history_initializing {
                Some(settings::Message::InitializeHistory)
            } else {
                None
            }
        }
        SETTINGS_PRIMARY_ACTION if state.section == Section::About => {
            Some(settings::Message::CopyDiagnostics)
        }
        node => permission_for_node(node)
            .map(settings::Message::OpenSystemSettings)
            .or_else(|| language_for_node(node).map(settings::Message::SetLanguage)),
    }
}

/// Translate a native panel action to the ordinary panel message path.
pub fn panel_message(state: &PanelState, request: &ActionRequest) -> Option<panel::Message> {
    match (request.target_node, request.action) {
        (PANEL_INPUT, Action::SetValue) => action_value(request).map(panel::Message::InputChanged),
        (PANEL_MODEL, Action::SetValue) => {
            let value = action_value(request)?;
            state
                .model_options
                .iter()
                .find(|option| {
                    option.binding.model == value
                        || option.display_name == value
                        || option.to_string() == value
                })
                .cloned()
                .map(panel::Message::SelectModel)
        }
        (PANEL_ACCEPT, Action::Click) => Some(panel::Message::Accept),
        (PANEL_COPY, Action::Click) => Some(panel::Message::Copy),
        (PANEL_DISMISS, Action::Click) => Some(panel::Message::Dismiss),
        (PANEL_SHOW_TASK, Action::Click) => Some(panel::Message::ShowTask),
        _ => None,
    }
}

/// Translate a native task action to the ordinary task-window message path.
pub fn task_message(request: &ActionRequest) -> Option<task_window::Message> {
    match (request.target_node, request.action) {
        (TASK_CONFIRMATION, Action::SetValue) => {
            action_value(request).map(task_window::Message::ConfirmationChanged)
        }
        (TASK_APPROVE, Action::Click) => {
            Some(task_window::Message::Decide(ApprovalDecision::Approve))
        }
        (TASK_APPROVE_SESSION, Action::Click) => Some(task_window::Message::Decide(
            ApprovalDecision::ApproveForSession,
        )),
        (TASK_DENY, Action::Click) => Some(task_window::Message::Decide(ApprovalDecision::Deny)),
        (TASK_CANCEL, Action::Click) => Some(task_window::Message::Cancel),
        (TASK_CLOSE, Action::Click) => Some(task_window::Message::Close),
        (TASK_COPY, Action::Click) => Some(task_window::Message::CopyTranscript),
        _ => None,
    }
}

/// Focus requested by an assistive technology.
pub fn requested_focus(request: &ActionRequest) -> Option<NodeId> {
    (request.action == Action::Focus).then_some(request.target_node)
}

fn action_value(request: &ActionRequest) -> Option<String> {
    match request.data.as_ref()? {
        ActionData::Value(value) => Some(value.to_string()),
        _ => None,
    }
}

fn semantic_node(role: Role, label: &str, bounds: Rect) -> Node {
    let mut node = Node::new(role);
    node.set_label(label);
    node.set_bounds(bounds);
    node
}

fn interactive_node(role: Role, label: &str, bounds: Rect) -> Node {
    let mut node = semantic_node(role, label, bounds);
    node.add_action(Action::Click);
    node.add_action(Action::Focus);
    node
}

fn logical_rect(x: f64, y: f64, width: impl Into<f64>, height: impl Into<f64>) -> Rect {
    Rect::from_origin_size((x, y), (width.into().max(0.0), height.into().max(0.0)))
}

fn full_tree(nodes: Vec<(NodeId, Node)>, root: NodeId, focus: NodeId) -> TreeUpdate {
    let focus = if focus.0 != 0 && nodes.iter().any(|(id, _)| *id == focus) {
        focus
    } else {
        root
    };
    TreeUpdate {
        nodes,
        tree: Some(Tree::new(root)),
        tree_id: TreeId::ROOT,
        focus,
    }
}

fn section_node_id(section: Section) -> NodeId {
    NodeId(
        SETTINGS_NAV_BASE
            + match section {
                Section::Providers => 0,
                Section::Roles => 1,
                Section::Budgets => 2,
                Section::Permissions => 3,
                Section::Actions => 4,
                Section::History => 5,
                Section::Language => 6,
                Section::About => 7,
            },
    )
}

fn section_for_node(node: NodeId) -> Option<Section> {
    Section::VISIBLE
        .into_iter()
        .find(|section| section_node_id(*section) == node)
}

fn permission_node_id(permission: Permission) -> NodeId {
    NodeId(
        SETTINGS_PERMISSION_BASE
            + match permission {
                Permission::Accessibility => 0,
                Permission::PostEvents => 1,
                Permission::ElevatedWindowAccess => 2,
                Permission::Notifications => 3,
                Permission::Autostart => 4,
            },
    )
}

fn permission_for_node(node: NodeId) -> Option<Permission> {
    [
        Permission::Accessibility,
        Permission::PostEvents,
        Permission::ElevatedWindowAccess,
        Permission::Notifications,
        Permission::Autostart,
    ]
    .into_iter()
    .find(|permission| permission_node_id(*permission) == node)
}

fn language_node_id(language: Lang) -> NodeId {
    NodeId(
        SETTINGS_LANGUAGE_BASE
            + match language {
                Lang::En => 0,
                Lang::Ja => 1,
            },
    )
}

fn language_for_node(node: NodeId) -> Option<Lang> {
    Lang::ALL
        .iter()
        .copied()
        .find(|language| language_node_id(*language) == node)
}

fn permission_status_label(status: PermissionStatus) -> &'static str {
    match status {
        PermissionStatus::Granted => i18n::t(Key::PermissionGranted),
        PermissionStatus::Denied => i18n::t(Key::PermissionDenied),
        PermissionStatus::NotDetermined => i18n::t(Key::PermissionNotDetermined),
        PermissionStatus::Restricted => i18n::t(Key::PermissionRestricted),
        PermissionStatus::NotApplicable => i18n::t(Key::PermissionNotApplicable),
        PermissionStatus::Revoked => i18n::t(Key::PermissionRevoked),
    }
}

#[cfg(test)]
mod tests {
    use accesskit::{Action, ActionData, ActionRequest, TreeId};

    use super::*;

    fn request(target_node: NodeId, action: Action) -> ActionRequest {
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node,
            data: None,
        }
    }

    #[test]
    fn settings_tree_exposes_only_working_sections_and_primary_provider_action() {
        let mut state = SettingsState::default();
        state.onboarding = true;
        let tree = settings_tree(&state, (880.0, 520.0), 2.0, SETTINGS_ROOT);
        let root = tree
            .nodes
            .iter()
            .find(|(id, _)| *id == SETTINGS_ROOT)
            .map(|(_, node)| node)
            .expect("root");
        assert_eq!(root.role(), Role::Window);
        assert!(tree.nodes.iter().any(|(id, node)| {
            *id == CODEX_ACTION
                && node.role() == Role::DefaultButton
                && node.label() == Some(i18n::t(Key::SettingsCodexSignIn))
        }));
        assert!(!tree.nodes.iter().any(|(id, _)| {
            *id == section_node_id(Section::Roles) || *id == section_node_id(Section::Actions)
        }));
    }

    #[test]
    fn native_settings_clicks_use_the_existing_message_vocabulary() {
        let state = SettingsState::default();
        assert!(matches!(
            settings_message(
                &state,
                &request(section_node_id(Section::History), Action::Click)
            ),
            Some(settings::Message::Select(Section::History))
        ));
        assert!(matches!(
            settings_message(&state, &request(CODEX_ACTION, Action::Click)),
            Some(settings::Message::SignIn(provider)) if provider == ProviderId::CODEX
        ));
    }

    #[test]
    fn stale_focus_falls_back_to_the_surface_root() {
        let mut state = SettingsState::default();
        state.section = Section::History;
        let tree = settings_tree(&state, (880.0, 520.0), 1.0, CODEX_ACTION);
        assert_eq!(tree.focus, SETTINGS_ROOT);
    }

    #[test]
    fn editable_native_values_are_not_logged_or_reinterpreted() {
        let mut request = request(PANEL_INPUT, Action::SetValue);
        request.data = Some(ActionData::Value("ユーザーの指示".into()));
        assert!(matches!(
            panel_message(&PanelState::new(uuid::Uuid::from_u128(1)), &request),
            Some(panel::Message::InputChanged(value)) if value == "ユーザーの指示"
        ));
    }

    #[test]
    fn native_model_values_are_limited_to_offered_choices() {
        let mut state = PanelState::new(uuid::Uuid::from_u128(1));
        let option = crate::bridge::ModelOption {
            binding: aibo_core::types::ModelBinding {
                provider: ProviderId::CODEX,
                model: "gpt-5.6-terra".to_owned(),
            },
            display_name: "GPT-5.6 Terra".to_owned(),
            latency_ms: Some(446),
            released_at: None,
            abilities: Default::default(),
            cost: None,
        };
        state.model_options.push(option.clone());
        state.selected_model = Some(option.clone());

        let tree = panel_tree(&state, (680.0, 240.0), 1.0, PANEL_MODEL);
        assert!(tree.nodes.iter().any(|(id, node)| {
            *id == PANEL_MODEL
                && node.role() == Role::ComboBox
                && node.value() == Some(option.to_string().as_str())
        }));

        let mut accepted = request(PANEL_MODEL, Action::SetValue);
        accepted.data = Some(ActionData::Value("gpt-5.6-terra".into()));
        assert!(matches!(
            panel_message(&state, &accepted),
            Some(panel::Message::SelectModel(selected)) if selected == option
        ));

        let mut refused = request(PANEL_MODEL, Action::SetValue);
        refused.data = Some(ActionData::Value("unoffered-model".into()));
        assert!(panel_message(&state, &refused).is_none());
    }
}
