/// Rust-side glue shared by every overlay window: reading back a window's
/// position after an OS-native drag finishes and persisting it. The
/// window-level Slint chrome (no-frame/always-on-top/locked/drag `TouchArea`)
/// is shared on the Slint side by `ui/common/overlay-shell.slint`'s
/// `OverlayShell` component instead — this module is only the Rust half
/// that can't be expressed there (config writes, the deferred re-read).
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_shell {
    use std::sync::Arc;
    use std::time::Duration;

    use slint::{ComponentHandle, Weak};

    use crate::overlay_registry::overlay_registry::OverlayKind;
    use crate::tray::tray::AppHandle;

    /// Deferred read-back of a window's position after an OS-native
    /// interactive drag (`WinitWindowAccessor::drag_window()`) finishes.
    /// `drag_window()` returns as soon as the mouse button is released, but
    /// the window's reported position can still lag one event-loop
    /// iteration behind its true final value at that exact instant, so the
    /// read happens 50ms later, after that last position update has landed.
    pub fn handle_drag_end<W>(weak: Weak<W>, handle: Arc<AppHandle>, kind: OverlayKind)
    where
        W: ComponentHandle + 'static,
    {
        slint::Timer::single_shot(Duration::from_millis(50), move || {
            let Some(w) = weak.upgrade() else { return };
            let scale = w.window().scale_factor();
            let pos = w.window().position().to_logical(scale);
            let (x, y) = {
                let mut cfg = handle.config.lock().unwrap();
                {
                    let win = kind.config_mut(&mut cfg);
                    win.x = pos.x.round() as i32;
                    win.y = pos.y.round() as i32;
                }
                cfg.save();
                let win = kind.config(&cfg);
                (win.x, win.y)
            };
            crate::settings_window::settings_window::sync_window_position(kind, x, y);
        });
    }
}
