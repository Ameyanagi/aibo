//! Native accessibility adapters for aibo's custom-rendered iced windows.
//!
//! Iced 0.14 does not expose a semantic widget tree. aibo therefore builds an
//! AccessKit tree in `aibo-ui` and attaches it to iced's existing native view
//! through the same borrowed raw-window-handle boundary used by overlays.
//!
//! Adapter instances are thread-local. AppKit requires creation and mutation on
//! the main thread, while iced/winit owns every native window on that thread.
//! Keeping the adapter beside the event loop also avoids pretending AppKit
//! objects are `Send`.

use std::fmt;

use accesskit::{ActionRequest, TreeUpdate};
use raw_window_handle::{RawWindowHandle, WindowHandle};
use thiserror::Error;
use tokio::sync::mpsc::{Sender, error::TrySendError};

/// Stable identity for one aibo window's semantic tree.
///
/// This is deliberately independent from iced's opaque `window::Id`, so native
/// callbacks can safely carry it across threads without holding a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccessibilitySurface(pub u64);

/// A native accessibility action routed back to iced.
#[derive(Clone)]
pub struct AccessibilityEvent {
    /// Window whose semantic node received the action.
    pub surface: AccessibilitySurface,
    /// AccessKit action. Its data can contain user-entered text and must never
    /// be logged or formatted by infrastructure code.
    pub request: ActionRequest,
}

