/// Shared color-parsing helper used across the overlay windows and the
/// Settings dialog's trigger action editor.
///
/// Everything else that used to live here (premultiplied-BGRA compositing,
/// GDI-text-to-DIB glyph rendering, `make_font`, `apply_lock_style`) was the
/// pixel-blit machinery the old raw-Win32 overlay windows needed to paint
/// themselves into a `WS_EX_LAYERED` DIB by hand. The Slint windows that
/// replaced them (`overlay.rs`, `overlay_history.rs`, `overlay_dps.rs`) let
/// the renderer handle compositing/text/fonts natively, so none of that is
/// needed anymore — removed as part of the Phase 4 cutover rather than kept
/// as dead code.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_draw {
    /// Parse `#RRGGBB` or `RRGGBB` → `0x00RRGGBB`.
    pub fn parse_hex_color(s: &str) -> Option<u32> {
        let s = s.trim_start_matches('#');
        if s.len() == 6 {
            let r = u32::from_str_radix(&s[0..2], 16).ok()?;
            let g = u32::from_str_radix(&s[2..4], 16).ok()?;
            let b = u32::from_str_radix(&s[4..6], 16).ok()?;
            Some((r << 16) | (g << 8) | b)
        } else {
            None
        }
    }

    // ── Taskbar visibility ──────────────────────────────────────────────────
    //
    // The alert/history/DPS-meter/merged overlay windows are transient HUD
    // elements layered over the game, not real application windows a user
    // would want to Alt-Tab or taskbar-click to individually (unlike the
    // Settings dialog, which keeps its normal taskbar entry).
    //
    // Two mechanisms combine to hide them, in order of how much they're
    // relied on:
    //
    // 1. (Linux, primary) `tray::run()` installs a `winit_window_attributes_hook`
    //    that tags every window `_NET_WM_WINDOW_TYPE_UTILITY` *before* it's
    //    ever mapped — set as an initial creation attribute, not a
    //    post-creation call, so there's no window-manager-processing race to
    //    lose. Confirmed live against xfwm4: a Utility-typed window gets
    //    `_NET_WM_STATE_SKIP_TASKBAR`/`SKIP_PAGER` auto-applied by the WM as
    //    soon as it sees the type, and this is standard EWMH convention
    //    other desktops (GNOME/Mutter, KDE/KWin, etc.) follow too, not an
    //    xfwm4-specific behavior. Settings is exempted via
    //    `suppress_utility_window_hint` around its one `.show()` call site.
    // 2. (below, both platforms) An explicit, deferred post-show request —
    //    Windows' `set_skip_taskbar` and X11's `_NET_WM_STATE_SKIP_TASKBAR`
    //    ClientMessage. Belt-and-suspenders for window managers that don't
    //    treat Utility-type as taskbar-exempt by default.

    use std::cell::Cell;

    thread_local! {
        static SUPPRESS_UTILITY_HINT: Cell<bool> = const { Cell::new(false) };
    }

    /// Wraps a window's `::new()` call to opt it out of mechanism 1 above —
    /// used around the Settings window's `SettingsWindow::new()` (its only
    /// creation site) so it keeps a normal taskbar entry. Must wrap `new()`,
    /// not `.show()`: Slint applies the `winit_window_attributes_hook`
    /// while building the window adapter inside `new()` itself. Only
    /// meaningful on Linux; a harmless no-op wrapper elsewhere.
    pub fn suppress_utility_window_hint<R>(f: impl FnOnce() -> R) -> R {
        SUPPRESS_UTILITY_HINT.with(|c| c.set(true));
        let r = f();
        SUPPRESS_UTILITY_HINT.with(|c| c.set(false));
        r
    }

    /// Read by the `winit_window_attributes_hook` installed in `tray::run()`.
    #[cfg(target_os = "linux")]
    pub(crate) fn utility_window_hint_suppressed() -> bool {
        SUPPRESS_UTILITY_HINT.with(|c| c.get())
    }

    /// Takes a `Weak` handle and defers 50ms, rather than acting on
    /// `&slint::Window` immediately — on X11, sending the EWMH state change
    /// right after `.show()` can lose a race against the window manager,
    /// which is a separate process that has to receive and process the
    /// just-sent `MapNotify` on its own connection before it'll honor a
    /// `_NET_WM_STATE` change for that window. Same 50ms-defer pattern as
    /// `overlay_shell::handle_drag_end`'s post-drag position read-back.
    /// Deferring is a no-op cost on Windows (single synchronous style-flag
    /// change, no separate WM process to race), so this applies uniformly
    /// rather than only on Linux.
    pub fn hide_from_taskbar<W>(weak: slint::Weak<W>)
    where
        W: slint::ComponentHandle + 'static,
    {
        slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
            let Some(w) = weak.upgrade() else { return };
            hide_from_taskbar_now(w.window());
        });
    }

    #[cfg(target_os = "windows")]
    fn hide_from_taskbar_now(window: &slint::Window) {
        use slint::winit_030::WinitWindowAccessor;
        use winit::platform::windows::WindowExtWindows;
        let _ = window.with_winit_window(|w| w.set_skip_taskbar(true));
    }

    /// Linux equivalent of the above. Winit has no cross-platform
    /// `set_skip_taskbar` — X11 does it via the EWMH `_NET_WM_STATE_SKIP_TASKBAR`
    /// / `_NET_WM_STATE_SKIP_PAGER` window states, sent as a `ClientMessage` to
    /// the root window (the correct way to change state on an already-mapped
    /// window, per the EWMH spec).
    ///
    /// Wayland has no equivalent core-protocol concept (no `_NET_WM_STATE`,
    /// no universal taskbar spec) and winit reports a `Wayland` raw window
    /// handle there instead of `Xlib` — that case is a no-op, same as this
    /// function used to be for all of Linux before this existed.
    #[cfg(target_os = "linux")]
    fn hide_from_taskbar_now(window: &slint::Window) {
        use slint::winit_030::WinitWindowAccessor;
        let _ = window.with_winit_window(linux_x11::skip_taskbar);
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    fn hide_from_taskbar_now(_window: &slint::Window) {}

    // ── Always-on-top reassertion ────────────────────────────────────────────
    //
    // Every overlay window's `.slint` sets `always-on-top: true` (see
    // `OverlayShell`), which Slint applies via `winit::window::Window::
    // set_window_level` — but only the *first* time the property is seen as
    // `true` (i-slint-backend-winit's `update_window_properties` diffs
    // against the level it last set and skips the call if nothing changed,
    // to dodge a window-manager bug where reasserting it constantly steals
    // focus — see that function's own comment). That one-shot application
    // doesn't survive another topmost window (a game that also marks itself
    // topmost, a screenshot/recording overlay, etc.) later taking the OS's
    // topmost slot out from under ours — the OS doesn't hand it back on its
    // own, and since Slint never sees the property "change" again, it never
    // re-pokes the OS either. Called on a slow repeating timer from each
    // overlay's own tick loop (throttled well below their render rate —
    // this is a z-order nudge, not a per-frame operation) to counter that
    // drift directly via winit, independent of Slint's one-shot tracking.
    pub fn reassert_topmost(window: &slint::Window) {
        use slint::winit_030::WinitWindowAccessor;
        let _ = window
            .with_winit_window(|w| w.set_window_level(winit::window::WindowLevel::AlwaysOnTop));
    }

    // ── Focus-stealing on show ──────────────────────────────────────────────
    //
    // Every overlay window cycles `.hide()`/`.show()` as alerts come and go
    // (see e.g. `overlay.rs`'s tick loop). On Windows, winit's `set_visible`
    // maps to `ShowWindow`: the *first* time a window is shown it uses
    // `SW_SHOWNOACTIVATE`, but every subsequent show — which is what
    // actually happens each time a trigger fires after the window has been
    // hidden once — uses plain `SW_SHOW`, which activates the window (see
    // `WindowState::set_window_flags` in winit's
    // `platform_impl/windows/window_state.rs`; the "already shown once"
    // marker is permanent for the window's lifetime, there's no way to make
    // winit keep using `SW_SHOWNOACTIVATE`). An activated overlay steals
    // Win32 keyboard/mouse focus from the game behind it, which is what
    // reads as "input freezes while triggers are firing" — the game window
    // loses focus every time an alert pops.
    //
    // `WS_EX_NOACTIVATE` fixes this at the style level rather than fighting
    // winit's `ShowWindow` call: a window carrying that extended style is
    // skipped by Win32's activation logic regardless of which `SW_*` flag
    // raised it (the standard technique used by overlay/OSD tools — RTSS,
    // Discord's game overlay, etc.). It's a persistent window style, so
    // setting it once covers every future hide/show cycle.
    /// Deferred the same way as `hide_from_taskbar` (see its doc comment) —
    /// `with_winit_window` silently no-ops until the native window actually
    /// exists, which isn't guaranteed synchronously right after `.show()`.
    pub fn set_no_activate<W>(weak: slint::Weak<W>)
    where
        W: slint::ComponentHandle + 'static,
    {
        slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
            let Some(w) = weak.upgrade() else { return };
            set_no_activate_now(w.window());
        });
    }

    #[cfg(target_os = "windows")]
    fn set_no_activate_now(window: &slint::Window) {
        use slint::winit_030::WinitWindowAccessor;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
        };

        let _ = window.with_winit_window(|w| {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let Ok(handle) = w.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(win32) = handle.as_raw() else {
                return;
            };
            let hwnd = win32.hwnd.get();
            unsafe {
                let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style | WS_EX_NOACTIVATE as isize);
            }
        });
    }

    #[cfg(not(target_os = "windows"))]
    fn set_no_activate_now(_window: &slint::Window) {}

    #[cfg(target_os = "linux")]
    mod linux_x11 {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask};
        use x11rb::xcb_ffi::XCBConnection;

        pub(super) fn skip_taskbar(winit_window: &winit::window::Window) {
            let Ok(handle) = winit_window.window_handle() else {
                return;
            };
            let RawWindowHandle::Xlib(xlib) = handle.as_raw() else {
                return;
            };
            if let Err(e) = send_skip_taskbar(xlib.window as u32) {
                tracing::warn!("skip_taskbar: failed to send EWMH state change: {e}");
            }
        }

        // Opens its own short-lived connection to the X server — separate
        // from whatever connection winit's X11 backend holds internally,
        // but EWMH client messages only need the target window's XID, not
        // that specific connection, so this works fine and keeps this code
        // independent of winit's internals.
        fn send_skip_taskbar(window_id: u32) -> Result<(), Box<dyn std::error::Error>> {
            let (conn, screen_num) = XCBConnection::connect(None)?;
            let root = conn.setup().roots[screen_num].root;

            let net_wm_state = conn.intern_atom(false, b"_NET_WM_STATE")?.reply()?.atom;
            let skip_taskbar = conn
                .intern_atom(false, b"_NET_WM_STATE_SKIP_TASKBAR")?
                .reply()?
                .atom;
            let skip_pager = conn
                .intern_atom(false, b"_NET_WM_STATE_SKIP_PAGER")?
                .reply()?
                .atom;

            // _NET_WM_STATE_ADD = 1 (see the EWMH spec's `_NET_WM_STATE` section).
            const NET_WM_STATE_ADD: u32 = 1;
            let event = xproto::ClientMessageEvent::new(
                32,
                window_id,
                net_wm_state,
                [NET_WM_STATE_ADD, skip_taskbar, skip_pager, 0, 0],
            );
            conn.send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?;
            conn.flush()?;
            Ok(())
        }
    }
}
