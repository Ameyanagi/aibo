# S2 — AX read across real apps (macOS)

> Does `text_field_context()` work in Safari, Chrome, VS Code, Slack, Word,
> Notion, Terminal? Build the honest matrix **and** `testapps/`.
> **If it fails: Complete degrades to clipboard-only in unsupported apps; narrow
> the marketing claim.** — §20

This binary decides nothing. It walks the AX tree of whatever app you put in
front of it and writes down what was readable, what failed, with which
`AXError`, and how long each read took. You assemble `docs/app-matrix.md` (§18
tier 4) from its output by hand.

Related plan sections: §8 (the capability matrix and the 120 ms deadline), §18
(tier 3 `testapps/`, tier 4 the manual app matrix), §20 (the S2 row).

macOS only. The Windows half — UIA `GetSelection`, and the fact that **Chromium
declares no `ITextProvider2`** so Chrome, Edge, Electron, Slack and VS Code have
no `GetCaretRange` — is a separate harness on a Windows box. §8's risk column for
the two platforms is different enough that one binary would obscure both.

## Before you start

**Grant Accessibility to the binary that actually runs.** For `cargo run` that
is `target/debug/s2_ax_matrix`, not Terminal and not cargo. §17: the TCC grant is
keyed to the **code signature**, so an unsigned debug build invalidates its own
grant on every rebuild — you will re-grant it more than once, and *that* is
itself a finding for the packaging story (S8).

The binary prints a loud warning and continues if `AXIsProcessTrusted()` is
false. Let it run once in that state: seeing every read come back
`kAXErrorAPIDisabled` is how you learn to recognise "no permission" versus "app
publishes nothing", which look identical in a log.

Every command waits `--delay` seconds (default 5) before capturing, because
**this process must not be frontmost when the read happens**. Start the command,
then click into the target app and put the caret in a text field.

## What the operator does

```sh
cd spikes/s2_ax_matrix

# Sanity-check the delay dance first.
cargo run -- who

# THE command. Run it once per app in the matrix, appending to one file.
cargo run -- probe --out ../../s2-matrix.jsonl

# When probe says "unsupported": is the text elsewhere in the tree, or absent?
cargo run -- walk --depth 8 --values

# Chromium and Electron hide their tree until a flag is set (§8).
cargo run -- enable --flag enhanced    # Chrome, Edge, Brave
cargo run -- enable --flag manual      # Slack, VS Code, Notion, Discord
```

### The app list, in the order worth doing

Do `testapps/aibo-axtarget` **first**. It is the controlled target: if the probe
cannot read a plain `NSTextField`, the problem is your permissions or this
binary, not the app under test. Every other row is meaningless until that one is
green.

| # | App | What to put the caret in | Why it is on the list |
|---|---|---|---|
| 0 | `testapps/aibo-axtarget` | the single-line field, then the multi-line view | control; §18 tier 3 |
| 1 | TextEdit | body | plain AppKit baseline |
| 2 | Notes | a note body | Apple, non-trivial |
| 3 | Mail | message body | the money surface |
| 4 | Safari | a `<textarea>`, then a `contenteditable` | WebKit |
| 5 | Chrome | same two | needs `--flag enhanced`; async activation |
| 6 | Slack | message composer | Electron; needs `--flag manual` |
| 7 | VS Code | editor, then the find box | Electron; the editor is a canvas |
| 8 | Notion | a page body | Electron + contenteditable |
| 9 | Word | document body | its own AX implementation |
| 10 | Terminal / Ghostty | the shell prompt | `AXTextArea` that is not editable in the usual sense |
| 11 | Obsidian | note body | Electron + CodeMirror |
| 12 | 1Password / any password field | a password field | expect **secure input mode** (§8) |

For each: run `probe`, then **repeat it with a non-empty selection** — the
selection path and the caret path fail independently and a matrix that only
tested one is wrong.

## Reading the output

```
  attribute                             ms  result
  AXValue                                0  string[3808] " ruviz  1:2.1.219…"
  AXSelectedText                         0  — kAXErrorNoValue (-25212)
  AXSelectedTextRange                    0  range loc=0 len=0 (UTF-16 units)
```

- A `!` in the left margin means that single read exceeded §8's **120 ms** AX
  deadline. **"Supported but slow" is a distinct finding from "unsupported"** and
  is just as fatal: §8's capture runs on a thread with a hard deadline, and an
  app that answers in 400 ms is an app where the panel shows "reading context…"
  and then gives up.
