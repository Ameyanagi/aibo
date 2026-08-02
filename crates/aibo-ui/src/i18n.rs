//! Externalised UI strings, from day one (§9).
//!
//! §9 is explicit that retrofitting i18n across every iced view after fifteen
//! weeks of hardcoded `text("Replace")` is "a mechanical multi-day slog plus a
//! layout redo later", and that it costs nothing to avoid now. So **no view in
//! this crate contains a user-visible string literal**: every one goes through
//! [`t`], [`t1`] or [`t2`].
//!
//! The catalogue is a compile-time `match` rather than a runtime file for three
//! reasons: it cannot desynchronise from the code, it costs no allocation on
//! the hot path (the panel is on a 250 ms first-token budget, §1), and adding a
//! [`Key`] without translating it is a compile error in every language arm.
//!
//! Layout consequences, also from §9: translated strings are longer, so the
//! panel width is a *range* ([`crate::theme::PANEL_WIDTH_MIN`] ..=
//! [`crate::theme::PANEL_WIDTH_MAX`]) and never a fixed 680 pt.
//!
//! RTL/bidi shaping, dead keys and AltGr are renderer- and winit-level concerns
//! tracked by **S10**; this module only guarantees the strings are not welded
//! into the views.

use std::sync::atomic::{AtomicU8, Ordering};

/// A supported UI language.
///
/// Deliberately a closed enum: every variant must have a complete catalogue,
/// which is what makes a missing translation a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Lang {
    /// English (source language).
    #[default]
    En,
    /// Japanese. First non-source language because §16 treats mixed
    /// Latin/CJK rendering as the common case, not the edge case.
    Ja,
}

impl Lang {
    /// Every language the catalogue is complete for.
    pub const ALL: &'static [Lang] = &[Lang::En, Lang::Ja];

    /// BCP-47 tag, for persistence and for the settings picker.
    pub const fn tag(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ja => "ja",
        }
    }

    /// Endonym, shown in the language picker.
    pub const fn endonym(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ja => "日本語",
        }
    }

    /// Parse a BCP-47 tag, matching on the primary subtag only.
    pub fn from_tag(tag: &str) -> Option<Lang> {
        let primary = tag.split(['-', '_']).next().unwrap_or(tag);
        match primary.to_ascii_lowercase().as_str() {
            "en" => Some(Lang::En),
            "ja" => Some(Lang::Ja),
            _ => None,
        }
    }

    fn from_repr(repr: u8) -> Lang {
        match repr {
            1 => Lang::Ja,
            _ => Lang::En,
        }
    }

    const fn repr(self) -> u8 {
        match self {
            Lang::En => 0,
            Lang::Ja => 1,
        }
    }
}

static ACTIVE: AtomicU8 = AtomicU8::new(0);

/// Set the active UI language. Cheap enough to call from `update`.
pub fn set_language(lang: Lang) {
    ACTIVE.store(lang.repr(), Ordering::Relaxed);
}

/// The active UI language.
pub fn language() -> Lang {
    Lang::from_repr(ACTIVE.load(Ordering::Relaxed))
}

