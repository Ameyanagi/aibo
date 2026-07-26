# S7 — IME composition detection

> Can composition state be detected cross-process on both OSes? What happens if
> you paste mid-composition in Japanese in Slack, VS Code, Word?
> **If it fails: block insert whenever the source app is in a known-IME state;
> document the limitation.** — §20

§9 already tells you the likely answer on macOS:

> Windows detection is `ImmGetContext` + `ImmGetCompositionString` on the
> foreground window. **macOS has no clean cross-process API for this**, which is
> why this is 🔴 and why it needs spike S7 rather than a paragraph of confidence.

So this spike is not trying to prove it works. It is trying to find out *how
badly* it does not, and whether there is anything better than §20's fallback.
Related plan sections: §9 (IME as first-class, the three rules), §8 (the IME row
in the capability matrix), §20 (the S7 row).

**You need to be able to type Japanese to run this.** Add Japanese-Romaji in
System Settings ▸ Keyboard ▸ Input Sources first.

## What the operator does

```sh
cd spikes/s7_ime

# 1. The coarse signal. Run once per input source.
cargo run -- source
cargo run -- source --watch     # then switch layouts with ⌃Space

# 2. Is there a per-app AX attribute nobody documented? (Run per app.)
cargo run -- attributes

# 3. THE experiment: a transcript of the field while you compose.
(cd ../../testapps/aibo-axtarget && cargo run)   # in another terminal
cargo run -- watch --duration 40 --out ../../s7-axtarget.jsonl
```

### The typing script for `watch`

Do this identically in every app so the transcripts are comparable:

1. Type `n i h o n g o` **slowly**, one key at a time.
2. Press **Space** to convert to 日本語.
3. Press **Return** to commit.
4. Repeat 1–2, and at step 2 press **⌘V** with something on the clipboard
   instead of Space. This is §9 rule 2: *"synthetic paste during composition
   corrupts the buffer — the pending composition and the pasted text interleave
   unpredictably."*
5. Repeat 1–2, and at step 2 press your **aibo hotkey candidate** (⌥Space). This
   is §9 rule 1: *"the hotkey may be swallowed by the IME while a composition is
   active."*

Apps to run it in, at minimum: `testapps/aibo-axtarget`, TextEdit, Safari,
Chrome, **Slack**, **VS Code**, **Word**, Notion. §20 names Slack, VS Code and
Word explicitly.

## What the three commands actually tell you

| Command | Signal | Strength |
|---|---|---|
| `source` | Text Input Services: which input source is selected | **Coarse.** "Japanese-Romaji is selected" does *not* mean a composition is active. "US ABC is selected" *does* mean one is not. This is §20's fallback, implemented. |
| `attributes` | every AX attribute the focused element advertises, with `mark`/`composi`/`input`/`candidate` flagged | **Exploratory.** Finding a real marked-text attribute in a widely-used app would beat the fallback outright and is the most valuable thing this spike can turn up. Expect nothing. |
| `watch` | timestamped `AXValue` / `AXSelectedTextRange` / `AXSelectedText` transcript | **The evidence.** §9 rule 3 claims reads during composition return the pre-composition text or the uncommitted reading. The transcript either shows that or it doesn't. |

`source` is process-global: it reports what the *system* has selected, which
follows the frontmost app. Convenient here, and a real limitation for the
product — it says nothing about a background app.

## What to record — the go/no-go note

```
S7 — IME composition detection (macOS)
Date / macOS version / input method (Kotoeri | Google Japanese Input | ATOK):

--- Coarse signal ---
Input source id when Japanese selected:
  kind reported:                     TISTypeKeyboardInputMode | …InputMethod | other
  is_cjk() matched it:               yes | no   (if no, add the prefix to tis.rs)
  changes when the frontmost app changes:   yes | no

--- Per app: the watch transcript ---
App:                              version:
  During composition, AXValue showed:
      [ ] the uncommitted reading (にほんご)
      [ ] the pre-composition text (nothing new)
      [ ] the romaji as typed (nihongo)
      [ ] unreadable / no focused element
  AXSelectedTextRange during composition:  loc __ len __   (stable? moving?)
  Any (value, range) pattern that reliably means "composing":   yes | no
      if yes, describe it — THIS IS THE HEADLINE FINDING:
  Composition-related AX attribute found by `attributes`:  none | ______

  ⌘V mid-composition:
      [ ] corrupted the buffer (interleaved text)
      [ ] was swallowed entirely
      [ ] committed the composition first, then pasted
      [ ] pasted cleanly
  ⌥Space mid-composition:
      [ ] swallowed by the IME (§9 rule 1 confirmed)
      [ ] reached the global hotkey

--- Decision (§9, §20) ---
  [ ] A real detection exists. FieldContext.ime_active can be honest.
  [ ] §20's fallback: block read AND insert whenever a CJK input source is
      selected, regardless of whether a composition is actually active.
      Cost to the user: aibo is unavailable for the whole time a Japanese user
      has Japanese input selected — which for a Japanese user is ALWAYS.
      >>> If this is the outcome, say so loudly. It is close to "aibo does not
          work for Japanese users", and §9 already flags the Japanese market as
          a go/no-go for the product.
  [ ] Something in between (e.g. block insert, allow read):
```

That middle box is the one to think hardest about. §20's fallback reads as a
mild degradation, but combined with §9's observation that a Japanese user has an
IME selected essentially all the time, "block whenever a known-IME state" can
collapse into "never works". If the transcript gives no finer signal, the honest
product answer may be to allow the operation and accept occasional corruption
with an undo path — which is a §11/§13 conversation, not an implementation
detail.

## The Windows half — not implemented

`SPIKE: S7` in `src/main.rs`. §9 says the Windows detection is clean:
`ImmGetContext` + `ImmGetCompositionString` on the foreground window. What the
Windows session must still establish:

- Does `ImmGetCompositionString(GCS_COMPSTR)` work **cross-process** against a
  foreground window you do not own, or only for your own HWND?
- Chromium and Electron use TSF (Text Services Framework), not IMM32, and expose
  their own text input. Does the IMM32 path report anything for Slack, VS Code,
  Chrome — i.e. exactly the apps that matter?
- UIPI: an elevated foreground window blocks the query entirely (§8).
- Does `Ctrl+Shift+Space` (§9's Windows default) survive an active composition?

The `windows` dependency is declared in `Cargo.toml` behind a
`cfg(target_os = "windows")` target block so the Windows session starts with a
compiling skeleton rather than a blank file.

## Not in scope

- Typing Japanese **into** aibo's own panel. That is **S10**, a different
  problem, and §9 calls it a market blocker on the critical path.
- Insert reliability generally. That is **S4** — run every S4 payload with no
  composition active first, then re-run `japanese` under the conditions this
  spike establishes.