- The error code is the diagnosis, so it is printed raw:
  - `kAXErrorAttributeUnsupported` (-25205) — the app genuinely does not expose it.
  - `kAXErrorNoValue` (-25212) — supported, currently empty (e.g. no selection).
    **Not the same thing.** Collapsing these two is the most common way to write
    a matrix that is wrong.
  - `kAXErrorCannotComplete` (-25204) — target busy, or not trusted.
  - `kAXErrorAPIDisabled` (-25211) — Accessibility not granted to *this* binary.
- `AXSelectedTextRange` prints as `range loc=… len=…` only because §8 says it is
  *"an `AXValue` wrapping `CFRange` — unwrap via `AXValueGetValue`"*. If you ever
  see it as `(opaque AXValue)`, the unwrap broke — do not record it as
  unsupported.
- **Range units are UTF-16 code units.** Not bytes, not graphemes. Test with an
  emoji and a Japanese sentence in the field and confirm the numbers you get back
  are what you expect; every later insert-at-caret depends on it.

## What to record — the matrix row

One row per app, per surface within the app (Safari's `<textarea>` and its
`contenteditable` are two rows, not one).

```
S2 — macOS AX read matrix
Date / macOS version / hardware:
Binary signed?  yes | no (unsigned debug — grant re-done __ times)

App:                          version:
Bundle id:
Surface within the app:       (textarea / contenteditable / editor / composer …)
Enabling flag needed:         none | AXEnhancedUserInterface | AXManualAccessibility
  if a flag was set — settle time before the tree appeared:   __ ms
  did the app visibly degrade (window positioning, resize)?   yes | no

Focused element role/subrole:
  AXValue readable:                 yes | no   err:______  __ ms
  AXSelectedText readable:          yes | no   err:______  __ ms   (WITH a selection)
  AXSelectedTextRange readable:     yes | no   err:______  __ ms
  AXNumberOfCharacters:             yes | no
  AXPosition/AXSize (caret anchor): yes | no       <- §9 placement depends on this
  total capture:                    __ ms   within 120 ms? yes | no

Unicode
  emoji + Japanese in the field, range offsets look like UTF-16:  yes | no
  AXValue round-trips the text exactly:                           yes | no

Verdict for this row:
  [ ] full        — value + selection + range
  [ ] partial     — value only, no range (no caret anchoring, no FIM)
  [ ] selection-only
  [ ] clipboard-only  — nothing readable; §20's degradation applies
  [ ] blocked     — secure input mode / no AX tree at all
```

## The three answers the spike owes the plan

1. **What fraction of the target surface is `clipboard-only`?** §20's fallback is
   *"Complete degrades to clipboard-only in unsupported apps; narrow the
   marketing claim."* Count the rows. If Electron plus Chromium land in
   `clipboard-only`, that is plausibly the majority of real use and the claim on
   the website has to change before P1, not after.
2. **Is the caret anchor (§9 placement) available often enough to design for?**
   If `AXPosition`/`AXSize` are missing on the important apps, the panel anchors
   to the screen, not the caret, and §9's placement section needs rewriting.
3. **Is setting the AX-enabling flags acceptable?** §8: `AXEnhancedUserInterface`
   *"breaks window positioning and makes resizing sluggish"*, which is why
   Electron invented an alternative, and *"setting it from a tray utility is
   user-hostile; consider asking first."* Answer with what you observed, not with
   what is convenient.

## Implementation notes worth carrying into `aibo-platform`

- All `unsafe` is in `src/ax.rs`, and the tree walk is **iterative with an
  explicit node budget** so a pathological tree cannot blow the stack from inside
  FFI. Chrome's web area is tens of thousands of nodes; `--budget` defaults to
  400 and hitting it is a result.
- `AXUIElementSetMessagingTimeout` is set on every element before use.
  §8 corrected an earlier draft on exactly this: an unbounded AX call against a
  busy app blocks for *seconds*. If a read only succeeds with `--ax-timeout 5`,
  record it as a failure with a note, not as a success.
- `accessibility-sys` 0.2.0 was read before being used. §20 flags it as
  single-maintainer with one release in four years; it is a bare `extern "C"`
  block with no logic, which is why vendoring it later is a copy-paste rather
  than a project.

## Not in scope

- Writing. Insert reliability is **S4**.
- IME composition state. That is **S7**, and §9 rule 3 says AX reads *during*
  composition return the pre-composition text or the uncommitted reading — so
  every row here should be captured with no composition in progress.
- Windows UIA.
