# aibo — visual design spec

> Implements §16 of `plan.md`. Written after seeing the first real render
> (2026-07-26), which works correctly and looks like a form.
>
> **The diagnosis:** every element is a bordered box — source chip, input,
> error — three nested rectangles, no hierarchy, no accent. The fix is not more
> styling. It is removing almost all of it and spending the boldness in one
> place.

---

## 1. The thing this product actually is

aibo appears *over* your work, already knowing what you were doing, and answers
before you've finished deciding to wait. Its world is the caret, the selection,
and the interrupted moment. The plan measures the entire product in
milliseconds.

So the panel should not read as a dialog that arrived. It should read as **the
caret continuing into a second place**. That is the design thesis, and every
choice below derives from it.

The most interesting information on screen is not the input field — it is the
line that says *"Ghostty · …and screencapture works"*. That line is the whole
pitch: it knows where you were. In the current build it is rendered as the
smallest, dullest element on the panel. That inversion is the main thing to fix.

---

## 2. Tokens

### Colour — indigo ground, single amber accent

Deliberately not near-black-plus-acid-green, and not cream-plus-serif — both are
house styles rather than choices. Amber is chosen for a reason specific to this
product: it is the colour of a text caret, it signals attention without alarm,
and it leaves red free for the permission and danger states §16 reserves.

| Token | Hex | Use |
|---|---|---|
| `ink` | `#0E1116` | Panel ground. Near-black with a blue cast, so amber reads warm against it. |
| `ink-raised` | `#161A21` | The one elevation: settings sidebar, hovered rows. |
| `rule` | `#262C36` | Hairlines and the inactive rail. |
| `text` | `#E6E9EF` | Primary. |
| `text-dim` | `#8B94A3` | Source line, provenance, key hints. |
| `amber` | `#F0A742` | Caret, focus, active rail segment, the one live accent. |
| `danger` | `#E5534B` | Permission prompts and destructive confirms **only**. |

Seven values. If a new colour is needed, something else is wrong.

### Type — IBM Plex Sans + IBM Plex Mono

Chosen on the axis that actually constrains this product: **CJK**. §16 records
the contradiction that no bundled variable sans has CJK coverage without
blowing the binary budget. Plex is the one open family with a real, visually
consistent Japanese companion (`IBM Plex Sans JP`), so the fallback chain
degrades within one type family instead of across two unrelated ones. It also
has genuine character — squared terminals, slightly mechanical — that reads as
*instrument* rather than *product*, which is what aibo is.

| Role | Face | Size / weight |
|---|---|---|
| Input | Plex Mono | 15 / 400 |
| Answer body | Plex Sans | 14 / 400, 1.55 line |
| Source line, provenance, key hints | Plex Mono | 11 / 400, `text-dim` |
| Settings headings | Plex Sans | 13 / 600 |
| Numerics (latency, cost, counts) | Plex Mono | tabular figures, always |

Latency and cost are always mono with tabular figures. They change while you
watch them, and digits that shift width while streaming look broken.

### Space

4 pt base, 8 pt rhythm. Panel width **680 pt** (the current build renders
around 440 — fix it). Panel padding 20 pt. Rail gutter 16 pt.

---

## 3. The signature: a state-carrying rail

A 3 pt vertical rail runs the full height of the panel at the left gutter. It is
`rule` by default and **`amber` only on the row that currently has the user's
attention** — the input while typing, the answer while streaming, and it turns
`danger` on a permission or error row.

This is the one memorable element, and it earns its place by encoding something
true rather than decorating: at a glance, the rail tells you where the panel
thinks you are. It also replaces every border in the current design. Nothing
else gets a box.

```
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓  680 pt, 12 pt radius
┃▎ ghostty · "…and screencapture works. Running it n…" ┃  source line, mono 11, dim
┃▎                                                     ┃
┃┃ rewrite this as a changelog entry▏                  ┃  ← rail AMBER at input row
┃▎                                                     ┃     amber block caret
┃▎ cerebras · gpt-oss-120b · 180 ms · ¢0.02            ┃  provenance, mono 11, dim
┃▎ Deployment is now gated behind a feature flag, so   ┃
┃▎ a bad release can be turned off without a rollback. ┃
┃▎                                                     ┃
┃  ───────────────────────────────────────────────     ┃  the ONLY hairline
┃  ⏎ replace    ⌘C copy    ⇥ smart model    esc        ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
```

Error state — the rail carries it, no box required:

```
┃▎ ghostty · "…"                                       ┃
┃┃ hello▏                                              ┃
┃▎                                                     ┃
┃╏ No provider configured.                             ┃  ← rail DANGER at this row
┃▎ Sign in with ChatGPT to start, or add an API key.   ┃
┃▎ ⏎ Sign in     ⌘, Settings                           ┃
```

Compare the current build, which renders that same state as a red-bordered
rectangle containing a bordered button — two more boxes, in a design that
already had three.

---

## 4. States

Every surface needs all of these. One polished happy path is not a design
system, and the current build has exactly one.

| State | Treatment |
|---|---|
| Empty (no input) | Source line + a single dim prompt: `ask, or ⇥ for actions`. No placeholder box. |
| Thinking (pre-first-token) | Rail amber; a 3-dot mono ellipsis in `text-dim` on the provenance row. **No spinner** — a spinner implies indeterminate; this resolves in ~400 ms. |
| Streaming | Answer area reserves its height on first chunk (§16: must not reflow). Rail amber. |
| Truncated | `⚠ stream ended early` in `text-dim` above the footer; footer swaps `⏎ replace` for `⏎ retry`. |
| Error (inline) | Rail `danger`, one sentence, one action. No box, no icon. |
| Permission denied | Rail `danger`, persistent, with `grant` action. Quiet — it is a state, not an alarm. |
| Context unavailable | Source line reads `no context — reading…` then `no context available`, never blank. |
| Long answer | Answer area scrolls internally at 60% display height; rail stays full-height. |

