# aibo

A hotkey-summoned AI panel that appears over whatever you're doing, already
knowing what you were doing. Pure Rust, [iced](https://iced.rs) 0.14, macOS
first (Windows builds in CI).

Press the hotkey and the panel opens over your work carrying the frontmost
app's context — selection, caret, or a screenshot crop — asks a model, and can
paste the answer straight back where your caret was. It is designed to be
measured in milliseconds: no dock icon, no browser tab, no context switch.

## Features

- **Context capture** — the panel opens knowing the frontmost app and your
  selection (accessibility tree on macOS, opt-in for Electron/Chrome apps),
  or a screenshot crop for anything visual.
- **Multi-provider** — OpenAI (Responses and Chat Completions), Anthropic,
  Gemini, Azure OpenAI, Cerebras, SambaNova, Groq, xAI, OpenRouter,
  Ollama/llama.cpp, and any OpenAI-compatible custom endpoint. Roles route
  each kind of request (fast/smart/vision/…) through a fallback chain.
- **ChatGPT plan (Codex) sign-in** — device-code auth uses a ChatGPT
  subscription directly, no API key. These models handle images too.
- **Model quick-pick** — `⌘K` opens a floating picker with per-provider
  icons; `⌘D` pins favourites, and pins persist.
- **Dictation** — `⌘L` streams the microphone through OpenAI realtime
  transcription (`gpt-live-transcribe`) into the composer. Works in Japanese.
- **`@` file attach** — type `@` in the composer to fuzzy-find a file by
  name and attach its text. Matching is [yuru](https://crates.io/crates/yuru-core),
  so romaji finds kanji and kana filenames: `toukei` → `統計資料.pdf`.
- **Budget** — an optional monthly spend ceiling with warnings, priced from
  a user-correctable TOML table.
- **English and Japanese UI**, following the system language.

## Keyboard

| Key | Action |
|---|---|
| `⌥Space` | Summon / dismiss the panel (Windows: `Ctrl+Shift+Space`) |
| `⌥⇧Space` | Crop a screen region and open it as an attachment (macOS) |
| `⏎` / `⇧⏎` | Send / newline |
| `⌘⏎` | Paste the latest answer back into the app you came from |
| `⌘⇧⏎` | Escalate the last question to the smart model |
| `⌘C` | Copy the answer (when not editing text) |
| `⌘V` | Attach the clipboard image |
| `⌘N` | New chat |
| `⌘K` / `⌘D` | Model picker / pin the current model |
| `⌘L` | Start or stop dictation |
| `⌘R` / `⌘.` | Retry / cancel |
| `@` | File finder |
| `⇥` | Cycle lanes |
| `↑` / `↓` | Prompt history |
| `⌘T` | Task window (agent runs) |
| `⌘,` | Settings |
| `esc` | Dismiss |

## First run

macOS asks for permissions as features are first used: Accessibility for
reading the frontmost app's selection, Microphone for dictation, and Screen
Recording for region capture. Each is optional — the panel works without it,
minus that feature. Electron and Chrome apps expose no selection until
`allow_ax_tree_activation` is enabled in settings; it is off by default
because that flag can degrade window behaviour in those apps.

## Configuration

Everything lives under one directory — `~/Library/Application Support/aibo`
on macOS, `%APPDATA%\aibo` on Windows. Credentials are stored there in
owner-only files, never in `config.toml`; environment variables
(`AIBO_<PROVIDER>_API_KEY`, e.g. `AIBO_OPENAI_API_KEY` — dictation uses the
OpenAI one) work as a fallback. Everything below is editable in the settings
window — providers, pins, the panel hotkey, `@` finder roots, the monthly
budget, the accessibility opt-in, language — and `config.toml` remains the
hand-editable equivalent (plus advanced knobs like role chains and request
deadlines that have no UI yet):

```toml
[ui]
language = "ja"                      # optional; defaults to the system language
panel_hotkey = "control+alt+Space"   # optional; rebind if the default is taken

[pins]
models = ["openai/gpt-5.6", "anthropic/claude-fable-5"]

[files]
roots = ["~/Documents", "~/dev"]     # @ finder roots; defaults to
                                     # Documents, Desktop and Downloads

[[providers]]
backend = "anthropic"                # key goes in settings, not here

[budget]
limit_micros = 20000000              # $20/month soft ceiling
```

## Building

```sh
cargo build --release
./target/release/aibo
```

Quality gates, all enforced in CI:

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

`vendor/` carries temporary patches to `cosmic-text` and `cryoglyph`
(overflow fixes for CJK fallback shaping) applied via `[patch.crates-io]`;
they retire with the next iced upgrade.

## Layout

| Crate | Owns |
|---|---|
| `src/` (root) | The runtime: process supervision, provider routing, file walk, dictation |
| `crates/aibo-ui` | The iced shell: panel, settings, picker, finder, i18n |
| `crates/aibo-platform` | OS integration: context capture, screenshots, paste-back |
| `crates/aibo-provider` | Provider protocol adapters |
| `crates/aibo-session` | Configuration and session state |
| `crates/aibo-core` | Roles, routing, shared types |
| `crates/aibo-agent` / `aibo-tools` | Agent loop and tiered tool execution |
| `crates/aibo-store` | Local-only encrypted persistence (SQLCipher) |

`docs/plan.md` is the product spec (§-references in code comments point into
it) and `docs/design.md` the visual spec.
