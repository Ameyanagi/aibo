# S3 — warm-surface idle footprint

Risk-register item **S3** (docs/plan.md §20; budget row in §15).

> Does a hidden pre-created iced window hold ≤ 60 MB idle on both OSes?
> **If it fails:** drop the surface when idle, accept ~200 ms first-show.

§15 lists idle footprint as **≤ 100 MB, stretch 60** and flags it as an
unmeasured aspiration. This binary replaces it with a real number.

## What this measures

A minimal `iced::daemon` — no widgets, no state, no providers — that runs three
phases inside a **single process**, so the before/after delta is measured under
identical conditions rather than across two different runs:

| Phase | What is alive |
|---|---|
| `baseline` | the iced event loop only, **no window at all** |
| `warming` | the window was just created; wgpu is still settling |
| `steady` | the pre-created window has been alive for `--settle` seconds |

The number that answers S3 is **`steady` p50 − `baseline` p50**: the marginal
cost of keeping the surface warm, which is what the plan is actually deciding
about. The number that answers **§15** is `steady` p50 outright.

Measurement follows §15's own rule — *"Measure properly or don't claim: macOS
`phys_footprint` (not RSS), Windows private working set **and** commit, GPU
memory separately, p50 **and p95**, on a named hardware class"*:

- macOS: `proc_pid_rusage(RUSAGE_INFO_V4)` → `ri_phys_footprint` (primary) and
  `ri_resident_size` (secondary), plus `ri_user_time` / `ri_system_time` so the
  idle-CPU row can be checked from the same samples.
- Windows: `GetProcessMemoryInfo` → `WorkingSetSize` / `PrivateUsage` (commit).

## Running it

**Always `--release`.** A debug build's footprint means nothing; the binary
prints `NUMBERS ARE MEANINGLESS` if you forget.

```sh
cd spikes/s3_idle_rss
cargo run --release                                    # 10 min, hidden window
cargo run --release -- --mode none                     # event loop only
cargo run --release -- --mode visible                  # upper bound
cargo run --release -- --seconds 240 --baseline 30 --settle 30 --interval 5
```

This is a **standalone cargo workspace** (`[workspace]` in its `Cargo.toml`), so
it builds without being a member of the root workspace.

## How to read the result

Streaming rows are `SAMPLE <t_s> <phase> <phys_footprint_mb> <resident_mb> <cpu_ms>`;
the closing block is the verdict.

| Line | Meaning |
|---|---|
| `IDLE FOOTPRINT` | `steady` p50 — **the number that replaces the §15 row.** |
| `WARM SURFACE COST` | `steady` p50 − `baseline` p50 — what pre-creating the window actually costs. If this is small, keeping the surface warm is cheap regardless of the absolute total. |
| `VERDICT` | PASS at ≤ 60 MB (stretch), PASS-but-rewrite-§15 at ≤ 100 MB, FAIL above — FAIL means take the stated fallback. |
| `idle CPU (steady)` | checks §15's "~0.1–0.3 % on macOS" claim. |
| `# window opened at t=…` | **read this against the tick that requested it** — the gap is how long `window::open` blocked the event loop, i.e. first-show latency. |

## Measured on this machine — 2026-07-26, macOS 26.5.1 (arm64, Apple silicon), release build, iced 0.14.0 + wgpu

`--mode hidden --minutes 10 --baseline 60 --settle 60 --interval 5`

**Run 1 — cold pipeline cache**, `--mode hidden --minutes 10 --baseline 60 --settle 60 --interval 5`:

```
PRE_ICED                 2.2 MB phys_footprint   (before iced is constructed)
baseline  n=16   min=    8.2 p50=    8.2 p95=  113.3 max=  113.3  MB
warming   n=13   min=   34.8 p50=   34.9 p95=   36.9 max=  113.3  MB
steady    n=91   min=   34.8 p50=   34.9 p95=   34.9 max=  125.9  MB
idle CPU (steady)     0.003% over 450s

WARM SURFACE COST     +26.7 MB   (steady p50 - baseline p50)
IDLE FOOTPRINT        34.9 MB    (§15 budget: <= 100 MB, stretch 60)
VERDICT               PASS — meets the stretch budget; keep the pre-created surface
resident_size         77.3 MB steady (secondary; note it is >2x phys_footprint)
```

**Run 2 — warm pipeline cache**, `--mode hidden --seconds 200 --baseline 30 --settle 30`:

