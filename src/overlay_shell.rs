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

    /// How often to re-check the window's true position while a drag is in
    /// progress.
    const POLL_INTERVAL_MS: u64 = 16;
    /// How long the position has to hold steady before it's treated as the
    /// drag's real resting point, rather than a mid-drag position the mouse
    /// is just passing through.
    const STABLE_HOLD_MS: u64 = 150;
    /// Safety net in case the position never settles — save whatever the
    /// last reading was rather than polling forever. Generous on purpose:
    /// a real drag can reasonably take a couple of seconds.
    const MAX_WAIT_MS: u64 = 8000;

    /// Watches a window's true position after its drag TouchArea reports a
    /// press and saves it once the drag has visibly finished.
    ///
    /// The naive approach — read the position once, synchronously, right
    /// after this is called — doesn't work: on Linux/X11 (xfwm4 confirmed),
    /// `WinitWindowAccessor::drag_window()` doesn't block for the drag's
    /// duration the way it does on Windows. Per the EWMH `_NET_WM_MOVERESIZE`
    /// convention it's built on, the client just hands the move off to the
    /// window manager and returns immediately — so `handle_drag_end` (called
    /// right after `drag_window()` returns, from the TouchArea's `pressed`
    /// callback, see e.g. `overlay_history.rs`) actually runs at the *start*
    /// of the drag, not the end, while the mouse button is still down and
    /// the window hasn't moved yet. Any single read at that point — however
    /// long it's deferred, however it's sourced (winit's cache or, as
    /// `overlay_draw::true_window_position` does, a live X server query) —
    /// just captures wherever the window already was sitting *before* this
    /// drag, which is the previous drag's endpoint. That's the actual
    /// mechanism behind the "always exactly one drag behind" symptom this
    /// module has chased before: it was never really about staleness
    /// duration, it was about reading at the wrong moment entirely.
    ///
    /// Since there's no reliable "drag ended" event to key off instead (the
    /// window manager owns the pointer grab for the whole move, so the
    /// client-side `TouchArea` never reliably sees the matching release),
    /// this polls the window's true position on a short interval and treats
    /// it as final once it's held the same value continuously for
    /// `STABLE_HOLD_MS` — long enough that a mouse still actively dragging
    /// the window won't spuriously look "done" between two adjacent samples,
    /// but short enough to feel instant once the user actually lets go.
    pub fn handle_drag_end<W>(weak: Weak<W>, handle: Arc<AppHandle>, kind: OverlayKind)
    where
        W: ComponentHandle + 'static,
    {
        poll_until_stable(weak, handle, kind, None, 0);
    }

    fn read_position<W>(w: &W) -> (f32, f32)
    where
        W: ComponentHandle,
    {
        let window = w.window();
        let scale = window.scale_factor();
        if let Some((x, y)) = crate::overlay_draw::overlay_draw::true_window_position(window) {
            return (x as f32 / scale, y as f32 / scale);
        }
        let pos = window.position().to_logical(scale);
        (pos.x, pos.y)
    }

    /// `stable` tracks the most recently seen position and how long ago (in
    /// elapsed poll time) it first appeared, so a run of identical readings
    /// can be timed even though each tick only sees one sample.
    fn poll_until_stable<W>(
        weak: Weak<W>,
        handle: Arc<AppHandle>,
        kind: OverlayKind,
        stable: Option<((f32, f32), u64)>,
        elapsed_ms: u64,
    ) where
        W: ComponentHandle + 'static,
    {
        slint::Timer::single_shot(Duration::from_millis(POLL_INTERVAL_MS), move || {
            let Some(w) = weak.upgrade() else { return };
            let current = read_position(&w);
            let elapsed_ms = elapsed_ms + POLL_INTERVAL_MS;

            let stable = match stable {
                Some((value, since_ms)) if value == current => Some((value, since_ms)),
                _ => Some((current, elapsed_ms)),
            };
            let (_, since_ms) = stable.unwrap();
            let held_ms = elapsed_ms - since_ms;

            if held_ms >= STABLE_HOLD_MS || elapsed_ms >= MAX_WAIT_MS {
                save_position(kind, &handle, current);
            } else {
                poll_until_stable(weak, handle, kind, stable, elapsed_ms);
            }
        });
    }

    fn save_position(kind: OverlayKind, handle: &Arc<AppHandle>, pos: (f32, f32)) {
        let (x, y) = {
            let mut cfg = handle.config.lock().unwrap();
            {
                let win = kind.config_mut(&mut cfg);
                win.x = pos.0.round() as i32;
                win.y = pos.1.round() as i32;
            }
            cfg.save();
            let win = kind.config(&cfg);
            (win.x, win.y)
        };
        crate::settings_window::settings_window::sync_window_position(kind, x, y);
    }
}