---

## 5. Motion

Three animations, per §16, and one of them is the signature.

1. **Panel in** — 180 ms spring. Opacity 0→1 and a 6 pt rise. Nothing scales;
   scaling reads as a dialog, and this is not one.
2. **The rail draws** — on open, the amber segment sweeps from the top of the
   input row downward over 140 ms. This is the caret arriving. It is the only
   decorative motion in the product and it should be the thing people remember.
3. **Height change** — 200 ms spring when the answer area grows.

Nothing else animates. Streaming text does not fade in per token — that reads as
latency theatre when the real number is already on screen.

Respect reduced-motion: all three collapse to instant, the rail simply appears.

---

## 6. Copy

The current strings name the mechanism instead of the outcome. Fixes:

| Now | Better | Why |
|---|---|---|
| "No provider is configured yet." | **"No provider configured."** + "Sign in with ChatGPT to start, or add an API key." | Drop "yet" — it apologises. Second line says what to do. |
| Button: "Open settings" | **"Sign in"** | Names the outcome, not the window it lives in. §16: the button that says Publish produces "Published". |
| "Input was shortened to fit the context budget." | **"Trimmed 4,200 characters to fit."** | Specific beats vague. The user can judge whether that matters. |
| "Smart model" (footer) | **"smart model"** | Footer hints are lowercase mono throughout — they are keys, not sentences. |

Errors state what happened and what to do. They never apologise and are never
vague. Empty states are an invitation to act, not a mood.

---

## 6b. Sign-in: the device code screen

The only moment aibo asks the user to leave the app and do something in a
browser. Measured from a real run: the code looks like `RJF3-XIERE`, the page is
`auth.openai.com/codex/device`, and it expires in 15 minutes. Retyping a
ten-character code with a hyphen is precisely where people make a mistake and
then cannot tell whether the code was wrong or the app is broken.

```
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃▎ Sign in with ChatGPT                                ┃
┃▎                                                     ┃
┃┃      R J F 3  –  X I E R E        ⧉ copy            ┃  ← mono 28, tracked
┃▎                                                     ┃     rail AMBER here
┃▎ Enter this code at auth.openai.com/codex/device     ┃
┃▎ ⏎ Open the page      ⧉ ⌘C copy code                 ┃
┃▎                                                     ┃
┃▎ Waiting for approval · expires in 14:32             ┃  ← live countdown, mono
┃▎                                                     ┃
┃  ────────────────────────────────────────────────    ┃
┃  esc cancel                                          ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
```

Requirements:

- **The code is copyable three ways**: a `⧉ copy` affordance next to it, `⌘C`
  while the screen is focused, and ordinary text selection with the mouse. Copy
  the code **exactly as the server issued it**, hyphen included — the verification
  page expects that form. Do not copy the display-tracked version with spaces.
- **Confirm the copy**: the affordance becomes `✓ copied` for 1.5 s. Silent
  copying leaves people pressing it twice.
- **`⏎ Open the page`** launches the default browser at the verification URL. Do
  not rely on the user transcribing the URL either.
- **Live countdown** from the server's `expires_at`, mono with tabular figures so
  it does not jitter. At expiry, swap the whole screen for
  `Code expired.` + `⏎ Get a new code` — never leave a dead code on screen.
- **Advance automatically** the moment the poll succeeds. The user is looking at
  the browser, not at aibo; when they come back it should already say
  `Signed in as <email> · ChatGPT Pro`.
- **Poll failures stay quiet.** Pending is HTTP 403 and is the normal case
  (§3a) — never surface it as an error. Only a genuine failure gets the rail.
- Display tracking is visual only: render with letter-spacing so the characters
  are readable one at a time, but keep the underlying value intact for copy.

Rationale for the size: this is the one screen where a transcription error costs
the user a full 15-minute retry cycle. It earns display-scale type where nothing
else in the product does.

---

## 7. Settings

Same rail, same tokens. The active sidebar item is marked by an amber rail
segment — the identity element, reused, so the two windows read as one product.
Sidebar on `ink-raised`, content on `ink`, one hairline between. No card
borders anywhere; group with space and a single hairline.

---

## 8. Quality floor — not optional, not announced

- Every action reachable and labelled by key (§16). The mouse is optional.
- Visible keyboard focus everywhere, drawn in `amber`, never removed.
- `accesskit` wired so VoiceOver and NVDA work — a tool built on accessibility
  APIs that is itself inaccessible is indefensible (§16).
- Contrast: `text` on `ink` is ~14:1, `text-dim` on `ink` ~5.2:1. Both pass AA.
  Amber on `ink` ~8:1. Verify after any palette edit.
- Reduced-motion and reduced-transparency both honoured; the macOS vibrancy in
  §8 must be disabled under Reduce Transparency.
- Width grows within bounds for localisation; the fixed 680 pt is a default,
  not a constraint (§9).

---

## 9. What was deliberately cut

Chanel's rule: take one thing off before leaving.

- **Icons.** Nothing in the panel needs one. The rail plus a key hint carries
  every meaning an icon would.
- **The lightning bolt** on the provenance row in the plan's §16 mock. The
  latency number already says "fast" and says it precisely.
- **All borders except one hairline.** This is the single biggest change from
  the current build, and the one that will make it stop looking generic.