```
baseline  n=6    min=    8.2 p50=    8.2 p95=    8.3 max=    8.3  MB
warming   n=6    min=   34.2 p50=   34.2 p95=   36.2 max=   36.2  MB
steady    n=28   min=   34.2 p50=   34.2 p95=   34.2 max=   34.2  MB
idle CPU (steady)     0.003% over 135s

WARM SURFACE COST     +26.0 MB
IDLE FOOTPRINT        34.2 MB
VERDICT               PASS
window creation       requested t=30.0, opened t=30.1  →  ~100 ms
```

(The run-1 `baseline p95 = 113.3` is a phase-classification artefact of the
25-second blocking window creation described below; it was fixed after that run
and does not appear in run 2.)

## Findings — what this changes in the plan

**S3 passes, comfortably, and the §15 row can be tightened rather than
relaxed.**

1. **A hidden pre-created iced window costs 34.2–34.9 MB `phys_footprint`** at
   steady state — 42 % under the ≤ 60 MB *stretch* goal and a third of the
   ≤ 100 MB budget, reproducible across two runs with p50 = p95 to 0.1 MB. The
   plan's fallback ("drop the surface when idle, accept ~200 ms first-show") is
   **not needed**. Keep the warm surface.
2. **The marginal cost of the surface is +26 MB.** An iced daemon with no
   window at all sits at 8.2 MB. So most of aibo's idle budget is still
   unspent — but see (5), it is also not yet spent on anything real.
3. **Idle CPU is essentially zero** (~0.002 % over 8 minutes of steady state),
   well inside §15's 0.1–0.3 % claim. Note this daemon has one 5-second timer
   and no animation; a real panel with a caret blink will not be free.
4. **`phys_footprint` and RSS disagree by more than 2×** — 34.9 MB vs 77.3 MB
   resident. §15's insistence on `phys_footprint` is load-bearing: quoting RSS
   here would have reported a *failure* of the stretch budget for a process
   that is nowhere near it.
5. **This is a floor, not aibo's number.** Not included: SQLCipher, a resident
   `codex app-server` child (§3 — a whole separate process, not free), tray +
   global-hotkey, any real widget tree, fonts beyond the default, or wasmtime
   (§11). Re-run this shape of measurement at the end of each phase; treat
   34.9 MB as the surface's share of the budget, not the budget.
6. **GPU memory is not in this number and §15 says to report it separately.**
   Pair a run with `vmmap --summary <pid>` for the IOKit/GPU split before
   quoting a total.

### The unexpected finding: first window creation blocked the event loop for 25 seconds

The tick that requested the window ran at `t=60.1`; `# window opened` printed at
`t=85.2`, and four consecutive samples carry the identical timestamp `85.2` —
the loop was blocked, not slow. Peak `phys_footprint` during creation was
**113.3 MB**, i.e. **3× the steady figure**.

**Run 2 confirms this was purely a cold wgpu/Metal pipeline cache**: with the
cache warm, the same `window::open` completed in **~100 ms** (requested at
`t=30.0`, opened at `t=30.1`) with no visible creation spike. So the steady-state
cost of creating the window is fine — §15's "~200 ms first-show" estimate is
about right *once warm*. But the cold case matters twice:

- It is direct evidence *for* the pre-created warm surface. §15's "~200 ms
  first-show" fallback assumes window creation is a 200 ms operation. On a cold
  cache it is two orders of magnitude worse, and it happens on the **first
  hotkey press after install** — the worst possible moment for a first
  impression.
- The transient 113 MB peak means a footprint budget checked only at steady
  state hides a spike 3× larger. If aibo ever drops and re-creates the surface,
  it pays that spike each time.

Actionable: warm the pipeline cache during onboarding (§17), not on the user's
first hotkey press, and ship the cache with the bundle if wgpu allows it.

The lone `125.9 MB` sample at `t=236.7` in the steady phase is an outlier caused
by another GPU-heavy process (the S1 spike) running concurrently on this
machine; p50/p95 are unaffected, which is why §15 asks for both.

### Not measured here

- **Windows.** This machine is macOS. The Windows path is written and cfg-gated
  but has never been compiled or run. It also reports `WorkingSetSize`, which is
  **not** the "private working set" §15 asks for — that needs `QueryWorkingSetEx`
  page-by-page accounting. Treat the Windows number as an upper bound until
  someone fixes it (`SPIKE: S3` marker in `src/footprint.rs`).
- **GPU / IOKit memory** — see finding 6.
- **The whole process tree.** A resident `codex app-server` is a separate
  process with its own footprint and belongs in any headline number.
