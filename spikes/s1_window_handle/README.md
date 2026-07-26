# S1 — iced 0.14 → native window handle

Risk-register item **S1** (docs/plan.md §20; background in §8 "Overlay window" /
"Vibrancy or blur" and §9 "Mixed DPI and hot-plug").

> **Largely de-risked already**: `iced::window::run(id, f: impl FnOnce(&dyn
> Window))` hands you a `HasWindowHandle`, and iced 0.14 and `window-vibrancy`
> 0.8 both use `raw-window-handle` 0.6 — they compose directly. What remains:
> does an inserted `NSVisualEffectView` composite correctly behind iced's
> `CAMetalLayer`-backed view? Plus all-Spaces and acrylic on Windows.

## What this measures

One runnable window that, from inside `iced::window::run`, does five things and
reports what the OS actually accepted (every mutation is **read back**, never
assumed):

1. **Handle extraction** — does `iced::window::run` really yield a usable
   `raw-window-handle` 0.6 handle, and on which thread?
2. **Vibrancy compositing** — the actual open question. Two insertion
   strategies, selectable with `--strategy`:
   - `subview` — `window_vibrancy::apply_vibrancy` verbatim. It calls
     `view.addSubview_positioned_relativeTo(blur, Below, None)` on **iced's own
     view**, so the effect view becomes a *child* of the `CAMetalLayer`-backed
     view.
   - `sibling` — reparent: a plain `NSView` becomes the window's `contentView`,
     the `NSVisualEffectView` is added **first** and iced's `WinitView` on top,
     making them true siblings.
   - `none` — skip vibrancy, isolate the level / all-Spaces half.
3. **Floating level** — set `NSFloatingWindowLevel` natively and read it back,
   so "iced asked for `Level::AlwaysOnTop`" is distinguished from "AppKit
   accepted it".
4. **All-Spaces** — §9: *"the panel must join all Spaces or it appears on the
   wrong desktop when the user is in a fullscreen app."* Sets
   `CanJoinAllSpaces | Stationary | FullScreenAuxiliary` and reads the mask back.
5. **Is it an `NSPanel`?** — §8 claims winit only ever creates `NSWindow`. The
   report states which class it actually is.

On Windows the same binary calls `window_vibrancy::apply_acrylic` (which sets
`DWMWA_SYSTEMBACKDROP_TYPE` = `DWMSBT_TRANSIENTWINDOW` on build 22621+, exactly
what §8 asks for) and reports the result.

## Running it

```sh
cd spikes/s1_window_handle
cargo run                              # --strategy sibling, 30 s
cargo run -- --strategy subview        # the off-the-shelf window-vibrancy path
cargo run -- --strategy none           # level + all-Spaces only
cargo run -- --hold                    # stay up until killed
cargo run -- --seconds 15 --opaque     # non-transparent control
```

This is a **standalone cargo workspace** (`[workspace]` in its `Cargo.toml`), so
it builds without being a member of the root workspace.

## How to read the result

The text report only proves **the calls were accepted**. The compositing
question is answered **by looking at the window**, so put something high-contrast
behind it first (a text editor, a photo).

| Key | Meaning |
|---|---|
| `appkit.main_thread` | `false` here kills the whole approach — every AppKit call in the probe would be UB and `iced::window::run` would be unusable for native window work. |
| `handle.kind` | `AppKit` / `Win32`. Proves the version unification claim: one `raw-window-handle` 0.6 shared by iced and `window-vibrancy`. |
| `window.class` | §8 says winit only makes `NSWindow`. |
| `window.is_nspanel` | `false` confirms §8: no non-activating panel without swizzling. |
| `level.is_floating` | `true` ⇒ `Level::AlwaysOnTop` really is `kCGFloatingWindowLevel`. |
| `collection_behavior.*` | `true` on all three ⇒ the all-Spaces requirement in §9 is reachable from the iced handle. |
| `sibling.container.subviews` | Must read `NSVisualEffectView,WinitView` — effect view first, i.e. **below**. |
| **the window itself** | blur visible **and** text visible ⇒ PASS. Blur visible but text gone ⇒ the effect view is in front. Text visible but no blur ⇒ the effect view is behind an opaque clear. |

## Measured on this machine — 2026-07-26, macOS 26.5.1 (arm64), iced 0.14.0, window-vibrancy 0.8.0

Both strategies, screenshotted against a terminal full of text.

```
strategy                                  subview | sibling
thread.is_main                            main
handle.ok                                 true
handle.kind                               AppKit
appkit.main_thread                        true
view.class                                WinitView
view.subviews.before                      <none>
window.found                              true
window.class                              NSKVONotifying_WinitWindow
window.is_nspanel                         false
window.style_mask                         0x4          (NSWindowStyleMaskResizable only)
window.level.before                       3            (already floating: iced's AlwaysOnTop)
level.readback                            3
level.is_floating                         true
collection_behavior.before                0x0
collection_behavior.after                 0x111
collection_behavior.can_join_all_spaces   true
collection_behavior.fullscreen_auxiliary  true
collection_behavior.stationary            true
```