/// Every user-visible string in the shell.
///
/// Grouped by surface. Keys are named for their *meaning*, not their English
/// text, so re-wording English never invalidates a translation memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Key {
    // --- product ---------------------------------------------------------
    /// The product name. Never translated, but routed through the catalogue so
    /// views hold no literals at all.
    AppName,

    // --- panel: input ----------------------------------------------------
    /// Placeholder in the empty panel input.
    PanelPlaceholder,
    /// Label for the model selector in the popup panel.
    PanelModel,
    /// Badge: the model accepts images.
    AbilityVision,
    /// Badge: the model exposes a reasoning-effort control.
    AbilityReasoning,
    /// Badge: the model can be given tools.
    AbilityTools,
    /// The quick-pick lane showing every provider.
    LaneAll,
    /// Quick-pick heading above pinned models.
    PickerFavourites,
    /// Quick-pick heading above recently used models.
    PickerRecent,
    /// Quick-pick placeholder.
    PickerPlaceholder,
    /// How many models match: `{}` = count.
    PickerCount,
    /// No model matched the query.
    PickerNoMatch,
    /// Choose the highlighted entry.
    ActionSelect,
    /// Pin or unpin the highlighted model.
    ActionPinModel,
    /// Accessible name for the generated answer surface.
    PanelResponse,
    /// Label above user-authored chat bubbles.
    ChatYou,
    /// Label above assistant-authored chat bubbles.
    ChatAssistant,
    /// Heading on the pinned selected-text context card.
    ContextSelectedText,
    /// Context chip label when the source app is known: `{}` = app name.
    ContextChipFrom,
    /// Context chip when no context could be captured at all.
    ContextChipNone,
    /// The empty panel's one line of invitation (`design.md` §4).
    PanelEmptyInvitation,
    /// Source line while the capture is still in flight.
    ///
    /// `design.md` §4 requires this state to read `no context — reading…` and
    /// then `no context available`, **never blank**. Capture is asynchronous
    /// with a 120 ms deadline (§8), so there is always a moment where the panel
    /// is up and the answer to "where was I?" is not known yet; saying so is
    /// what stops that moment reading as a failure.
    ContextChipReading,
    /// Source line once capture has settled with nothing to show.
    ///
    /// Distinct from [`Key::ContextChipNone`]: this is a capture that finished
    /// and found nothing, not one that never ran. The user can act on the
    /// difference — the first is worth a re-read, the second is not.
    ContextChipUnavailable,

    // --- panel: attachments (§2 vision, §14 cost) -------------------------
    /// Chip label for a clipboard image whose source app is known: `{}` = app.
    AttachmentClipboardFrom,
    /// Chip label for a clipboard image with no known source app.
    AttachmentClipboardLabel,
    /// Chip label for an image created by the crop-and-ask shortcut.
    AttachmentScreenRegion,
    /// Marker on a chip whose pixels were downscaled before sending, so
    /// "why is my screenshot blurry" has an answer (§14).
    AttachmentDownscaled,

    // --- panel: states (§16 "every state, not just the happy path") ------
    /// Shown between submit and the first token.
    StateLoading,
    /// Empty state before the user has typed anything.
    StateEmptyTitle,
    /// Empty state supporting line.
    StateEmptyBody,
    /// Context capture is still running or returned nothing usable.
    StateContextUnavailableTitle,
    /// Context-unavailable supporting line.
    StateContextUnavailableBody,
    /// The OS permission needed to read the focused field is missing.
    StatePermissionDeniedTitle,
    /// Permission-denied supporting line.
    StatePermissionDeniedBody,
    /// The response ended early and must never be auto-inserted (§13).
    StateTruncated,
    /// An IME composition is active, so aibo will neither read nor insert (§9).
    StateImeActive,
    /// Marker on a response the user cancelled.
    StateCancelled,

    // --- panel: footnotes ------------------------------------------------
    /// Silent-fallback footnote: `{}` = substitute provider.
    FootnoteFallback,
    /// Truncated-input marker (§5 middle-out truncation).
    FootnoteInputTruncated,
    /// What the attached images add to this turn — `{}` = estimated tokens.
    ///
    /// §14: BYOK means the user pays for every image, and an attachment is the
    /// one thing on the panel that can multiply a turn's cost without changing
    /// a visible word of the instruction.
    FootnoteImageTokens,

    // --- actions (every one has a key hint, §16) -------------------------
    /// Insert the response into the source app.
    ActionReplace,
    /// Copy the response.
    ActionCopy,
    /// Re-run against the `Smart` role.
    ActionSmartModel,
    /// Begin adding a provider in settings.
    ActionAddProvider,
    /// Save the provider being added.
    ActionSaveProvider,
    /// Forget a configured provider and its credential.
    ActionForgetProvider,
    /// Close the panel.
    ActionDismiss,
    /// Retry the failed request.
    ActionRetry,
    /// Submit a message from the chat composer.
    ActionSend,
    /// Generate the active assistant message again.
    ActionRegenerate,
    /// Expand the selected-text context card.
    ActionExpand,
    /// Collapse the selected-text context card.
    ActionCollapse,
    /// Remove selected text from future chat turns.
    ActionRemoveSelection,
    /// Re-authenticate with a provider.
    ActionSignIn,
    /// Shorten the selection so it fits the context budget.
    ActionTrimSelection,
    /// Open the settings window.
    ActionOpenSettings,
    /// Copy a redacted diagnostics bundle (§13, §19).
    ActionCopyDiagnostics,
    /// Cancel in-flight work.
    ActionCancel,
    /// Approve a tool call once.
    ActionApprove,
    /// Approve a tool call for the rest of the session.
    ActionApproveSession,
    /// Refuse a tool call.
    ActionDeny,
    /// Continue past a budget ceiling (§14).
    ActionContinueAnyway,
    /// Open the OS privacy settings pane.
    ActionOpenSystemSettings,
    /// Show the running agent task.
    ActionShowTask,
    /// Switch the failed binding to a model the provider does accept (§13's
    /// one action for `ModelRejected`).
    ActionSwitchModel,
    /// Attach the image on the clipboard. A deliberate act, never inferred.
    ActionAttachImage,
    /// Detach an image, by key or by clicking its chip.
    ActionRemoveImage,
    /// Enable encrypted history.
    ActionEnableHistory,
    /// Copy the one-time recovery code.
    ActionCopyRecoveryCode,
    /// Second-press label after a destructive Forget was armed.
    ActionConfirmForget,
    /// Momentary label confirming a copy affordance fired (`design.md` §6b).
    ActionCopied,
    /// Drop the conversation and start a fresh session (`⌘N`).
    ActionNewChat,
    /// The `@` finder's search placeholder (§P9+).
    FinderPlaceholder,
    /// Attach the highlighted file from the `@` finder.
    ActionAttachFile,
    /// A picked file's content reached the selection slot: `{}` = name.
    ToastFileAttached,
    /// A picked file could not be read: `{}` = name.
    ToastFileAttachFailed,
    /// Begin push-to-talk dictation (§P9+).
    ActionDictate,
    /// Finish dictation and keep the transcript.
    ActionStopDictation,
    /// Dictation needs an OpenAI key it does not have.
    ToastDictationNoKey,
    /// The microphone could not be opened.
    ToastDictationMicrophone,
    /// The transcription websocket failed.
    ToastDictationConnection,

    // --- error treatments (§13) ------------------------------------------
    /// Secure input mode blocks every read and every synthetic keystroke.
    ///
    /// Distinct from a permission denial, and the distinction is the point:
    /// §8's Accessibility checkbox is already ticked when this fires, so the
    /// copy must not send the user there.
    ErrSecureInput,
    /// `NoProviderConfigured` — the only blocking error.
    ErrNoProvider,
    /// Heading over the add-a-provider form.
    SettingsAddProvider,
    /// Placeholder for a custom endpoint's name.
    ProviderIdPlaceholder,
    /// Placeholder for a custom endpoint's base URL.
    ProviderBaseUrlPlaceholder,
    /// Placeholder for the API-key field.
    ProviderKeyPlaceholder,
    /// The second line of the no-provider state: what to actually do about it.
    ///
    /// `design.md` §6 requires errors to state what happened *and* what to do,
    /// in one sentence each. The headline alone ("No provider configured.") is
    /// a diagnosis with no next step.
    ErrNoProviderBody,
    /// `Auth` — `{}` = provider.
    ErrAuth,
    /// `RateLimited` — `{}` = provider.
    ErrRateLimited,
    /// `Offline`.
    ErrOffline,
    /// `ProviderUnavailable` — `{}` = provider.
    ErrProviderUnavailable,
    /// A 4xx: the provider answered and explained why. `{}` = provider,
    /// `{}` = the provider's own message.
    ///
    /// Distinct from [`Key::ErrProviderUnavailable`], which is for a provider
    /// that genuinely did not answer. Rendering a 400 as "not responding" is
    /// both wrong and unactionable.
    ErrProviderRejected,
    /// `ContextTooLarge`.
    ErrContextTooLarge,
    /// `Timeout`.
    ErrTimeout,
    /// `CaptureFailed` — `{}` = app name.
    ErrCaptureFailed,
    /// `InsertFailed`.
    ErrInsertFailed,
    /// `Sandbox`.
    ErrSandbox,
    /// `AgentBackendMissing` — `{}` = backend name.
    ErrAgentBackendMissing,
    /// `BudgetExceeded`.
    ErrBudgetExceeded,
    /// `ModelRejected` with a known-good replacement — `{}` = refused model id,
    /// `{}` = the id to use instead.
    ErrModelRejectedUse,
    /// `ModelRejected` when nothing is known to work — `{}` = refused model id.
    ErrModelRejected,
    /// `VisionUnsupported` with a bound model and a replacement that can see —
    /// `{}` = the bound model, `{}` = the one to use instead.
    ErrVisionUnsupportedUse,
    /// `VisionUnsupported` with a bound model and nothing to switch to —
    /// `{}` = the bound model. The remaining action is to detach the image.
    ErrVisionUnsupported,
    /// `VisionUnsupported` with no `Vision` chain at all, and a provider worth
    /// naming — `{}` = that provider.
    ErrVisionNoProviderUse,
    /// `VisionUnsupported` with no `Vision` chain and nothing to name.
    ErrVisionNoProvider,
    /// `AttachmentRejected`/`TooMany` — `{}` = the per-request limit.
    ErrAttachmentTooMany,
    /// `AttachmentRejected`/`TooLarge` and `TotalTooLarge` — `{}` = chip label,
    /// empty when the whole set is what overflowed.
    ErrAttachmentTooLarge,
    /// `AttachmentRejected`/`UnsupportedMediaType` and `Empty` — `{}` = label.
    ErrAttachmentUnusable,
    /// `Internal` — never rendered raw (§13).
    ErrInternal,

    // --- toasts (§13 non-blocking) ---------------------------------------
    /// ⌘V found an image the pasteboard would not hand over.
    ToastClipboardImageUnreadable,
    /// The OS crop picker failed rather than being cancelled.
    ToastScreenCaptureFailed,
    /// Startup recovered persisted state after a crash; names the diagnostics
    /// action so the user has a concrete next step if it repeats.
    ToastRecoveredFromCrash,
    /// Confirmation after copying a redacted diagnostics bundle.
    ToastDiagnosticsCopied,
    /// Confirmation after the answer reached the clipboard.
    ToastCopied,

    // --- tray ------------------------------------------------------------
    /// Tray tooltip.
    TrayTooltip,
    /// Tray item: show the panel.
    TrayOpenPanel,
    /// Tray item: show the running task window.
    TrayTasks,
    /// Tray item: open settings.
    TraySettings,
    /// Tray item: quit.
    TrayQuit,
    /// Tray tooltip while an agent run is active.
    TrayBusy,

    // --- hotkey ----------------------------------------------------------
    /// Registration failed — `{}` = combination.
    HotkeyFailedTitle,
    /// Registration failed, supporting line naming the usual culprits (§9).
    HotkeyFailedBody,
    /// macOS 15 rejects some modifier combinations outright (§8).
    HotkeyRejectedByOs,
    /// The way to change the binding while no in-app picker exists (§9).
    HotkeyChangeHint,

    // --- task window (§6) -------------------------------------------------
    /// Task window title.
    TaskWindowTitle,
    /// Header above the step list.
    TaskSteps,
    /// Collapsed reasoning section.
    TaskThinking,
    /// A tool is running — `{}` = tool name.
    TaskRunningTool,
    /// Between tool calls: the model is reading results or writing.
    TaskWaitingModel,
    /// A file diff step.
    TaskFileChanged,
    /// The run is blocked on an approval.
    TaskAwaitingApproval,
    /// The run finished.
    TaskCompleted,
    /// The run was cancelled.
    TaskCancelled,
    /// The run failed.
    TaskFailed,
    /// Empty state before the first step arrives.
    TaskEmpty,
    /// Provenance line above an approval prompt (§5 rule 3).
    TaskApprovalProvenance,

    // --- settings (§16 information architecture) --------------------------
    /// Settings window title.
    SettingsTitle,
    /// Section: providers.
    SettingsProviders,
    /// Section: the model catalogue and its pins.
    SettingsModels,
    /// One line explaining what a pin does, above the model list.
    SettingsModelsHint,
    /// "Files" (§P9+ finder roots).
    SettingsFiles,
    /// What the finder roots govern.
    SettingsFilesHint,
    /// Badge on a default root while none are configured.
    SettingsFilesDefaultBadge,
    /// Placeholder for a new root path.
    SettingsFilesRootPlaceholder,
    /// "Add" a finder root.
    ActionAddRoot,
    /// Return finder roots to the platform defaults.
    ActionResetDefaults,
    /// Syntax hint under the hotkey field.
    SettingsHotkeyHint,
    /// The typed hotkey did not parse.
    SettingsHotkeyInvalid,
    /// Generic apply.
    ActionApply,
    /// §8 AX activation toggle title.
    SettingsAxTitle,
    /// §8 AX activation toggle body, including the restart note.
    SettingsAxBody,
    /// What the budget section governs.
    SettingsBudgetHint,
    /// Monthly ceiling field label.
    SettingsBudgetLimitLabel,
    /// Warn threshold field label.
    SettingsBudgetWarnLabel,
    /// §14 hard stop toggle.
    SettingsBudgetHardStop,
    /// Remove the monthly ceiling.
    ActionRemoveBudget,
    /// Composer toggle: enter agent mode.
    ActionAgentMode,
    /// Composer toggle while agent mode is on.
    ActionAgentModeOn,
    /// Composer placeholder while agent mode is on.
    PanelAgentPlaceholder,
    /// Heading of the ⌘T tasks overlay.
    PanelTasksTitle,

    // --- panel: slash commands & help (owner redesign, 2026-08-02) --------
    /// The footer's help affordance and the help overlay's title.
    ActionHelp,
    /// Help section: keyboard shortcuts.
    HelpHeadingShortcuts,
    /// Help section: slash commands.
    HelpHeadingCommands,
    /// Help row: the global summon hotkey.
    HelpSummon,
    /// Help row: crop a screen region into an attachment.
    HelpCrop,
    /// Help row: ↑/↓ prompt history.
    HelpHistory,
    /// `/help` — show shortcuts and commands.
    CmdHelpDesc,
    /// `/agent` — run the coding agent on the trailing text.
    CmdAgentDesc,
    /// `/new` — start a fresh session.
    CmdNewDesc,
    /// `/model` — open the quick-pick.
    CmdModelDesc,
    /// `/settings` — open the settings window.
    CmdSettingsDesc,
    /// The composer's ＋ button: open the attach menu.
    ActionAttach,
    /// Attach menu row: crop a screen region into the conversation.
    ActionScreenshot,
    /// Marker on the workdir picker's recently-used rows.
    WorkdirRecent,
    /// Placeholder in the workdir picker's filter field.
    WorkdirPlaceholder,
    /// `/cd` — choose the agent's working directory.
    CmdCdDesc,
    /// `/cd` rejected a path that is not a directory: `{}` = what was typed.
    ToastWorkdirInvalid,
    /// `/skill` — invoke a skill by name.
    CmdSkillDesc,
    /// `/skills` — list installed skills.
    CmdSkillsDesc,
    /// Heading of the `/skills` overlay.
    SkillsTitle,
    /// The `/skills` overlay with nothing installed: how to get one.
    SkillsEmpty,
    /// Settings section: dictation source.
    SettingsDictation,
    /// STT choice: automatic.
    SttAuto,
    /// STT auto explanation.
    SttAutoDetail,
    /// STT choice: OpenAI key, realtime.
    SttOpenAi,
    /// STT OpenAI explanation.
    SttOpenAiDetail,
    /// STT choice: ChatGPT plan.
    SttChatGpt,
    /// STT ChatGPT explanation.
    SttChatGptDetail,
    /// Section: roles.
    SettingsRoles,
    /// Section: budgets.
    SettingsBudgets,
    /// Section: permissions.
    SettingsPermissions,
    /// Section: actions.
    SettingsActions,
    /// Section: history.
    SettingsHistory,
    /// Section: about and licence.
    SettingsAbout,
    /// Section: language.
    SettingsLanguage,
    /// Section: appearance.
    SettingsAppearance,
    /// Appearance choice: follow the OS.
    AppearanceSystem,
    /// Appearance choice: always dark.
    AppearanceDark,
    /// Appearance choice: always light.
    AppearanceLight,
    /// First-run setup heading.
    SettingsWelcomeTitle,
    /// First-run setup explanation.
    SettingsWelcomeBody,
    /// First onboarding step: connect a provider.
    SettingsSetupConnect,
    /// Second onboarding step: review OS permissions.
    SettingsSetupPermissions,
    /// Third onboarding step: invoke the panel once.
    SettingsSetupTryHotkey,
    /// Encrypted history has not been enabled.
    SettingsHistorySetupTitle,
    /// Explanation of encrypted history and recovery.
    SettingsHistorySetupBody,
    /// Encrypted history is ready.
    SettingsHistoryReady,
    /// A newly generated recovery code must be saved now.
    SettingsRecoveryTitle,
    /// Recovery-code handling guidance.
    SettingsRecoveryBody,
    /// History setup failed.
    SettingsHistoryFailed,
    /// Codex provider-card title.
    SettingsCodexTitle,
    /// Codex provider-card body before sign-in.
    SettingsCodexSignedOut,
    /// Codex provider-card body after sign-in.
    SettingsCodexSignedIn,
    /// Codex device-code request is starting.
    SettingsCodexStarting,
    /// Codex device-code request is waiting for approval.
    SettingsCodexAwaitingApproval,
    /// Codex authorization code is being exchanged for tokens.
    SettingsCodexExchanging,
    /// Generic Codex sign-in failure when no detail is available.
    SettingsCodexFailed,
    /// Start Codex device-code sign-in.
    SettingsCodexSignIn,
    /// Cancel an in-flight Codex sign-in.
    SettingsCodexCancelSignIn,
    /// Forget Codex tokens and remove the provider.
    SettingsCodexSignOut,
    /// Codex consent/storage posture note.
    SettingsCodexConsentNote,
    /// Expand or collapse the Codex sign-in security detail.
    SettingsCodexHowSignInWorks,
    /// Copy a Codex device code.
    SettingsCopyDeviceCode,
    /// Open the Codex device approval page.
    SettingsOpenDevicePage,
    /// OS permission: Accessibility.
    SettingsPermissionAccessibility,
    /// OS permission: Input Monitoring.
    SettingsPermissionInputMonitoring,
    /// OS permission: elevated-window access.
    SettingsPermissionElevatedWindowAccess,
    /// OS permission: notifications.
    SettingsPermissionNotifications,
    /// OS permission: launch at login.
    SettingsPermissionAutostart,

    // --- misc -------------------------------------------------------------
    /// Spend meter label — `{}` = formatted amount.
    SpendThisMonth,
    /// Permission banner: granted.
    PermissionGranted,
    /// Permission banner: denied.
    PermissionDenied,
    /// Permission banner: never asked.
    PermissionNotDetermined,
    /// Permission banner: blocked by device policy.
    PermissionRestricted,
    /// Permission banner: meaningless on this platform.
    PermissionNotApplicable,
    /// Permission banner: revoked after an update (§17).
    PermissionRevoked,
}

