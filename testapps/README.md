# testapps — controlled AX/UIA targets

> **`testapps/` is the important idea:** a controlled AX/UIA target makes the
> platform layer genuinely testable instead of "run it and see". Build it in P0
> alongside S2 — **the spike and the test harness are the same work.** — §18

| App | Platform | Status |
|---|---|---|
| [`aibo-axtarget`](aibo-axtarget/) | macOS | built |
| `aibo-uiatarget` | Windows | **not built** — see below |

## Why this exists

Every other row in §18's tier-4 app matrix is a third-party app that will change
under you. §18 says so directly: *"Third-party app behaviour will rot as those
apps update."* A run against Slack that fails tells you nothing until you know
whether your reader works at all.

`aibo-axtarget` is the control. Plain AppKit controls — `NSTextField`,
`NSSecureTextField`, `NSTextView` — whose accessibility behaviour is documented,
seeded with text whose byte / UTF-16 / grapheme counts are **printed at launch**.
A harness asserts on numbers; a human diffs a table. Either way the expected
answer exists in writing before anyone looks at a window.

**Run every spike against this app first.** If `s2_ax_matrix probe` cannot read a
plain `NSTextField`, the problem is your Accessibility grant or the spike, and
nothing you learn about Slack that afternoon is worth writing down.

## `aibo-axtarget`

```sh
cd testapps/aibo-axtarget

# Run it directly — stdout is the oracle.
cargo run

# Or as a real .app, so it has a bundle id for the matrix key.
./bundle.sh
./target/debug/aibo-axtarget.app/Contents/MacOS/aibo-axtarget

# The counts are unit-tested and run anywhere (§18 tier 1).
cargo test
```

Five controls, each there for a stated reason:

| Identifier | Control | Why |
|---|---|---|
| `aibo.single-line` | `NSTextField` | baseline ASCII; bytes == UTF-16 == graphemes |
| `aibo.single-line-ja` | `NSTextField` | BMP Japanese; bytes ≠ UTF-16, UTF-16 == graphemes |
| `aibo.unicode-traps` | `NSTextField` | ZWJ family emoji, regional-indicator flag, combining mark, astral CJK — **every count differs** |
| `aibo.secure` | `NSSecureTextField` | reproduces **secure input mode** (§8) on demand |
| `aibo.multi-line` | `NSTextView` | newlines, leading spaces, `U+3000`, trailing spaces, a blank line |

Each control also carries a caption `NSTextField` with the identifier
`<id>.caption`. Those are deliberate negative cases: a reader that reports the
caption as the focused editable field has a bug, and catching it here is cheaper
than catching it in Slack.

### The three counts, and why all three are printed

```
identifier              bytes  utf16  chars  graphemes
aibo.unicode-traps         76     55     48         40
```

- **UTF-16 code units** is what `kAXSelectedTextRangeAttribute` speaks — §8:
  *"an `AXValue` wrapping `CFRange`"*, and `CFRange` over a `CFString` is
  UTF-16. Every off-by-one in insert-at-caret lives in this column.
- **graphemes** is what a user calls "characters", and what §5's middle-out
  truncation must not split.
- **bytes** is what Rust's `str::len` gives you, and is right for nothing at the
  platform boundary.

The grapheme counter in `src/known.rs` is a deliberate hand-rolled
approximation. The app has **no `unicode-segmentation` dependency on purpose**: a
fixture that uses the same crate as the code under test can agree with it and
still have both be wrong. It is exact for the shipped samples and unit-tested to
prove it.

### Secure input mode

`aibo.secure` is an `NSSecureTextField`. Focusing it turns on secure event input
process-wide, which is §8's named silent-failure mode:

> `IsSecureEventInputEnabled()` — password fields, Terminal, and password
> managers block keystroke synthesis and AX reads. Other apps can leave it stuck
> **globally**. […] Paste-based insert fails silently with no diagnosable cause
> unless you detect and explain it.

Focus it, run `s2_ax_matrix probe` and `s4_insert insert`, and record exactly
what each does. Most likely: nothing at all, with no error. **That silence is the
finding**, and §13's promise to explain failures to the user depends on being
able to detect it.

### Not signed

`bundle.sh` produces an unsigned `.app`. §17 notes the Accessibility TCC grant is
keyed to the code signature, so the *probing* binary needs re-granting on every
rebuild — but the *target* needs no permissions at all, which is another reason
to keep it dumb. Signing and notarisation are S8's problem, not this app's.

## The Windows target — not built

§18 puts the Windows half in a materially better position:

> Windows: **CI-able**, UIA needs no permission. macOS: **needs a self-hosted
> runner** with Accessibility pre-granted — GitHub-hosted runners cannot grant
> TCC.

So `aibo-uiatarget` is worth more than its macOS twin: it is the only tier-3
target that can run on every push. What it must cover, from §8:

- A plain Win32 `EDIT` control and a WinUI/WPF `TextBox`, so both UIA providers
  are exercised.
- A control that supports `ITextProvider` but **not** `ITextProvider2` — the
  Chromium shape. §8: *"Chromium declares only `ITextProvider`/`ITextEditProvider`
  — no `ITextProvider2` — so Chrome, Edge, Electron, Slack and VS Code have no
  `GetCaretRange`. That's plausibly the majority of the target surface."* A
  control that reproduces this without needing Chrome installed is the single
  most valuable thing the Windows target can offer.
- A control where `SupportedTextSelection` is `None`, because §8 warns that
  `GetSelection` on such a control returns *success with NULL ranges*, not an
  error — a failure mode that looks like an empty selection.
- A password `EDIT` for the UIPI/secure-input analogue.
- The same seed strings and the same printed count table, so the two platforms'
  results are directly comparable.