strategy-specific:

```
subview:   view.subviews.after            NSVisualEffectViewTagged0.8.0
sibling:   sibling.iced_view_is_contentview   true
           sibling.container.subviews         NSVisualEffectView,WinitView
           sibling.contentview.after          NSView
```

### The visual result, which is the actual answer

| Strategy | Blur behind the window | iced's UI visible | Verdict |
|---|---|---|---|
| `subview` (`window-vibrancy` as shipped) | **yes** | **NO — completely hidden** | **FAIL** |
| `sibling` (reparented) | **yes** | **yes** | **PASS** |

## Findings — what this changes in the plan

**S1's remaining question is answered, and the off-the-shelf answer is wrong.**

1. **`iced::window::run` works exactly as §20 describes.** The closure runs on
   the main thread (`MainThreadMarker::new()` succeeds), the handle is
   `RawWindowHandle::AppKit`, and `iced` 0.14 + `window-vibrancy` 0.8 +
   `raw-window-handle` 0.6 unify onto one version with no shim. Nothing about
   `windowNumber` matching is needed. This half of S1 is closed.
2. **`window_vibrancy::apply_vibrancy` is not usable on an iced window as-is.**
   It adds the `NSVisualEffectView` as a **subview of iced's `WinitView`**, and
   in AppKit a layer-backed view's own layer is the *parent* of its subviews'
   layers — so the effect view can only ever draw **in front of** the
   `CAMetalLayer`. Observed: a beautifully blurred rectangle with the entire UI
   erased. The call returns `Ok(())`; there is no error to detect this by.
3. **The fix is a reparent, and it works.** Make a plain `NSView` the window's
   `contentView`, add the `NSVisualEffectView`, then add `WinitView` above it.
   `contentView.subviews` reads `NSVisualEffectView,WinitView` and the window
   renders blur *and* UI. So **vibrancy is available to aibo**, but through
   ~30 lines of `objc2` in `aibo-platform`, not through the crate the plan pins.
   `window-vibrancy` remains useful for its Windows half and for its material
   enums.
4. **Preconditions that are easy to lose.** Vibrancy is invisible unless *all*
   of these hold: `window::Settings.transparent = true`, `NSWindow.setOpaque(false)`,
   `backgroundColor = NSColor.clearColor`, and iced's theme
   `background_color = Color::TRANSPARENT`. Any one of them opaque and the blur
   disappears with no error. Worth an assertion in `aibo-platform`.
5. **All-Spaces is free.** `collectionBehavior` went `0x0` → `0x111`
   (`CanJoinAllSpaces | Stationary | FullScreenAuxiliary`) and read back intact.
   §9's fullscreen-Space requirement does not need any unsupported technique —
   just the handle S1 proved you can get. Note `CanJoinAllSpaces` **alone** is
   not enough for the fullscreen case; `FullScreenAuxiliary` is the one that
   lets the panel appear over another app's fullscreen Space.
6. **§8's `NSPanel` claim is confirmed**: the window is
   `NSKVONotifying_WinitWindow`, not any `NSPanel` subclass. Non-activating
   panel behaviour still requires the unsupported swizzle, and §20's note stands
   — the real alternative is `iced_wgpu` + a custom shell.
7. **`Level::AlwaysOnTop` really is level 3** (`NSFloatingWindowLevel`) —
   `window.level.before` was already `3` before the probe set it. §8 confirmed.

### SPIKEs this leaves open

- **The reparent is unsupported by winit.** Verify before shipping: resize and
  scale-factor changes across displays (§9 mixed-DPI), mouse hit-testing, and
  **IME** — `NSTextInputClient` is attached to `WinitView`'s input context and
  moving the view could break candidate placement. That interacts directly with
  **S10**, which §20 puts on the critical path. If reparenting renders correctly
  but breaks Japanese input, take `iced_wgpu` + custom shell instead.
- **`NSVisualEffectState::Active` vs `FollowsWindowActiveState`.** This spike
  forces `Active` because an overlay panel is usually the *inactive* window and
  the default would desaturate it exactly then. The macOS analogue of §8's
  Windows acrylic-deactivation warning; confirm it looks right in the real
  design (§16).
- **Windows is entirely unmeasured here** — this machine is macOS. Not covered:
  `DWMWA_USE_IMMERSIVE_DARK_MODE` (§8: windows default to light regardless of
  system setting, and `window-vibrancy` does not set it), acrylic's neutral-colour
  fallback on deactivation against an always-inactive panel, and whether a
  `WS_EX_TOOLWINDOW|WS_EX_TOPMOST` panel can take text input at all.
