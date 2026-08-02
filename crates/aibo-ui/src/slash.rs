//! Slash commands: the registry, its filtering, and the popup's state
//! (owner redesign, 2026-08-02).
//!
//! The composer is the command line. Typing `/` as the **first** character
//! opens a completion popup over the panel — the same floating-menu chrome as
//! the quick-pick and the `@` finder — filtered as the token grows. The popup
//! never steals focus: the query *is* the composer text, so typing continues
//! uninterrupted and `esc` merely puts the popup away for that input.
//!
//! A `/` anywhere else is text (paths must stay typeable), and an unknown
//! `/token` submits as ordinary text rather than erroring — `/Users/…` is a
//! path, not a typo'd command.

use std::sync::LazyLock;

use yuru_core::{Candidate, SearchConfig, build_index, search};

use crate::i18n::Key;

/// What accepting a command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAction {
    /// Open the help overlay.
    Help,
    /// Choose the agent's working directory: bare `/cd` opens the picker,
    /// `/cd path` sets it directly.
    Workdir,
    /// Run the coding agent on the trailing text (handled by the runtime's
    /// existing `/agent` strip; the popup only completes the prefix).
    Agent,
    /// Start a fresh session.
    NewChat,
    /// Open the model quick-pick.
    Model,
    /// Open the settings window.
    Settings,
    /// Invoke a named skill; the runtime expands it (falls through to
    /// submit, like [`CommandAction::Agent`]).
    Skill,
    /// Open the installed-skills overlay.
    Skills,
}

/// One registered command.
#[derive(Debug)]
pub struct Command {
    /// Canonical spelling, slash included.
    pub name: &'static str,
    /// Bare-word aliases accepted as a *whole* submission ("?", "help").
    pub aliases: &'static [&'static str],
    /// One-line description in the popup and the help card.
    pub description: Key,
    /// What accepting it does.
    pub action: CommandAction,
    /// Whether trailing text belongs to the command ("/agent fix the test").
    /// Accepting completes to `name ` and leaves the user typing; an
    /// argument-less command executes immediately.
    pub takes_args: bool,
}

/// Every command, in display order.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "/help",
        aliases: &["?", "help"],
        description: Key::CmdHelpDesc,
        action: CommandAction::Help,
        takes_args: false,
    },
    Command {
        name: "/agent",
        aliases: &[],
        description: Key::CmdAgentDesc,
        action: CommandAction::Agent,
        takes_args: true,
    },
    Command {
        name: "/cd",
        aliases: &[],
        description: Key::CmdCdDesc,
        action: CommandAction::Workdir,
        takes_args: true,
    },
    Command {
        name: "/new",
        aliases: &[],
        description: Key::CmdNewDesc,
        action: CommandAction::NewChat,
        takes_args: false,
    },
    Command {
        name: "/model",
        aliases: &[],
        description: Key::CmdModelDesc,
        action: CommandAction::Model,
        takes_args: false,
    },
    Command {
        name: "/settings",
        aliases: &[],
        description: Key::CmdSettingsDesc,
        action: CommandAction::Settings,
        takes_args: false,
    },
    Command {
        name: "/skill",
        aliases: &[],
        description: Key::CmdSkillDesc,
        action: CommandAction::Skill,
        takes_args: true,
    },
    Command {
        name: "/skills",
        aliases: &[],
        description: Key::CmdSkillsDesc,
        action: CommandAction::Skills,
        takes_args: false,
    },
];

/// yuru's index over the command names, sans slash, ids matching positions in
/// [`COMMANDS`]. Built once: the registry is static, and the backend — shared
/// with the `@` finder — carries the embedded dictionary.
static SLASH_INDEX: LazyLock<(Vec<Candidate>, SearchConfig)> = LazyLock::new(|| {
    let config = SearchConfig {
        limit: COMMANDS.len(),
        ..SearchConfig::default()
    };
    let index = build_index(
        COMMANDS.iter().map(|command| command.name[1..].to_owned()),
        crate::file_finder::japanese_backend(),
        &config,
    );
    (index, config)
});