impl fmt::Debug for AccessibilityEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessibilityEvent")
            .field("surface", &self.surface)
            .field("action", &self.request.action)
            .field("target_tree", &self.request.target_tree)
            .field("target_node", &self.request.target_node)
            .field("data", &self.request.data.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// Failure to attach or update a native semantic tree.
#[derive(Debug, Error)]
pub enum AccessibilityError {
    /// The handle is not native to this target.
    #[error("expected a {expected} window handle, got {actual}")]
    UnsupportedHandle {
        /// Handle kind required by this target.
        expected: &'static str,
        /// Handle kind supplied by the caller.
        actual: &'static str,
    },
    /// AppKit adapter installation is main-thread-only.
    #[error("native accessibility attachment must run on the main thread")]
    MainThreadRequired,
    /// A semantic adapter was already attached to this surface.
    #[error("an accessibility adapter is already attached to surface {0:?}")]
    AlreadyAttached(AccessibilitySurface),
    /// An adapter was accessed from a thread other than the one that attached it.
    #[error("the accessibility adapter for surface {0:?} belongs to another thread")]
    WrongThread(AccessibilitySurface),
}

/// Attach a semantic tree to a borrowed iced/winit native window.
///
/// The window must still be hidden and must never have been focused. AccessKit
/// dynamically subclasses the AppKit view / Win32 window and requires this call
/// before the native accessibility client first observes the surface.
pub fn attach_accessibility(
    handle: WindowHandle<'_>,
    surface: AccessibilitySurface,
    initial_tree: TreeUpdate,
    events: Sender<AccessibilityEvent>,
) -> Result<(), AccessibilityError> {
    let raw = handle.as_raw();

    #[cfg(target_os = "macos")]
    {
        if objc2_foundation::MainThreadMarker::new().is_none() {
            return Err(AccessibilityError::MainThreadRequired);
        }
        if !matches!(raw, RawWindowHandle::AppKit(_)) {
            return Err(wrong_handle("AppKit", raw));
        }
        supported::attach(raw, surface, initial_tree, events)
    }

    #[cfg(target_os = "windows")]
    {
        if !matches!(raw, RawWindowHandle::Win32(_)) {
            return Err(wrong_handle("Win32", raw));
        }
        supported::attach(raw, surface, initial_tree, events)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (raw, surface, initial_tree, events);
        Ok(())
    }
}

/// Replace the latest full semantic snapshot for `surface`.
///
/// Sending full snapshots is intentional. aibo's windows contain tens, not
/// thousands, of nodes, and a complete snapshot lets activation after a long
/// inactive period initialize synchronously without a placeholder tree.
pub fn update_accessibility(surface: AccessibilitySurface, tree: TreeUpdate) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Err(error) = supported::update(surface, tree) {
        tracing::warn!(?surface, %error, "accessibility update rejected");
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = (surface, tree);
}

/// Tell AccessKit whether the native host window currently has focus.
pub fn set_accessibility_focus(surface: AccessibilitySurface, focused: bool) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Err(error) = supported::set_focus(surface, focused) {
        tracing::warn!(?surface, %error, "accessibility focus update rejected");
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = (surface, focused);
}

/// Detach and discard a window's semantic adapter.
pub fn detach_accessibility(surface: AccessibilitySurface) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Err(error) = supported::detach(surface) {
        tracing::warn!(?surface, %error, "accessibility detach rejected");
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = surface;
}

fn wrong_handle(expected: &'static str, raw: RawWindowHandle) -> AccessibilityError {
    AccessibilityError::UnsupportedHandle {
        expected,
        actual: raw_kind(raw),
    }
}

fn raw_kind(raw: RawWindowHandle) -> &'static str {
    match raw {
        RawWindowHandle::UiKit(_) => "UiKit",
        RawWindowHandle::AppKit(_) => "AppKit",
        RawWindowHandle::Orbital(_) => "Orbital",
        RawWindowHandle::OhosNdk(_) => "OhosNdk",
        RawWindowHandle::Xlib(_) => "Xlib",
        RawWindowHandle::Xcb(_) => "Xcb",
        RawWindowHandle::Wayland(_) => "Wayland",
        RawWindowHandle::Drm(_) => "Drm",
        RawWindowHandle::Gbm(_) => "Gbm",
        RawWindowHandle::Win32(_) => "Win32",
        RawWindowHandle::WinRt(_) => "WinRt",
        RawWindowHandle::Web(_) => "Web",
        RawWindowHandle::WebCanvas(_) => "WebCanvas",
        RawWindowHandle::WebOffscreenCanvas(_) => "WebOffscreenCanvas",
        RawWindowHandle::AndroidNdk(_) => "AndroidNdk",
        RawWindowHandle::Haiku(_) => "Haiku",
        _ => "unknown",
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod supported {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
    use std::thread::{self, ThreadId};

    use accesskit::{ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
    #[cfg(target_os = "macos")]
    use accesskit_macos::SubclassingAdapter as MacAdapter;
    #[cfg(target_os = "windows")]
    use accesskit_windows::{HWND, SubclassingAdapter as WindowsAdapter};
    use raw_window_handle::RawWindowHandle;

    use super::{
        AccessibilityError, AccessibilityEvent, AccessibilitySurface, Sender, TrySendError,
    };

    static OWNERS: LazyLock<SurfaceOwners> = LazyLock::new(SurfaceOwners::default);

    thread_local! {
        static ADAPTERS: RefCell<ThreadAdapters> =
            RefCell::new(ThreadAdapters::default());
    }

    #[derive(Default)]
    struct SurfaceOwners {
        inner: Mutex<HashMap<AccessibilitySurface, ThreadId>>,
    }

    impl SurfaceOwners {
        fn lock(&self) -> MutexGuard<'_, HashMap<AccessibilitySurface, ThreadId>> {
            self.inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn claim(&self, surface: AccessibilitySurface) -> Result<(), AccessibilityError> {
            let current = thread::current().id();
            let mut owners = self.lock();
            match owners.get(&surface) {
                Some(owner) if *owner != current => Err(AccessibilityError::WrongThread(surface)),
                Some(_) => Ok(()),
                None => {
                    owners.insert(surface, current);
                    Ok(())
                }
            }
        }

        /// `Ok(false)` means the surface has no adapter. That remains a benign
        /// no-op for compatibility; only crossing an established affinity is
        /// an error.
        fn is_current(&self, surface: AccessibilitySurface) -> Result<bool, AccessibilityError> {
            let current = thread::current().id();
            match self.lock().get(&surface) {
                Some(owner) if *owner != current => Err(AccessibilityError::WrongThread(surface)),
                Some(_) => Ok(true),
                None => Ok(false),
            }
        }

        fn release(&self, surface: AccessibilitySurface) {
            let current = thread::current().id();
            let mut owners = self.lock();
            if owners.get(&surface) == Some(&current) {
                owners.remove(&surface);
            }
        }
    }

    #[derive(Default)]
    struct ThreadAdapters {
        adapters: HashMap<AccessibilitySurface, AdapterState>,
    }

    impl Drop for ThreadAdapters {
        fn drop(&mut self) {
            // A thread-local native adapter cannot outlive its owner thread.
            // Remove its affinity records as the adapters are dropped so a
            // later window may safely reuse a surface identifier.
            for surface in self.adapters.keys().copied() {
                OWNERS.release(surface);
            }
        }
    }

    struct AdapterState {
        adapter: NativeAdapter,
        latest: Arc<Mutex<TreeUpdate>>,
    }

    enum NativeAdapter {
        #[cfg(target_os = "macos")]
        Mac(MacAdapter),
        #[cfg(target_os = "windows")]
        Windows(WindowsAdapter),
    }

    impl NativeAdapter {
        #[cfg_attr(target_os = "macos", allow(unsafe_code))]
        fn new(
            raw: RawWindowHandle,
            activation: impl 'static + ActivationHandler,
            actions: impl 'static + ActionHandler + Send,
        ) -> Self {
            #[cfg(target_os = "macos")]
            {
                let RawWindowHandle::AppKit(handle) = raw else {
                    unreachable!("the public boundary validates AppKit handles");
                };
                // SAFETY: `WindowHandle` keeps this NSView alive for the call,
                // and the returned adapter retains it for its own lifetime.
                // The caller also guarantees installation before first show.
                Self::Mac(unsafe { MacAdapter::new(handle.ns_view.as_ptr(), activation, actions) })
            }
            #[cfg(target_os = "windows")]
            {
                let RawWindowHandle::Win32(handle) = raw else {
                    unreachable!("the public boundary validates Win32 handles");
                };
                Self::Windows(WindowsAdapter::new(
                    HWND(handle.hwnd.get() as *mut _),
                    activation,
                    actions,
                ))
            }
        }

        fn update_if_active(&mut self, tree: TreeUpdate) {
            match self {
                #[cfg(target_os = "macos")]
                Self::Mac(adapter) => {
                    if let Some(events) = adapter.update_if_active(|| tree) {
                        events.raise();
                    }
                }
                #[cfg(target_os = "windows")]
                Self::Windows(adapter) => {
                    if let Some(events) = adapter.update_if_active(|| tree) {
                        events.raise();
                    }
                }
            }
        }

        fn set_focus(&mut self, focused: bool) {
            match self {
                #[cfg(target_os = "macos")]
                Self::Mac(adapter) => {
                    if let Some(events) = adapter.update_view_focus_state(focused) {
                        events.raise();
                    }
                }
                // The subclassed HWND receives native focus messages directly.
                #[cfg(target_os = "windows")]
                Self::Windows(_) => {
                    let _ = focused;
                }
            }
        }
    }

    #[derive(Clone)]
    struct InitialTree {
        latest: Arc<Mutex<TreeUpdate>>,
    }

    impl ActivationHandler for InitialTree {
        fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
            Some(
                self.latest
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            )
        }
    }

    #[derive(Clone)]
    struct ActionSink {
        surface: AccessibilitySurface,
        events: Sender<AccessibilityEvent>,
    }

    impl ActionHandler for ActionSink {
        fn do_action(&mut self, request: ActionRequest) {
            let event = AccessibilityEvent {
                surface: self.surface,
                request,
            };
            match self.events.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Closed(_)) => {
                    tracing::debug!("accessibility action channel closed");
                }
                Err(TrySendError::Full(_)) => {
                    // Never format the rejected event: SetValue can carry
                    // private user text or a destructive confirmation.
                    tracing::warn!("accessibility action queue saturated");
                }
            }
        }
    }

    pub(super) fn attach(
        raw: RawWindowHandle,
        surface: AccessibilitySurface,
        initial_tree: TreeUpdate,
        events: Sender<AccessibilityEvent>,
    ) -> Result<(), AccessibilityError> {
        OWNERS.claim(surface)?;
        ADAPTERS.with_borrow_mut(|thread_adapters| {
            if thread_adapters.adapters.contains_key(&surface) {
                return Err(AccessibilityError::AlreadyAttached(surface));
            }
            let latest = Arc::new(Mutex::new(initial_tree));
            let adapter = NativeAdapter::new(
                raw,
                InitialTree {
                    latest: Arc::clone(&latest),
                },
                ActionSink { surface, events },
            );
            thread_adapters
                .adapters
                .insert(surface, AdapterState { adapter, latest });
            Ok(())
        })
    }

    pub(super) fn update(
        surface: AccessibilitySurface,
        tree: TreeUpdate,
    ) -> Result<(), AccessibilityError> {
        if !OWNERS.is_current(surface)? {
            return Ok(());
        }
        ADAPTERS.with_borrow_mut(|thread_adapters| {
            let Some(state) = thread_adapters.adapters.get_mut(&surface) else {
                return Ok(());
            };
            *state
                .latest
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = tree.clone();
            state.adapter.update_if_active(tree);
            Ok(())
        })
    }

    pub(super) fn set_focus(
        surface: AccessibilitySurface,
        focused: bool,
    ) -> Result<(), AccessibilityError> {
        if !OWNERS.is_current(surface)? {
            return Ok(());
        }
        ADAPTERS.with_borrow_mut(|thread_adapters| {
            if let Some(state) = thread_adapters.adapters.get_mut(&surface) {
                state.adapter.set_focus(focused);
            }
            Ok(())
        })
    }

    pub(super) fn detach(surface: AccessibilitySurface) -> Result<(), AccessibilityError> {
        if !OWNERS.is_current(surface)? {
            return Ok(());
        }
        ADAPTERS.with_borrow_mut(|thread_adapters| {
            thread_adapters.adapters.remove(&surface);
        });
        OWNERS.release(surface);
        Ok(())
    }

    #[cfg(test)]
    mod affinity_tests {
        use std::sync::Arc;

        use super::*;

        #[test]
        fn surface_owner_rejects_a_different_thread() {
            let owners = Arc::new(SurfaceOwners::default());
            let surface = AccessibilitySurface(91);
            owners.claim(surface).unwrap();

            let other = Arc::clone(&owners);
            let error = std::thread::spawn(move || other.is_current(surface).unwrap_err())
                .join()
                .unwrap();
            assert!(matches!(error, AccessibilityError::WrongThread(s) if s == surface));
        }

        #[test]
        fn releasing_a_surface_allows_a_new_owner() {
            let owners = Arc::new(SurfaceOwners::default());
            let surface = AccessibilitySurface(92);
            owners.claim(surface).unwrap();
            owners.release(surface);

            let other = Arc::clone(&owners);
            std::thread::spawn(move || other.claim(surface).unwrap())
                .join()
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use accesskit::{Action, ActionData, ActionRequest, NodeId, TreeId};

    use super::{AccessibilityEvent, AccessibilitySurface};

    #[test]
    fn debug_redacts_action_data() {
        let event = AccessibilityEvent {
            surface: AccessibilitySurface(7),
            request: ActionRequest {
                action: Action::SetValue,
                target_tree: TreeId::ROOT,
                target_node: NodeId(9),
                data: Some(ActionData::Value("private confirmation".into())),
            },
        };

        let debug = format!("{event:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("private confirmation"));
    }
}
