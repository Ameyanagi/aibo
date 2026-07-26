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
    /// Context chip label when the source app is known: `{}` = app name.
    ContextChipFrom,
    /// Context chip when no context could be captured at all.
    ContextChipNone,

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

    // --- actions (every one has a key hint, §16) -------------------------
    /// Insert the response into the source app.
    ActionReplace,
    /// Copy the response.
    ActionCopy,
    /// Re-run against the `Smart` role.
    ActionSmartModel,
    /// Close the panel.
    ActionDismiss,
    /// Retry the failed request.
    ActionRetry,
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

    // --- error treatments (§13) ------------------------------------------
    /// `NoProviderConfigured` — the only blocking error.
    ErrNoProvider,
    /// `Auth` — `{}` = provider.
    ErrAuth,
    /// `RateLimited` — `{}` = provider.
    ErrRateLimited,
    /// `Offline`.
    ErrOffline,
    /// `ProviderUnavailable` — `{}` = provider.
    ErrProviderUnavailable,
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
    /// `Internal` — never rendered raw (§13).
    ErrInternal,

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

    // --- task window (§6) -------------------------------------------------
    /// Task window title.
    TaskWindowTitle,
    /// Header above the step list.
    TaskSteps,
    /// Collapsed reasoning section.
    TaskThinking,
    /// A tool is running — `{}` = tool name.
    TaskRunningTool,
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

    // --- misc -------------------------------------------------------------
    /// Spend meter label — `{}` = formatted amount.
    SpendThisMonth,
    /// Permission banner: granted.
    PermissionGranted,
    /// Permission banner: denied.
    PermissionDenied,
    /// Permission banner: never asked.
    PermissionNotDetermined,
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

        K::PanelPlaceholder => "Ask, transform, or compute…",
        K::ContextChipFrom => "{}",
        K::ContextChipNone => "No context",

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

        K::ActionReplace => "Replace",
        K::ActionCopy => "Copy",
        K::ActionSmartModel => "Smart model",
        K::ActionDismiss => "Dismiss",
        K::ActionRetry => "Retry",
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

        K::ErrNoProvider => "No provider is configured yet.",
        K::ErrAuth => "Your {} credentials are no longer valid.",
        K::ErrRateLimited => "{} is rate limiting requests.",
        K::ErrOffline => "aibo cannot reach the network.",
        K::ErrProviderUnavailable => "{} is not responding.",
        K::ErrContextTooLarge => "The selection is too long for this model.",
        K::ErrTimeout => "The request took too long.",
        K::ErrCaptureFailed => "aibo could not read the text in {}.",
        K::ErrInsertFailed => "aibo could not insert the text — it is still here to copy.",
        K::ErrSandbox => "The sandboxed code did not finish.",
        K::ErrAgentBackendMissing => "The agent backend {} is not installed.",
        K::ErrBudgetExceeded => "This run reached its budget.",
        K::ErrModelRejectedUse => "{} is not available on this account — use {} instead.",
        K::ErrModelRejected => "{} is not available on this account.",
        K::ErrInternal => "Something went wrong inside aibo.",

        K::TrayTooltip => "aibo",
        K::TrayOpenPanel => "Open aibo",
        K::TrayTasks => "Tasks",
        K::TraySettings => "Settings…",
        K::TrayQuit => "Quit aibo",
        K::TrayBusy => "aibo — working",

        K::HotkeyFailedTitle => "The shortcut {} is unavailable",
        K::HotkeyFailedBody => {
            "Another app has already claimed it. Pick a different shortcut in settings."
        }
        K::HotkeyRejectedByOs => "macOS does not accept this combination as a global shortcut.",

        K::TaskWindowTitle => "aibo — task",
        K::TaskSteps => "Steps",
        K::TaskThinking => "Thinking",
        K::TaskRunningTool => "Running {}",
        K::TaskFileChanged => "Changed",
        K::TaskAwaitingApproval => "Waiting for your approval",
        K::TaskCompleted => "Finished",
        K::TaskCancelled => "Cancelled",
        K::TaskFailed => "Failed",
        K::TaskEmpty => "Starting…",
        K::TaskApprovalProvenance => "Requested by your instruction:",

        K::SettingsTitle => "aibo — settings",
        K::SettingsProviders => "Providers",
        K::SettingsRoles => "Roles",
        K::SettingsBudgets => "Budgets",
        K::SettingsPermissions => "Permissions",
        K::SettingsActions => "Actions",
        K::SettingsHistory => "History",
        K::SettingsAbout => "About",
        K::SettingsLanguage => "Language",

        K::SpendThisMonth => "{} this month",
        K::PermissionGranted => "Granted",
        K::PermissionDenied => "Denied",
        K::PermissionNotDetermined => "Not asked yet",
        K::PermissionRevoked => "Permission was removed — grant it again",
    }
}