/// The commands matching the composer's current text, for the popup.
///
/// Empty when the text is not a command-in-progress at all — the popup should
/// then be closed, not showing "no match".
///
/// Matching is yuru (owner, 2026-08-02) — the same matcher as the `@` finder
/// — rather than a bare prefix test, so `/mdl` still finds `/model` and a
/// romaji-minded typo is forgiven. An empty token shows the registry in
/// display order; a query shows yuru's ranking.
pub fn matches(input: &str) -> Vec<&'static Command> {
    let Some(token) = command_token(input) else {
        return Vec::new();
    };
    if token.is_empty() {
        return COMMANDS.iter().collect();
    }
    let (index, config) = &*SLASH_INDEX;
    search(
        &token,
        index,
        crate::file_finder::japanese_backend(),
        config,
    )
    .into_iter()
    .filter_map(|scored| COMMANDS.get(scored.id))
    .collect()
}

/// The lowercase command token being typed, if the input is exactly a leading
/// `/` plus a partial name. Any whitespace means arguments have begun; any
/// second `/` means it is a path.
fn command_token(input: &str) -> Option<String> {
    let input = half_width(input);
    let rest = input.strip_prefix('/')?;
    if rest.contains(char::is_whitespace) || rest.contains('/') {
        return None;
    }
    Some(rest.to_lowercase())
}

