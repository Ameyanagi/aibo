# S4 — Insert reliability

> Paste-and-restore vs `SendInput`/`CGEventPost` across the app set, Unicode and
> 5 KB inserts. **Does clipboard save/restore round-trip?**
> **If it fails: paste-only, always ask before clobbering the clipboard.** — §20

Related plan sections: §8 (the insert row, secure input mode, the enigo defect
list), §12 (clipboard hygiene), §13 (undo after inserting into someone else's
app), §20 (the S4 row and the clipboard exclusion markers).

## ⚠ Read this before running anything

**This binary types into whatever application is frontmost.** It will paste 5 KB
of filler into your production Slack if that is what has focus when the countdown
ends. It also overwrites your clipboard, and — as this spike is partly designed
to prove — cannot always put it back.

Point it at `testapps/aibo-axtarget` or a scratch document. Do the `roundtrip`
command *first*, on a machine where losing the clipboard does not matter.

macOS only for the read-back and the flavour listing. The Windows half —
`SendInput` per UTF-16 unit, `GetClipboardSequenceNumber`, UIPI against elevated
windows, and the `CanIncludeInClipboardHistory` / `CanUploadToCloudClipboard`
markers that §20 says need a **serialised DWORD 0** rather than a bare presence
marker — is marked `SPIKE: S4` in `src/clipboard.rs` and is a separate session on
a Windows box.

## What the operator does

```sh
cd spikes/s4_insert

# 0. See what will be typed, and why each payload exists.
cargo run -- payloads

# 1. THE clipboard question, on its own. Put a SCREENSHOT on the clipboard first.
cargo run -- roundtrip

# 2. Launch the controlled target in another terminal.
(cd ../../testapps/aibo-axtarget && cargo run)

# 3. One payload, one method. Focus an EMPTY field during the countdown.
cargo run -- insert --payload ascii  --method paste
cargo run -- insert --payload ascii  --method synthetic

# 4. The whole sweep. It pauses between payloads so you can clear the field.
cargo run -- insert --payload all --method paste     --out ../../s4-results.jsonl
cargo run -- insert --payload all --method synthetic --out ../../s4-results.jsonl
```

**Focus an empty field.** Verification compares the *whole* field value against
the payload; a field with existing text reports "payload present but the field is
not only the payload", which is a weaker result.

### The app list

Same list as S2, and for the same reason — the two spikes produce one matrix
between them. Run at least: `aibo-axtarget`, TextEdit, Mail, Safari
(`<textarea>` and `contenteditable`), Chrome, Slack, VS Code, Notion, Word,
Terminal, and **the secure field in `aibo-axtarget`**.

The secure field is not a curiosity. §8:

> `IsSecureEventInputEnabled()` — password fields, Terminal, and password
> managers block keystroke synthesis and AX reads. Other apps can leave it stuck
> **globally**. […] Paste-based insert fails silently with no diagnosable cause
> unless you detect and explain it.

Focus it, run `insert --payload ascii --method paste`, and record *exactly* what
happens — most likely nothing at all, with no error. That silence is the finding.

## Reading the output

- `read-back: EXACT MATCH` — verified over AX. Trustworthy.
- `read-back: MISMATCH at char N` plus the surrounding context. **N is the
  diagnosis.** A mismatch at char 0 on `leading-newline` is §8's named enigo bug.
  A mismatch at a multiple of 20 on `newline-at-chunk-boundary` is the same bug
  surviving a naive guard. A clean truncation at 4096 is a buffer cap in the
  target app.
- `read-back: UNAVAILABLE` — AX could not read the field. **This is not a
  failure.** Look at the screen and judge it yourself; the harness prints what to
  look for.
- `insert took N ms` — a 5 KB synthetic type is thousands of events. §15 has a
  performance budget; an insert that takes two seconds is a product problem even
  when the text is correct.

## What to record — the go/no-go note

```
S4 — insert reliability
Date / macOS version / hardware:
enigo version:                    0.6.x
Accessibility granted to the s4 binary:   yes | no

--- The clipboard question (§20) ---
Clipboard held (flavours):
  text round-tripped:                        yes | no
  non-text flavours survived:                yes | no | n/a
  empty clipboard restorable:                yes | no
  clipboard MANAGER captured the probe:      yes | no
  changeCount bumps per insert:              __

--- Per app, per method ---
App / surface:
  paste      ascii / japanese / emoji / combining / leading-newline /
             chunk-boundary / multiline / whitespace / 5kb / 5kb-unicode
             __ / __ / __ / __ / __ / __ / __ / __ / __ / __
  synthetic  (same order)
             __ / __ / __ / __ / __ / __ / __ / __ / __ / __
  5 KB paste time:        __ ms      5 KB synthetic time:   __ ms
  newline SENT the message instead of inserting:   yes | no
  app froze or lagged visibly:                     yes | no

--- Secure input mode ---
  focused the secure field, paste:      silently did nothing | error | inserted
  focused the secure field, synthetic:  silently did nothing | error | inserted
  did secure input stay stuck after the app lost focus:   yes | no

--- Decision (§20) ---
  [ ] paste-and-restore is the default, synthetic for short inserts (plan unchanged)
  [ ] PASTE-ONLY. §20's fallback. Synthetic is not reliable enough to ship.
  [ ] paste-only AND always ask before clobbering the clipboard
  Apps where NEITHER method works (Complete is copy-to-clipboard there):
```

## The three things this spike must settle

1. **Is `synthetic` viable at all on macOS?** §8's list against enigo is long:
   20-character chunking, silent failure on chunks starting with a newline,
   keydown-only events with no delivery confirmation, an inter-event delay
   applied only on `Drop`, open Unicode bugs where emoji type the wrong
   character, and a crate that self-describes as early alpha. If
   `newline-at-chunk-boundary` and `emoji` both fail, the answer is paste-only
   and §7's `InsertMethod` shrinks to one variant.
2. **Is save/restore honest enough to do without asking?** If a screenshot on the
   clipboard does not survive, the product cannot silently borrow the clipboard,
   and §17's onboarding needs a consent step that is not currently in the plan.
3. **Does insert failure have a detectable signature?** §13 requires the panel to
   tell the user what happened. Right now the most likely failure — secure input
   mode — produces *nothing*: no error, no event, no exception. If this spike
   cannot find a way to detect it before inserting, §13's "explain the failure"
   promise cannot be kept and the fallback is "always show the text in the panel
   so it can be copied by hand".

## Not in scope

- **`restore_focus` and target validation.** §8 requires confirming the target
  regained focus before pasting, and re-validating pid, window, focused element
  and a hash of the selection at insert time — *"an unconfirmed restore races and
  pastes into the wrong window, which is the most damaging bug this product can
  ship."* This spike deliberately does the unsafe thing (type into whatever is
  frontmost) because it is measuring the insert primitive, not the product's
  safety rail. Do not read a green matrix here as evidence that insert is safe.
- IME composition. That is **S7**; §9 rule 2 says a synthetic paste during
  composition corrupts the buffer, so run every payload here with no composition
  active, then re-run `japanese` under S7's conditions.
- Undo. §11 is explicit that undo is weaker than an earlier draft claimed.