fn ja(key: Key) -> &'static str {
    use Key as K;
    match key {
        K::AppName => "aibo",

        K::PanelPlaceholder => "質問・書き換え・計算…",
        K::ContextChipFrom => "{}",
        K::ContextChipNone => "コンテキストなし",

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

        K::ActionReplace => "置換",
        K::ActionCopy => "コピー",
        K::ActionSmartModel => "高性能モデル",
        K::ActionDismiss => "閉じる",
        K::ActionRetry => "再試行",
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

        K::ErrNoProvider => "プロバイダーが設定されていません。",
        K::ErrAuth => "{} の認証情報が無効になりました。",
        K::ErrRateLimited => "{} がレート制限中です。",
        K::ErrOffline => "ネットワークに接続できません。",
        K::ErrProviderUnavailable => "{} が応答しません。",
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
        K::ErrInternal => "aibo の内部で問題が発生しました。",

        K::TrayTooltip => "aibo",
        K::TrayOpenPanel => "aibo を開く",
        K::TrayTasks => "タスク",
        K::TraySettings => "設定…",
        K::TrayQuit => "aibo を終了",
        K::TrayBusy => "aibo — 実行中",

        K::HotkeyFailedTitle => "ショートカット {} を使用できません",
        K::HotkeyFailedBody => {
            "他のアプリが使用しています。設定で別のショートカットを選んでください。"
        }
        K::HotkeyRejectedByOs => {
            "macOS はこの組み合わせをグローバルショートカットとして受け付けません。"
        }

        K::TaskWindowTitle => "aibo — タスク",
        K::TaskSteps => "ステップ",
        K::TaskThinking => "思考",
        K::TaskRunningTool => "{} を実行中",
        K::TaskFileChanged => "変更",
        K::TaskAwaitingApproval => "承認を待っています",
        K::TaskCompleted => "完了",
        K::TaskCancelled => "キャンセル",
        K::TaskFailed => "失敗",
        K::TaskEmpty => "開始しています…",
        K::TaskApprovalProvenance => "この指示によるものです:",

        K::SettingsTitle => "aibo — 設定",
        K::SettingsProviders => "プロバイダー",
        K::SettingsRoles => "ロール",
        K::SettingsBudgets => "予算",
        K::SettingsPermissions => "権限",
        K::SettingsActions => "アクション",
        K::SettingsHistory => "履歴",
        K::SettingsAbout => "情報",
        K::SettingsLanguage => "言語",

        K::SpendThisMonth => "今月 {}",
        K::PermissionGranted => "許可済み",
        K::PermissionDenied => "拒否",
        K::PermissionNotDetermined => "未確認",
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
        ];
        for key in SAMPLE {
            for lang in Lang::ALL {
                assert!(!lookup(*key, *lang).is_empty(), "{key:?} / {lang:?}");
            }
            assert_ne!(lookup(*key, Lang::En), lookup(*key, Lang::Ja), "{key:?}");
        }
    }
}