/// Look up `key` in the active language.
pub fn t(key: Key) -> &'static str {
    lookup(key, language())
}

/// Look up `key` and substitute a single `{}` placeholder.
pub fn t1(key: Key, arg: &str) -> String {
    let template = t(key);
    match template.find("{}") {
        Some(at) => {
            let mut out = String::with_capacity(template.len() + arg.len());
            out.push_str(&template[..at]);
            out.push_str(arg);
            out.push_str(&template[at + 2..]);
            out
        }
        None => template.to_owned(),
    }
}

/// Look up `key` and substitute two `{}` placeholders, left to right.
///
/// The second one arrived with §13's `ModelRejected` sentence, which has to
/// name both the model that was refused and the one to use instead — a single
/// sentence naming only one of them is not actionable. Still a two-placeholder
/// positional substitution rather than a full ICU message formatter, per the
/// note this replaces.
///
/// Placeholders beyond the ones the template actually contains are dropped, so
/// a translation that legitimately needs only one still renders.
pub fn t2(key: Key, first: &str, second: &str) -> String {
    let template = t(key);
    let mut out = String::with_capacity(template.len() + first.len() + second.len());
    let mut rest = template;
    for arg in [first, second] {
        let Some(at) = rest.find("{}") else { break };
        out.push_str(&rest[..at]);
        out.push_str(arg);
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

/// Look up `key` in an explicit language, ignoring the active one.
pub fn lookup(key: Key, lang: Lang) -> &'static str {
    match lang {
        Lang::En => en(key),
        Lang::Ja => ja(key),
    }
}

fn en(key: Key) -> &'static str {
    use Key as K;
    match key {
        K::AppName => "aibo",

        K::PanelPlaceholder => "Ask about the selection or reply…",
        K::PanelModel => "Model",
        K::AbilityVision => "vision",
        K::AbilityReasoning => "reasoning",
        K::AbilityTools => "tools",
        K::LaneAll => "all",
        K::PickerFavourites => "pinned",
        K::PickerRecent => "recent",
        K::PickerPlaceholder => "search models, or \u{21e5} to browse",
        K::PickerCount => "{} models",
        K::PickerNoMatch => "no model matches",
        K::ActionSelect => "Select",
        K::ActionPinModel => "Pin",
        K::PanelResponse => "Response",
        K::ChatYou => "You",
        K::ChatAssistant => "Aibo",
        K::ContextSelectedText => "Selected text",
        K::ContextChipFrom => "{}",
        K::ContextChipNone => "No context",
        K::PanelEmptyInvitation => "ask, or ⇥ for models",
        K::ContextChipReading => "no context — reading…",
        K::ContextChipUnavailable => "no context available",

        K::AttachmentClipboardFrom => "Image from {}",
        K::AttachmentClipboardLabel => "Clipboard image",
        K::AttachmentScreenRegion => "Screen region",
        K::AttachmentDownscaled => "resized",

        K::StateLoading => "Thinking…",
        K::StateEmptyTitle => "Nothing to show yet",
        K::StateEmptyBody => "Type an instruction, or press ↩ to continue where you left off.",
        K::StateContextUnavailableTitle => "No context from this app",
        K::StateContextUnavailableBody => {
            "aibo could not read the focused field, so it will work from what you type."
        }
        K::StatePermissionDeniedTitle => "aibo needs permission to read this app",
        K::StatePermissionDeniedBody => {
            "Grant Accessibility access so aibo can see the field you are typing in."
        }
        K::StateTruncated => "Response ended early — review before using it.",
        K::StateImeActive => "Finish typing to continue.",
        K::StateCancelled => "Cancelled.",

        K::FootnoteFallback => "Answered by {} instead.",
        K::FootnoteInputTruncated => "Input was shortened to fit the context budget.",
        K::FootnoteImageTokens => "Attached images add about {} tokens to this request.",

        K::ActionReplace => "Replace",
        K::ActionCopy => "Copy",
        K::ActionSmartModel => "smart model",
        K::ActionAddProvider => "Add a provider",
        K::ActionSaveProvider => "Save",
        K::ActionForgetProvider => "Forget",
        K::SettingsAddProvider => "Add a provider",
        K::ProviderIdPlaceholder => "Name, e.g. deepseek",
        K::ProviderBaseUrlPlaceholder => "Base URL, e.g. https://api.deepseek.com/v1",
        K::ProviderKeyPlaceholder => "API key",
        K::ActionDismiss => "Dismiss",
        K::ActionRetry => "Retry",
        K::ActionSend => "Send",
        K::ActionRegenerate => "Regenerate",
        K::ActionExpand => "Expand",
        K::ActionCollapse => "Collapse",
        K::ActionRemoveSelection => "Remove",
        K::ActionSignIn => "Sign in",
        K::ActionTrimSelection => "Trim selection",
        K::ActionOpenSettings => "Open settings",
        K::ActionCopyDiagnostics => "Copy diagnostics",
        K::ActionCancel => "Cancel",
        K::ActionApprove => "Approve",
        K::ActionApproveSession => "Approve for session",
        K::ActionDeny => "Deny",
        K::ActionContinueAnyway => "Continue anyway",
        K::ActionOpenSystemSettings => "Open system settings",
        K::ActionShowTask => "Show task",
        K::ActionSwitchModel => "Switch model",
        K::ActionAttachImage => "Attach image",
        K::ActionRemoveImage => "Remove image",
        K::ActionEnableHistory => "Enable encrypted history",
        K::ActionCopyRecoveryCode => "Copy recovery code",
        K::ActionConfirmForget => "Press again to forget",
        K::ActionCopied => "✓ copied",
        K::ActionNewChat => "New chat",
        K::FinderPlaceholder => "search files — romaji finds Japanese names",
        K::ActionAttachFile => "Attach file",
        K::ToastFileAttached => "Attached {}.",
        K::ToastFileAttachFailed => "Could not read {}.",
        K::ActionDictate => "Dictate",
        K::ActionStopDictation => "Stop dictation",
        K::ToastDictationNoKey => "Dictation needs an OpenAI API key. Add one in settings.",
        K::ToastDictationMicrophone => "The microphone could not be started.",
        K::ToastDictationConnection => "Dictation lost its connection.",

        K::ErrSecureInput => "A password field has focus, so nothing can be read.",
        K::ErrNoProvider => "No provider configured.",
        K::ErrNoProviderBody => "Sign in with ChatGPT to start, or add an API key.",
        K::ErrAuth => "Your {} credentials are no longer valid.",
        K::ErrRateLimited => "{} is rate limiting requests.",
        K::ErrOffline => "aibo cannot reach the network.",
        K::ErrProviderUnavailable => "{} is not responding.",
        K::ErrProviderRejected => "{} rejected the request: {}",
        K::ErrContextTooLarge => "The selection is too long for this model.",
        K::ErrTimeout => "The request took too long.",
        K::ErrCaptureFailed => "aibo could not read the text in {}.",
        K::ErrInsertFailed => "aibo could not insert the text — it is still here to copy.",
        K::ErrSandbox => "The sandboxed code did not finish.",
        K::ErrAgentBackendMissing => "The agent backend {} is not installed.",
        K::ErrBudgetExceeded => "This run reached its budget.",
        K::ErrModelRejectedUse => "{} is not available on this account — use {} instead.",
        K::ErrModelRejected => "{} is not available on this account.",
        K::ErrVisionUnsupportedUse => "{} cannot read images — use {} instead.",
        K::ErrVisionUnsupported => "{} cannot read images.",
        K::ErrVisionNoProviderUse => "No model here can read images — sign in to {}.",
        K::ErrVisionNoProvider => "No model here can read images.",
        K::ErrAttachmentTooMany => "aibo sends at most {} images at a time.",
        K::ErrAttachmentTooLarge => "{} is too large to send.",
        K::ErrAttachmentUnusable => "{} is not an image aibo can send.",
        K::ErrInternal => "Something went wrong inside aibo.",

        K::ToastClipboardImageUnreadable => "aibo could not read the image on the clipboard.",
        K::ToastScreenCaptureFailed => "aibo could not capture that screen region.",
        K::ToastRecoveredFromCrash => {
            "aibo recovered from a previous crash. Copy diagnostics if this keeps happening."
        }
        K::ToastDiagnosticsCopied => "Diagnostics copied.",
        K::ToastCopied => "Copied.",

        K::TrayTooltip => "aibo",
        K::TrayOpenPanel => "Open aibo",
        K::TrayTasks => "Tasks",
        K::TraySettings => "Settings…",
        K::TrayQuit => "Quit aibo",
        K::TrayBusy => "aibo — working",

        K::HotkeyFailedTitle => "The shortcut {} is unavailable",
        K::HotkeyFailedBody => "Another app has already claimed it.",
        K::HotkeyRejectedByOs => "macOS does not accept this combination as a global shortcut.",
        K::HotkeyChangeHint => "Set hotkey = \"…\" in config.toml to change it, then restart aibo.",

        K::TaskWindowTitle => "aibo — task",
        K::TaskSteps => "Steps",
        K::TaskThinking => "Thinking",
        K::TaskRunningTool => "Running {}",
        K::TaskWaitingModel => "Waiting for the model",
        K::TaskFileChanged => "Changed",
        K::TaskAwaitingApproval => "Waiting for your approval",
        K::TaskCompleted => "Finished",
        K::TaskCancelled => "Cancelled",
        K::TaskFailed => "Failed",
        K::TaskEmpty => "Starting…",
        K::TaskApprovalProvenance => "Requested by your instruction:",

        K::SettingsTitle => "aibo — settings",
        K::SettingsProviders => "Providers",
        K::SettingsModels => "Models",
        K::SettingsModelsHint => "★ pinned models appear first in the panel's quick-pick.",
        K::SettingsFiles => "Files",
        K::SettingsFilesHint => {
            "The @ finder searches these folders. ~/ means your home directory."
        }
        K::SettingsFilesDefaultBadge => "default",
        K::SettingsFilesRootPlaceholder => "~/Documents",
        K::ActionAddRoot => "Add folder",
        K::ActionResetDefaults => "Reset to defaults",
        K::SettingsHotkeyHint => {
            "Modifiers from control, alt, shift, super joined by +, then a key — e.g. control+alt+Space."
        }
        K::SettingsHotkeyInvalid => {
            "That combination could not be read. Example: control+alt+Space."
        }
        K::ActionApply => "Apply",
        K::SettingsAxTitle => "Read selections from Chrome and Electron apps",
        K::SettingsAxBody => {
            "Turns on those apps' accessibility trees so text can be captured. Can make them resize sluggishly. Takes effect the next time aibo starts."
        }
        K::SettingsBudgetHint => {
            "A soft monthly ceiling on API spend. The meter above shows this month."
        }
        K::SettingsBudgetLimitLabel => "Monthly ceiling (in your billing currency)",
        K::SettingsBudgetWarnLabel => "Warn at (% of ceiling)",
        K::SettingsBudgetHardStop => "Refuse new requests past the ceiling",
        K::ActionRemoveBudget => "Remove ceiling",
        K::ActionAgentMode => "Agent",
        K::ActionAgentModeOn => "✓ Agent",
        K::PanelAgentPlaceholder => "describe a task — the agent asks before it acts",
        K::PanelTasksTitle => "Agent runs",
        K::ActionHelp => "Help",
        K::HelpHeadingShortcuts => "Keyboard",
        K::HelpHeadingCommands => "Commands",
        K::HelpSummon => "Summon or dismiss the panel",
        K::HelpCrop => "Crop a screen region and attach it",
        K::HelpHistory => "Prompt history",
        K::CmdHelpDesc => "Show shortcuts and commands",
        K::CmdAgentDesc => "Run the coding agent on what follows",
        K::CmdNewDesc => "Start a new chat",
        K::CmdModelDesc => "Open the model picker",
        K::CmdSettingsDesc => "Open settings",
        K::ActionAttach => "Attach",
        K::ActionScreenshot => "Capture a screen region",
        K::WorkdirRecent => "recent",
        K::WorkdirPlaceholder => "choose where the agent works",
        K::CmdCdDesc => "Choose the agent's working directory",
        K::ToastWorkdirInvalid => "{} is not a directory",
        K::CmdSkillDesc => "Invoke a skill by name",
        K::CmdSkillsDesc => "List installed skills",
        K::SkillsTitle => "Skills",
        K::SkillsEmpty => "No skills yet. In agent mode, try: make yourself a skill that …",
        K::SettingsDictation => "Dictation",
        K::SttAuto => "Automatic",
        K::SttAutoDetail => "OpenAI key when present, otherwise the ChatGPT plan",
        K::SttOpenAi => "OpenAI API key",
        K::SttOpenAiDetail => "Streaming — words appear as you speak",
        K::SttChatGpt => "ChatGPT plan",
        K::SttChatGptDetail => "Uses the Codex sign-in; the text arrives when you stop",
        K::SettingsRoles => "Roles",
        K::SettingsBudgets => "Budgets",
        K::SettingsPermissions => "Permissions",
        K::SettingsActions => "Actions",
        K::SettingsHistory => "History",
        K::SettingsAbout => "About",
        K::SettingsLanguage => "Language",
        K::SettingsAppearance => "Appearance",
        K::AppearanceSystem => "Match the system",
        K::AppearanceDark => "Dark",
        K::AppearanceLight => "Light",
        K::SettingsWelcomeTitle => "Welcome to aibo",
        K::SettingsWelcomeBody => "Three quick steps, then aibo is ready wherever you write.",
        K::SettingsSetupConnect => "Connect ChatGPT",
        K::SettingsSetupPermissions => "Review permissions",
        K::SettingsSetupTryHotkey => "Try {}",
        K::SettingsHistorySetupTitle => "Encrypted history is off",
        K::SettingsHistorySetupBody => {
            "Enable it to save conversations in a local SQLCipher database. The encryption key stays in your OS credential store."
        }
        K::SettingsHistoryReady => "Encrypted history is ready.",
        K::SettingsRecoveryTitle => "Save this recovery code now",
        K::SettingsRecoveryBody => {
            "aibo shows this code only once. Anyone with it can decrypt your local history."
        }
        K::SettingsHistoryFailed => "aibo could not enable encrypted history.",
        K::SettingsCodexTitle => "ChatGPT subscription (Codex)",
        K::SettingsCodexSignedOut => "Use your ChatGPT plan—no API key required.",
        K::SettingsCodexSignedIn => "Signed in. Codex is bound to the Smart and Ask surfaces.",
        K::SettingsCodexStarting => "Starting ChatGPT sign-in…",
        K::SettingsCodexAwaitingApproval => "Enter the device code on OpenAI's approval page.",
        K::SettingsCodexExchanging => "Approved. Finishing sign-in…",
        K::SettingsCodexFailed => "Sign-in failed. Try again.",
        K::SettingsCodexSignIn => "Sign in with ChatGPT",
        K::SettingsCodexCancelSignIn => "Cancel sign-in",
        K::SettingsCodexSignOut => "Sign out",
        K::SettingsCodexConsentNote => {
            "aibo uses OpenAI's device-code consent page and stores only its own tokens in your OS credential store. It never reads or refreshes tokens owned by the Codex CLI."
        }
        K::SettingsCodexHowSignInWorks => "How sign-in works",
        K::SettingsCopyDeviceCode => "Copy code",
        K::SettingsOpenDevicePage => "Open the page",
        K::SettingsPermissionAccessibility => "Accessibility",
        K::SettingsPermissionInputMonitoring => "Input Monitoring",
        K::SettingsPermissionElevatedWindowAccess => "Elevated window access",
        K::SettingsPermissionNotifications => "Notifications",
        K::SettingsPermissionAutostart => "Launch at login",

        K::SpendThisMonth => "{} this month",
        K::PermissionGranted => "Granted",
        K::PermissionDenied => "Denied",
        K::PermissionNotDetermined => "Not asked yet",
        K::PermissionRestricted => "Restricted by device policy",
        K::PermissionNotApplicable => "Not applicable on this device",
        K::PermissionRevoked => "Permission was removed — grant it again",
    }
}