/// Fold the full-width forms (U+FF01–U+FF5E) back onto their ASCII originals.
///
/// A Japanese IME's default commit for `/help` is `／ｈｅｌｐ` — same intent,
/// different code points (observed live, 2026-08-02: the popup opened for a
/// pasted `/` and stayed shut for the committed `／`, which reads as "commands
/// don't work in Japanese mode"). Command names are ASCII, so folding the
/// token loses nothing; arguments are never folded — a full-width string in
/// `/cd`'s path is a real filename, not a typo.
fn half_width(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// Parse a submission into the command it names, with its arguments.
///
/// Bare aliases match only as the *entire* trimmed input: "help" is a command,
/// "help me write this" is a sentence. Slash names match as the first token,
/// case-insensitively; an unknown `/token` is text, not an error.
pub fn parse(input: &str) -> Option<(&'static Command, &str)> {
    let trimmed = input.trim();
    let lowered = trimmed.to_lowercase();
    if let Some(command) = COMMANDS
        .iter()
        .find(|command| command.aliases.iter().any(|alias| *alias == lowered))
    {
        return Some((command, ""));
    }
    if !trimmed.starts_with('/') && !trimmed.starts_with('\u{FF0F}') {
        return None;
    }
    let (token, args) = match trimmed.split_once(char::is_whitespace) {
        Some((token, args)) => (token, args.trim_start()),
        None => (trimmed, ""),
    };
    // Only the token is folded (see `half_width`); `args` stays verbatim.
    let token = half_width(token).to_lowercase();
    COMMANDS
        .iter()
        .find(|command| command.name == token)
        .map(|command| (command, args))
}

/// Popup state. The query lives in the composer; this holds only what the
/// composer text cannot express — the highlight, and whether `esc` dismissed
/// the popup for the text as it stood.
#[derive(Debug, Default)]
pub struct SlashState {
    /// Whether the popup is showing.
    pub open: bool,
    /// Highlighted row, an index into [`matches`].
    pub highlight: usize,
    /// The exact input `esc` dismissed the popup for. Any edit reopens.
    dismissed_for: Option<String>,
}

impl SlashState {
    /// Reconcile with the composer text. Call after every input change.
    pub fn sync(&mut self, input: &str) {
        if self
            .dismissed_for
            .as_deref()
            .is_some_and(|dismissed| dismissed != input)
        {
            self.dismissed_for = None;
        }
        let candidates = matches(input);
        self.open = !candidates.is_empty() && self.dismissed_for.is_none();
        if self.highlight >= candidates.len() {
            self.highlight = 0;
        }
    }

    /// Move the highlight, wrapping.
    pub fn move_highlight(&mut self, delta: i32, count: usize) {
        if count == 0 {
            return;
        }
        let count = i32::try_from(count).unwrap_or(i32::MAX);
        let current = i32::try_from(self.highlight).unwrap_or(0);
        self.highlight = usize::try_from((current + delta).rem_euclid(count)).unwrap_or(0);
    }

    /// `esc`: put the popup away until the text changes.
    pub fn dismiss(&mut self, input: &str) {
        self.open = false;
        self.dismissed_for = Some(input.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of yuru here: completion forgives more than a prefix.
    #[test]
    fn fuzzy_queries_reach_their_command() {
        let m = matches("/mdl");
        assert!(
            m.iter().any(|c| c.name == "/model"),
            "/mdl finds /model, got {:?}",
            m.iter().map(|c| c.name).collect::<Vec<_>>()
        );
        let m = matches("/setings");
        assert!(
            m.iter().any(|c| c.name == "/settings"),
            "a dropped letter still finds /settings"
        );
    }

    /// The Japanese IME commits `／ｈｅｌｐ` for `/help`; both spellings are
    /// the same intent and both must reach the same command.
    #[test]
    fn full_width_spellings_match_commands() {
        let m = matches("\u{FF0F}\u{FF48}\u{FF45}");
        assert_eq!(m.len(), 1, "／ｈｅ filters like /he");
        assert_eq!(m[0].name, "/help");

        let (command, args) =
            parse("\u{FF0F}\u{FF48}\u{FF45}\u{FF4C}\u{FF50}").expect("／ｈｅｌｐ parses");
        assert_eq!(command.name, "/help");
        assert_eq!(args, "");

        // Arguments are never folded: a full-width path stays as typed.
        let (command, args) = parse("/cd ／ｄａｔａ").expect("args keep their spelling");
        assert_eq!(command.name, "/cd");
        assert_eq!(args, "／ｄａｔａ");
    }

    #[test]
    fn a_leading_slash_filters_the_registry() {
        assert_eq!(matches("/").len(), COMMANDS.len());
        let m = matches("/he");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "/help");
        assert!(matches("/HE").len() == 1, "case-insensitive");
    }

    #[test]
    fn paths_and_arguments_never_open_the_popup() {
        assert!(matches("/Users/ryuichi").is_empty(), "a second / is a path");
        assert!(matches("/agent fix it").is_empty(), "arguments close it");
        assert!(matches("say /help").is_empty(), "mid-text / is text");
        assert!(matches("").is_empty());
    }

    #[test]
    fn parse_accepts_names_and_whole_input_aliases() {
        assert_eq!(parse("/help").unwrap().0.name, "/help");
        assert_eq!(parse("?").unwrap().0.name, "/help");
        assert_eq!(parse("  Help  ").unwrap().0.name, "/help");
        assert!(
            parse("help me write this").is_none(),
            "a sentence is not a command"
        );
        let (agent, args) = parse("/agent fix the flaky test").unwrap();
        assert_eq!(agent.name, "/agent");
        assert_eq!(args, "fix the flaky test");
        assert!(
            parse("/Users/ryuichi/dev").is_none(),
            "unknown /token is text"
        );
    }

    #[test]
    fn esc_dismisses_until_the_text_changes() {
        let mut state = SlashState::default();
        state.sync("/he");
        assert!(state.open);
        state.dismiss("/he");
        state.sync("/he");
        assert!(!state.open, "dismissed for this exact text");
        state.sync("/hel");
        assert!(state.open, "an edit reopens");
    }

    #[test]
    fn the_highlight_wraps_and_clamps() {
        let mut state = SlashState::default();
        state.sync("/");
        state.move_highlight(-1, COMMANDS.len());
        assert_eq!(state.highlight, COMMANDS.len() - 1);
        state.move_highlight(1, COMMANDS.len());
        assert_eq!(state.highlight, 0);
        // Narrowing the filter below the highlight resets it.
        state.highlight = 3;
        state.sync("/he");
        assert_eq!(state.highlight, 0);
    }
}
