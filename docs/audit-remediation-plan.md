# Repository Audit Remediation Plan

This plan consolidates the five parallel audits of UI/UX, accessibility,
security, code quality, and testing/performance. Work is ordered by user harm
and exploitability rather than by subsystem.

## Completion criteria

The remediation is complete when:

- untrusted provider, clipboard, file, and tool data is bounded and cannot
  grant authority;
- credentials and history keys are protected by the operating system and no
  secret-bearing value reaches diagnostics;
- cancellation, timeouts, and application shutdown terminate complete child
  process trees;
- every user-visible action either works, explains why it is unavailable, or is
  absent;
- the panel remains keyboard-usable, IME-safe, selectable, localized in English
  and Japanese, and legible at WCAG AA contrast;
- the root-to-provider event path has bounded memory under a hostile or stalled
  producer;
- the full workspace passes formatting, tests, strict Clippy, dependency
  policy, and macOS/Windows target checks.

## Phase 1 — P0 security and data integrity

1. Bound HTTP bodies, SSE frames, streamed output, tool-call assembly, shell
   output, and channel fan-out.
2. Treat captured selection, field, clipboard, file, MCP, and tool-result
   content as structurally fenced untrusted input.
3. Authorize canonical paths at approval time and revalidate them immediately
   before execution to close symlink and rename races.
4. Validate approval provenance in the backend; require typed confirmation for
   destructive actions.
5. Store credentials in the OS credential store, migrate legacy plaintext
   values, and keep plaintext storage behind an explicit development-only
   marker.
6. Make history creation a user gesture, persist it atomically, and reveal a
   new recovery code exactly once.
7. Manage complete subprocess trees with cancellation, initialization,
   request, and shutdown deadlines.

Acceptance: adversarial size, taint, path-race, cancellation, timeout, secret
redaction, and key-loss tests pass.

## Phase 2 — P0/P1 lifecycle and reliability

1. Enforce one active panel session and discard late capture/model events.
2. Cancel panel and Do-agent tasks deterministically.
3. Use an OS file lock for one application instance and focus the existing
   instance from later launches.
4. Move blocking provider/config/history initialization off the UI thread.
5. Replace root/UI unbounded queues with bounded delivery and coalesce adjacent
   text deltas without losing transcript content.
6. Persist history exchanges transactionally and write protected files
   atomically with restrictive permissions.

Acceptance: saturation, stale-session, rapid reopen, shutdown, crash recovery,
and single-instance tests pass without leaks or unbounded growth.

## Phase 3 — P1 UI/UX and accessibility

1. Make answer and device-code text selectable and expose reliable copy
   actions.
2. Fix shortcut scope, platform-specific shortcut labels, destructive-action
   confirmation, disabled/dead actions, and long-answer footers.
3. Add first-run provider onboarding and explicit encrypted-history setup.
4. Preserve IME preedit, restore logical focus, announce important state
   changes, honor reduced motion, and configure the native overlay without
   stealing focus.
5. Raise text/state contrast to at least 4.5:1, use 44-point interaction
   targets, and add non-color selected-state indicators.
6. Localize visible English/Japanese strings and make truncation grapheme-safe.

Acceptance: keyboard-only flows, Japanese IME input, screen-reader
announcements, contrast/target tests, selectable transcripts, and native panel
behavior are verified.

## Phase 4 — P1 code quality and delivery

1. Remove unnecessary default features and make platform dependencies
   target-specific.
2. Add CI for formatting, tests, strict Clippy, dependency policy, and both
   shipping targets.
3. Keep tool execution behind a closed adapter whose declared approval kind
   matches its actual authority.
4. Update stale comments and tests when an audited placeholder becomes
   production behavior.
5. Record remaining framework or product limitations as explicit residual
   risks rather than silent TODO behavior.

Acceptance: CI-equivalent commands pass locally, `git diff --check` is clean,
and no high-severity audit item is left without an owner or documented
boundary.

## Final verification order

1. `cargo fmt --all -- --check`
2. `cargo test --workspace --all-targets`
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo deny check`
5. macOS and Windows target checks used by CI
6. native smoke test of onboarding, settings, Ask, Do, task cancellation,
   diagnostics recovery, and keyboard focus
7. review of the final diff for accidental secret logging, unrelated changes,
   stale TODOs, and generated artifacts

## Residual-risk policy

Anything that cannot be completed because of an upstream framework limit or an
unverified external protocol must be fail-closed, hidden from the UI when
nonfunctional, and listed in the final audit report with an owner and a concrete
validation step.