fn ja(key: Key) -> &'static str {
    use Key as K;
    match key {
        K::AppName => "aibo",

        K::PanelPlaceholder => "選択範囲について質問、または返信…",
        K::PanelModel => "モデル",
        K::AbilityVision => "画像",
        K::AbilityReasoning => "推論",
        K::AbilityTools => "ツール",
        K::LaneAll => "すべて",
        K::PickerFavourites => "ピン留め",
        K::PickerRecent => "最近使用",
        K::PickerPlaceholder => "モデルを検索",
        K::PickerCount => "{} 件",
        K::PickerNoMatch => "一致するモデルがありません",
        K::ActionSelect => "選択",
        K::ActionPinModel => "ピン留め",
        K::PanelResponse => "応答",
        K::ChatYou => "あなた",
        K::ChatAssistant => "Aibo",
        K::ContextSelectedText => "選択したテキスト",
        K::ContextChipFrom => "{}",
        K::ContextChipNone => "コンテキストなし",
        K::PanelEmptyInvitation => "質問するか、⇥ でモデルを選択",
        K::ContextChipReading => "コンテキストを読み取り中…",
        K::ContextChipUnavailable => "コンテキストを取得できません",

        K::AttachmentClipboardFrom => "{} の画像",
        K::AttachmentClipboardLabel => "クリップボードの画像",
        K::AttachmentScreenRegion => "画面の選択範囲",
        K::AttachmentDownscaled => "縮小済み",

        K::StateLoading => "考えています…",
        K::StateEmptyTitle => "まだ表示するものがありません",
        K::StateEmptyBody => "指示を入力するか、↩ で続きから始めます。",
        K::StateContextUnavailableTitle => "このアプリからは読み取れません",
        K::StateContextUnavailableBody => {
            "入力欄を読み取れないため、入力された内容だけで動作します。"
        }
        K::StatePermissionDeniedTitle => "読み取りの許可が必要です",
        K::StatePermissionDeniedBody => "アクセシビリティを許可すると、入力中の欄を認識できます。",
        K::StateTruncated => "応答が途中で終了しました。内容を確認してください。",
        K::StateImeActive => "入力を確定してください。",
        K::StateCancelled => "キャンセルしました。",

        K::FootnoteFallback => "代わりに {} が応答しました。",
        K::FootnoteInputTruncated => "コンテキスト上限に合わせて入力を短縮しました。",
        K::FootnoteImageTokens => "添付画像でおよそ {} トークン増えます。",

        K::ActionReplace => "置換",
        K::ActionCopy => "コピー",
        K::ActionSmartModel => "高性能モデル",
        K::ActionAddProvider => "プロバイダーを追加",
        K::ActionSaveProvider => "保存",
        K::ActionForgetProvider => "削除",
        K::SettingsAddProvider => "プロバイダーを追加",
        K::ProviderIdPlaceholder => "名前（例: deepseek）",
        K::ProviderBaseUrlPlaceholder => "ベース URL（例: https://api.deepseek.com/v1）",
        K::ProviderKeyPlaceholder => "API キー",
        K::ActionDismiss => "閉じる",
        K::ActionRetry => "再試行",
        K::ActionSend => "送信",
        K::ActionRegenerate => "再生成",
        K::ActionExpand => "展開",
        K::ActionCollapse => "折りたたむ",
        K::ActionRemoveSelection => "削除",
        K::ActionSignIn => "サインイン",
        K::ActionTrimSelection => "選択範囲を短縮",
        K::ActionOpenSettings => "設定を開く",
        K::ActionCopyDiagnostics => "診断情報をコピー",
        K::ActionCancel => "キャンセル",
        K::ActionApprove => "許可",
        K::ActionApproveSession => "このセッション中は許可",
        K::ActionDeny => "拒否",
        K::ActionContinueAnyway => "続行",
        K::ActionOpenSystemSettings => "システム設定を開く",
        K::ActionShowTask => "タスクを表示",
        K::ActionSwitchModel => "モデルを切り替える",
        K::ActionAttachImage => "画像を添付",
        K::ActionRemoveImage => "画像を削除",
        K::ActionEnableHistory => "暗号化履歴を有効にする",
        K::ActionCopyRecoveryCode => "復旧コードをコピー",
        K::ActionConfirmForget => "もう一度押すと削除します",
        K::ActionCopied => "✓ コピーしました",
        K::ActionNewChat => "新しいチャット",
        K::FinderPlaceholder => "ファイルを検索 — ローマ字でも探せます",
        K::ActionAttachFile => "ファイルを添付",
        K::ToastFileAttached => "{} を添付しました。",
        K::ToastFileAttachFailed => "{} を読み込めませんでした。",
        K::ActionDictate => "音声入力",
        K::ActionStopDictation => "音声入力を停止",
        K::ToastDictationNoKey => {
            "音声入力には OpenAI API キーが必要です。設定で追加してください。"
        }
        K::ToastDictationMicrophone => "マイクを起動できませんでした。",
        K::ToastDictationConnection => "音声入力の接続が切れました。",

        K::ErrSecureInput => "パスワード入力中のため読み取れません。",
        K::ErrNoProvider => "プロバイダーが未設定です。",
        K::ErrNoProviderBody => "ChatGPT でサインインするか、API キーを追加してください。",
        K::ErrAuth => "{} の認証情報が無効になりました。",
        K::ErrRateLimited => "{} がレート制限中です。",
        K::ErrOffline => "ネットワークに接続できません。",
        K::ErrProviderUnavailable => "{} が応答しません。",
        K::ErrProviderRejected => "{} がリクエストを拒否しました: {}",
        K::ErrContextTooLarge => "選択範囲がこのモデルには長すぎます。",
        K::ErrTimeout => "応答に時間がかかりすぎました。",
        K::ErrCaptureFailed => "{} のテキストを読み取れませんでした。",
        K::ErrInsertFailed => "テキストを挿入できませんでした。ここからコピーできます。",
        K::ErrSandbox => "サンドボックス内のコードが完了しませんでした。",
        K::ErrAgentBackendMissing => "エージェントバックエンド {} がインストールされていません。",
        K::ErrBudgetExceeded => "この実行は上限に達しました。",
        K::ErrModelRejectedUse => {
            "{} はこのアカウントでは利用できません。代わりに {} を使用してください。"
        }
        K::ErrModelRejected => "{} はこのアカウントでは利用できません。",
        K::ErrVisionUnsupportedUse => "{} は画像を読み取れません。代わりに {} を使用してください。",
        K::ErrVisionUnsupported => "{} は画像を読み取れません。",
        K::ErrVisionNoProviderUse => {
            "画像を読み取れるモデルがありません。{} にサインインしてください。"
        }
        K::ErrVisionNoProvider => "画像を読み取れるモデルがありません。",
        K::ErrAttachmentTooMany => "画像は一度に最大 {} 枚までです。",
        K::ErrAttachmentTooLarge => "{} はサイズが大きすぎて送信できません。",
        K::ErrAttachmentUnusable => "{} は aibo が送信できる画像形式ではありません。",
        K::ErrInternal => "aibo の内部で問題が発生しました。",

        K::ToastClipboardImageUnreadable => "クリップボードの画像を読み取れませんでした。",
        K::ToastScreenCaptureFailed => "画面の選択範囲を取り込めませんでした。",
        K::ToastRecoveredFromCrash => {
            "前回のクラッシュから復旧しました。繰り返す場合は診断情報をコピーしてください。"
        }
        K::ToastDiagnosticsCopied => "診断情報をコピーしました。",
        K::ToastCopied => "コピーしました。",

        K::TrayTooltip => "aibo",
        K::TrayOpenPanel => "aibo を開く",
        K::TrayTasks => "タスク",
        K::TraySettings => "設定…",
        K::TrayQuit => "aibo を終了",
        K::TrayBusy => "aibo — 実行中",

        K::HotkeyFailedTitle => "ショートカット {} を使用できません",
        K::HotkeyFailedBody => "他のアプリが使用しています。",
        K::HotkeyRejectedByOs => {
            "macOS はこの組み合わせをグローバルショートカットとして受け付けません。"
        }
        K::HotkeyChangeHint => {
            "変更するには config.toml に hotkey = \"…\" を設定し、aibo を再起動してください。"
        }

        K::TaskWindowTitle => "aibo — タスク",
        K::TaskSteps => "ステップ",
        K::TaskThinking => "思考",
        K::TaskRunningTool => "{} を実行中",
        K::TaskWaitingModel => "モデルの応答を待っています",
        K::TaskFileChanged => "変更",
        K::TaskAwaitingApproval => "承認を待っています",
        K::TaskCompleted => "完了",
        K::TaskCancelled => "キャンセル",
        K::TaskFailed => "失敗",
        K::TaskEmpty => "開始しています…",
        K::TaskApprovalProvenance => "この指示によるものです:",

        K::SettingsTitle => "aibo — 設定",
        K::SettingsProviders => "プロバイダー",
        K::SettingsModels => "モデル",
        K::SettingsModelsHint => "★ を付けたモデルはクイックピックの先頭に表示されます。",
        K::SettingsFiles => "ファイル",
        K::SettingsFilesHint => {
            "@ ファインダーはこれらのフォルダを検索します。~/ はホームディレクトリです。"
        }
        K::SettingsFilesDefaultBadge => "デフォルト",
        K::SettingsFilesRootPlaceholder => "~/Documents",
        K::ActionAddRoot => "フォルダを追加",
        K::ActionResetDefaults => "デフォルトに戻す",
        K::SettingsHotkeyHint => {
            "control・alt・shift・super を + でつなぎ、最後にキーを指定します。例: control+alt+Space"
        }
        K::SettingsHotkeyInvalid => "この組み合わせは読み取れませんでした。例: control+alt+Space",
        K::ActionApply => "適用",
        K::SettingsAxTitle => "Chrome や Electron アプリから選択テキストを読み取る",
        K::SettingsAxBody => {
            "対象アプリのアクセシビリティツリーを有効にしてテキストを取得します。ウィンドウ操作が重くなることがあります。次回の起動時に反映されます。"
        }
        K::SettingsBudgetHint => {
            "API 支出のソフトな月間上限です。上のメーターは今月の支出を示します。"
        }
        K::SettingsBudgetLimitLabel => "月間上限（請求通貨）",
        K::SettingsBudgetWarnLabel => "警告する割合（上限の %）",
        K::SettingsBudgetHardStop => "上限を超えたら新しいリクエストを拒否する",
        K::ActionRemoveBudget => "上限を削除",
        K::ActionAgentMode => "エージェント",
        K::ActionAgentModeOn => "✓ エージェント",
        K::ActionHelp => "ヘルプ",
        K::HelpHeadingShortcuts => "キーボード",
        K::HelpHeadingCommands => "コマンド",
        K::HelpSummon => "パネルを呼び出す・閉じる",
        K::HelpCrop => "画面の範囲を切り取って添付",
        K::HelpHistory => "入力履歴",
        K::CmdHelpDesc => "ショートカットとコマンドを表示",
        K::CmdAgentDesc => "続くテキストをコーディングエージェントで実行",
        K::CmdNewDesc => "新しいチャットを開始",
        K::CmdModelDesc => "モデルピッカーを開く",
        K::CmdSettingsDesc => "設定を開く",
        K::ActionAttach => "添付",
        K::ActionScreenshot => "画面の範囲を撮影",
        K::WorkdirRecent => "最近",
        K::WorkdirPlaceholder => "エージェントの作業ディレクトリを選択",
        K::CmdCdDesc => "エージェントの作業ディレクトリを選択",
        K::ToastWorkdirInvalid => "{} はディレクトリではありません",
        K::CmdSkillDesc => "スキルを名前で実行",
        K::CmdSkillsDesc => "インストール済みスキルを一覧",
        K::SkillsTitle => "スキル",
        K::SkillsEmpty => {
            "スキルはまだありません。エージェントモードで「〜するスキルを作って」と頼んでみてください"
        }
        K::SettingsDictation => "音声入力",
        K::SttAuto => "自動",
        K::SttAutoDetail => "OpenAI キーがあれば使用し、なければ ChatGPT プランを使用",
        K::SttOpenAi => "OpenAI API キー",
        K::SttOpenAiDetail => "ストリーミング — 話すそばから文字になります",
        K::SttChatGpt => "ChatGPT プラン",
        K::SttChatGptDetail => "Codex サインインを使用。停止すると文字が届きます",
        K::PanelAgentPlaceholder => "タスクを入力 — エージェントは実行前に確認します",
        K::PanelTasksTitle => "エージェントの実行",
        K::SettingsRoles => "ロール",
        K::SettingsBudgets => "予算",
        K::SettingsPermissions => "権限",
        K::SettingsActions => "アクション",
        K::SettingsHistory => "履歴",
        K::SettingsAbout => "情報",
        K::SettingsLanguage => "言語",
        K::SettingsAppearance => "外観",
        K::AppearanceSystem => "システムに合わせる",
        K::AppearanceDark => "ダーク",
        K::AppearanceLight => "ライト",
        K::SettingsWelcomeTitle => "aibo へようこそ",
        K::SettingsWelcomeBody => {
            "3 つの簡単な手順で、どこで文章を書いていても aibo を使えるようになります。"
        }
        K::SettingsSetupConnect => "ChatGPT に接続",
        K::SettingsSetupPermissions => "権限を確認",
        K::SettingsSetupTryHotkey => "{} を試す",
        K::SettingsHistorySetupTitle => "暗号化履歴はオフです",
        K::SettingsHistorySetupBody => {
            "会話をローカルの SQLCipher データベースに保存するには有効にしてください。暗号化キーは OS の資格情報ストアに保管されます。"
        }
        K::SettingsHistoryReady => "暗号化履歴を使用できます。",
        K::SettingsRecoveryTitle => "今すぐ復旧コードを保存してください",
        K::SettingsRecoveryBody => {
            "このコードが表示されるのは一度だけです。コードを知る人はローカル履歴を復号できます。"
        }
        K::SettingsHistoryFailed => "暗号化履歴を有効にできませんでした。",
        K::SettingsCodexTitle => "ChatGPT サブスクリプション（Codex）",
        K::SettingsCodexSignedOut => "API キーを使わず、ChatGPT プランで利用できます。",
        K::SettingsCodexSignedIn => {
            "サインイン済みです。Codex は「高性能」と「質問」に割り当てられています。"
        }
        K::SettingsCodexStarting => "ChatGPT へのサインインを開始しています…",
        K::SettingsCodexAwaitingApproval => {
            "OpenAI の承認ページにデバイスコードを入力してください。"
        }
        K::SettingsCodexExchanging => "承認済みです。サインインを完了しています…",
        K::SettingsCodexFailed => "サインインに失敗しました。もう一度お試しください。",
        K::SettingsCodexSignIn => "ChatGPT でサインイン",
        K::SettingsCodexCancelSignIn => "サインインをキャンセル",
        K::SettingsCodexSignOut => "サインアウト",
        K::SettingsCodexConsentNote => {
            "aibo は OpenAI のデバイスコード同意画面を使用し、aibo 専用のトークンだけを OS の資格情報ストアに保存します。Codex CLI のトークンを読み取ったり更新したりすることはありません。"
        }
        K::SettingsCodexHowSignInWorks => "サインインの仕組み",
        K::SettingsCopyDeviceCode => "コードをコピー",
        K::SettingsOpenDevicePage => "承認ページを開く",
        K::SettingsPermissionAccessibility => "アクセシビリティ",
        K::SettingsPermissionInputMonitoring => "入力監視",
        K::SettingsPermissionElevatedWindowAccess => "昇格されたウィンドウへのアクセス",
        K::SettingsPermissionNotifications => "通知",
        K::SettingsPermissionAutostart => "ログイン時に起動",

        K::SpendThisMonth => "今月 {}",
        K::PermissionGranted => "許可済み",
        K::PermissionDenied => "拒否",
        K::PermissionNotDetermined => "未確認",
        K::PermissionRestricted => "デバイスポリシーにより制限",
        K::PermissionNotApplicable => "このデバイスでは対象外",
        K::PermissionRevoked => "権限が取り消されました — 再度許可してください",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_substitution() {
        set_language(Lang::En);
        assert_eq!(
            t1(Key::FootnoteFallback, "groq"),
            "Answered by groq instead."
        );
        // A key with no placeholder must not be corrupted by an argument.
        assert_eq!(t1(Key::ActionCopy, "ignored"), "Copy");
    }

    #[test]
    fn two_placeholders_substitute_left_to_right() {
        set_language(Lang::En);
        assert_eq!(
            t2(Key::ErrModelRejectedUse, "gpt-5-codex", "gpt-5.5"),
            "gpt-5-codex is not available on this account — use gpt-5.5 instead."
        );
        // Both languages must keep both placeholders: dropping the second is
        // how the sentence stops naming a model that works.
        for lang in Lang::ALL {
            assert_eq!(
                lookup(Key::ErrModelRejectedUse, *lang)
                    .matches("{}")
                    .count(),
                2,
                "{lang:?}"
            );
        }
        // Fewer placeholders than arguments must not panic or duplicate.
        assert_eq!(
            t2(Key::ErrModelRejected, "a", "b"),
            "a is not available on this account."
        );
        assert_eq!(t2(Key::ActionCopy, "a", "b"), "Copy");
    }

    /// §9: every new key ships translated, or the Japanese market gets an
    /// English panel. The attachment work added a whole surface at once, which
    /// is exactly when a language is quietly left behind.
    #[test]
    fn the_attachment_strings_ship_in_every_language() {
        const ADDED: &[Key] = &[
            Key::AttachmentClipboardFrom,
            Key::AttachmentClipboardLabel,
            Key::AttachmentDownscaled,
            Key::FootnoteImageTokens,
            Key::ActionAttachImage,
            Key::ActionRemoveImage,
            Key::ErrVisionUnsupportedUse,
            Key::ErrVisionUnsupported,
            Key::ErrVisionNoProviderUse,
            Key::ErrVisionNoProvider,
            Key::ErrAttachmentTooMany,
            Key::ErrAttachmentTooLarge,
            Key::ErrAttachmentUnusable,
            Key::ToastClipboardImageUnreadable,
            // The settings-coverage work (§8, §9, §14, §P9+): every knob the
            // window gained ships in both languages.
            Key::SettingsFiles,
            Key::SettingsFilesHint,
            Key::SettingsFilesDefaultBadge,
            Key::ActionAddRoot,
            Key::ActionResetDefaults,
            Key::SettingsHotkeyHint,
            Key::SettingsHotkeyInvalid,
            Key::ActionApply,
            Key::SettingsAxTitle,
            Key::SettingsAxBody,
            Key::SettingsBudgetHint,
            Key::SettingsBudgetLimitLabel,
            Key::SettingsBudgetWarnLabel,
            Key::SettingsBudgetHardStop,
            Key::ActionRemoveBudget,
            Key::ActionAgentMode,
            Key::ActionAgentModeOn,
            Key::PanelAgentPlaceholder,
            Key::PanelTasksTitle,
        ];
        for key in ADDED {
            for lang in Lang::ALL {
                assert!(!lookup(*key, *lang).is_empty(), "{key:?} / {lang:?}");
            }
            assert_ne!(
                lookup(*key, Lang::En),
                lookup(*key, Lang::Ja),
                "{key:?} was left untranslated"
            );
        }
    }

    #[test]
    fn the_settings_strings_ship_in_every_language() {
        const SETTINGS: &[Key] = &[
            Key::SettingsCodexTitle,
            Key::SettingsCodexSignedOut,
            Key::SettingsCodexSignedIn,
            Key::SettingsCodexStarting,
            Key::SettingsCodexAwaitingApproval,
            Key::SettingsCodexExchanging,
            Key::SettingsCodexFailed,
            Key::SettingsCodexSignIn,
            Key::SettingsCodexCancelSignIn,
            Key::SettingsCodexSignOut,
            Key::SettingsCodexConsentNote,
            Key::SettingsCodexHowSignInWorks,
            Key::SettingsSetupConnect,
            Key::SettingsSetupPermissions,
            Key::SettingsSetupTryHotkey,
            Key::SettingsCopyDeviceCode,
            Key::SettingsOpenDevicePage,
            Key::SettingsPermissionAccessibility,
            Key::SettingsPermissionInputMonitoring,
            Key::SettingsPermissionElevatedWindowAccess,
            Key::SettingsPermissionNotifications,
            Key::SettingsPermissionAutostart,
            Key::PermissionRestricted,
            Key::PermissionNotApplicable,
            Key::PanelModel,
            Key::PanelResponse,
            Key::SettingsHistorySetupTitle,
            Key::SettingsHistorySetupBody,
            Key::SettingsHistoryReady,
            Key::SettingsRecoveryTitle,
            Key::SettingsRecoveryBody,
            Key::SettingsHistoryFailed,
            Key::ActionEnableHistory,
            Key::ActionCopyRecoveryCode,
            Key::ActionConfirmForget,
            Key::ActionCopied,
            Key::ActionNewChat,
            Key::HotkeyChangeHint,
            Key::SettingsModels,
            Key::SettingsModelsHint,
            Key::ActionDictate,
            Key::ActionStopDictation,
            Key::ToastDictationNoKey,
            Key::ToastDictationMicrophone,
            Key::ToastDictationConnection,
            Key::FinderPlaceholder,
            Key::ActionAttachFile,
            Key::ToastFileAttached,
            Key::ToastFileAttachFailed,
        ];
        for key in SETTINGS {
            assert!(!lookup(*key, Lang::En).is_empty(), "{key:?} / en");
            assert!(!lookup(*key, Lang::Ja).is_empty(), "{key:?} / ja");
            assert_ne!(
                lookup(*key, Lang::En),
                lookup(*key, Lang::Ja),
                "{key:?} was left untranslated"
            );
        }
    }

    #[test]
    fn recovery_toasts_ship_in_every_language() {
        for key in [
            Key::ToastRecoveredFromCrash,
            Key::ToastDiagnosticsCopied,
            Key::ToastCopied,
        ] {
            assert!(!lookup(key, Lang::En).is_empty());
            assert!(!lookup(key, Lang::Ja).is_empty());
            assert_ne!(lookup(key, Lang::En), lookup(key, Lang::Ja));
        }
        assert!(
            lookup(Key::ToastRecoveredFromCrash, Lang::En)
                .contains(lookup(Key::ActionCopyDiagnostics, Lang::En))
        );
        assert!(
            lookup(Key::ToastRecoveredFromCrash, Lang::Ja)
                .contains(lookup(Key::ActionCopyDiagnostics, Lang::Ja))
        );
    }

    /// The vision refusal is §13's one sentence, and it is only actionable if
    /// it names both the model that cannot see and one that can. A translation
    /// that drops the second placeholder silently removes the way out.
    #[test]
    fn the_vision_refusal_names_both_models_in_every_language() {
        for lang in Lang::ALL {
            assert_eq!(
                lookup(Key::ErrVisionUnsupportedUse, *lang)
                    .matches("{}")
                    .count(),
                2,
                "{lang:?}"
            );
        }
        for key in [
            Key::ErrVisionUnsupported,
            Key::ErrVisionNoProviderUse,
            Key::ErrAttachmentTooMany,
            Key::ErrAttachmentTooLarge,
            Key::ErrAttachmentUnusable,
            Key::FootnoteImageTokens,
            Key::AttachmentClipboardFrom,
        ] {
            for lang in Lang::ALL {
                assert_eq!(
                    lookup(key, *lang).matches("{}").count(),
                    1,
                    "{key:?} / {lang:?}"
                );
            }
        }
        // These carry no argument, and a stray placeholder would render as
        // literal braces in the panel.
        for key in [Key::ErrVisionNoProvider, Key::ToastClipboardImageUnreadable] {
            for lang in Lang::ALL {
                assert_eq!(lookup(key, *lang).matches("{}").count(), 0, "{key:?}");
            }
        }
    }

    #[test]
    fn tags_round_trip_and_ignore_region() {
        assert_eq!(Lang::from_tag("ja-JP"), Some(Lang::Ja));
        assert_eq!(Lang::from_tag("en_US"), Some(Lang::En));
        assert_eq!(Lang::from_tag("de"), None);
        for lang in Lang::ALL {
            assert_eq!(Lang::from_tag(lang.tag()), Some(*lang));
        }
    }

    #[test]
    fn sampled_keys_are_translated_in_every_language() {
        const SAMPLE: &[Key] = &[
            Key::PanelPlaceholder,
            Key::StateLoading,
            Key::ActionReplace,
            Key::ErrOffline,
            Key::TrayQuit,
            Key::TaskAwaitingApproval,
            Key::SettingsProviders,
            Key::SettingsWelcomeTitle,
            Key::SettingsWelcomeBody,
        ];
        for key in SAMPLE {
            for lang in Lang::ALL {
                assert!(!lookup(*key, *lang).is_empty(), "{key:?} / {lang:?}");
            }
            assert_ne!(lookup(*key, Lang::En), lookup(*key, Lang::Ja), "{key:?}");
        }
    }
}
