# S10 — IME **into** aibo's panel

> Can a Japanese user type Japanese into an iced overlay window? `Ime` events,
> `set_ime_allowed`, `set_ime_cursor_area` candidate placement.
> **If it fails: Critical path — if this fails the Japanese market is closed and
> the UI stack decision reopens.** — §20

This is the spike whose failure mode is not "add a workaround" but "reconsider
iced". §9:

> If a Japanese user cannot type Japanese into the panel, the Japanese market is
> closed. Spike **S10**, and it is on the critical path, not a nicety.

Related plan sections: §9 (IME, panel placement), §8 (the overlay-window row and
the macOS/Windows asymmetry), §16 (the 680 pt panel), §20 (the S10 row).

## What is already known, from reading iced 0.14's source

Not assumed — checked before this harness was written:

- `iced_core::event::Event::InputMethod(input_method::Event)` exists, with
  `Opened`, `Preedit(String, Option<Range<usize>>)`, `Commit(String)`, `Closed`.
- `iced_winit` maps winit's `Ime::{Enabled, Preedit, Commit, Disabled}` onto
  those four, and calls `set_ime_allowed` / `set_ime_cursor_area` in response to
  a widget's `Shell::request_input_method`.
- `iced_widget::text_input` issues that request, so **IME is driven by the
  focused widget**, not by the application. An unfocused input means no IME at
  all — which is why this harness focuses one on boot.

So the plumbing is present in 0.14, which is better than §9's "historically been
incomplete in iced" suggests. What remains unknown is whether it *works* on a
wgpu/`CAMetalLayer` surface, in an undecorated always-on-top window, with a real
Japanese input method — and where the candidate window lands.

## Before you start

Add **Japanese – Romaji** in System Settings ▸ Keyboard ▸ Input Sources. If you
have Google Japanese Input or ATOK, run the whole checklist with each: they are
different IME implementations and §20's S7 row already assumes they behave
differently.

## What the operator does

```sh
cd spikes/s10_ime_panel

# 1. Normal window first — establishes the baseline.
cargo run --release

# 2. THE test. Undecorated, AlwaysOnTop, 680 pt: the actual panel shape.
cargo run --release -- --overlay
```

Use `--release`. IME latency in a debug wgpu build tells you nothing.

Every `InputMethod` and `Keyboard` event is printed to stdout *and* shown in the
on-screen log, so you can watch the window and read the transcript afterwards.

### The checklist, in both modes

1. **Basic composition.** Type `n i h o n g o`. Does a preedit appear *in the
   text field* (on-the-spot), or only in a floating IME box, or not at all?
2. **Candidate placement.** Press Space. **Where does the candidate window
   appear?** Next to the caret, or at the window origin, or off in a screen
   corner? This is `set_ime_cursor_area`, and **no event log can answer it — you
   have to look.** Move the window to a different screen position and repeat: a
   candidate window that is correct at one position and wrong at another means
   the coordinates are being computed in the wrong space.
3. **Commit.** Press Return. Does the committed text match what the candidate
   window showed? Does it land at the caret?
4. **Escape mid-composition.** Does it cancel the composition, or close the
   panel? For the product, cancelling the composition must win; a panel that
   vanishes and loses the draft is a bug §13 would have to cover.
5. **Focus change mid-composition.** Start a composition in the first field,
   then click the second field. What happens to the pending composition?
6. **Backspace mid-composition.** Does it delete a kana from the preedit, or a
   character from the committed text behind it?
7. **Multi-display and DPI (§9).** Drag the window to a second display,
   especially one at a different scale factor, and redo steps 1–3. §9 calls out
   mixed DPI and hot-plug explicitly.
8. **Overlay only:** does the window still take key focus at all? §8 says the
   platforms are asymmetric — macOS `nonactivatingPanel` can take key input,
   **Windows `WS_EX_NOACTIVATE` and a text input are mutually exclusive**. On
   Windows, expect to have to accept activation.

## What to record — the go/no-go note

```
S10 — IME into aibo's panel
Date / OS version / GPU:
Input method:  Kotoeri | Google Japanese Input | ATOK      version:
Build:         --release      Mode:  normal | overlay

Event plumbing (from the log)
  Opened fired:                              yes | no
  Preedit fired, with content:               yes | no        count: __
  Preedit selection range present:           yes | no
  Commit fired with the converted text:      yes | no
  Closed fired:                              yes | no

Rendering — look, do not infer
  preedit shown ON-THE-SPOT in the field:    yes | no | as an overlay box
  candidate window position:                 at the caret | window origin | screen corner | elsewhere
  candidate position still correct after moving the window:   yes | no
  candidate position correct on a second display:             yes | no
  candidate position correct at a different DPI scale:        yes | no

Behaviour
  Escape mid-composition:                    cancels composition | closes panel | nothing
  focus change mid-composition:              commits | discards | corrupts
  Backspace mid-composition:                 edits preedit | edits committed text
  committed text == candidate shown:         yes | no

Overlay specifics
  window takes key focus while AlwaysOnTop + undecorated:   yes | no
  any difference vs the normal window:

Latency (subjective is fine, note it as such)
  keystroke → preedit visible:               instant | laggy | unusable

DECISION (§20 — this one is on the critical path)
  [ ] PASS. Japanese input works in the overlay. §9's 🔴 comes down.
  [ ] PASS WITH A DEFECT. Works, but candidate placement is wrong.
      → §9's placement section needs a fix; the market is NOT closed.
  [ ] FAIL in the overlay, PASS in a normal window.
      → The panel cannot be undecorated/AlwaysOnTop as designed. §8's overlay
        row and §16's panel design both reopen.
  [ ] FAIL outright.
      → §20: "the Japanese market is closed and the UI stack decision reopens."
        Escalate before any further UI work — this invalidates §6's crate layout
        for aibo-ui, not just a widget.
```

## Interpreting a partial result

- **`Opened` and `Closed` fire but `Preedit` never does.** The composition is
  happening somewhere the runtime cannot see. The panel can never render inline
  preedit, so the best available product is "the IME's own floating box on top
  of the panel" — usable, ugly, and worth writing down rather than discovering
  in P5.
- **Everything fires, candidate window in the wrong place.** This is a
  `set_ime_cursor_area` coordinate-space bug, most likely logical-vs-physical
  pixels. Annoying, fixable, **not** a market blocker. Do not let it be recorded
  as a fail.
- **Works normal, fails overlay.** The most likely genuinely bad outcome, and
  the one §8 predicts for Windows. Note that §8 also closes the obvious escape
  hatch on macOS: hosting the iced surface in a hand-rolled `NSPanel` is *not*
  available, because `iced_winit::run` builds the `EventLoop` itself with no
  hook. The real alternative is `iced_wgpu` + a custom shell, which is a much
  larger decision than a spike.

## Not in scope

- Detecting composition in **another** application. That is **S7**, a different
  and harder problem.
- Vibrancy and the native window handle. That is **S1**.
- i18n of the UI strings themselves. §9 asks for externalised strings from day
  one; that is P-phase work, not a spike.
