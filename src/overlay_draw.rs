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
        // Linux: overlays are override-redirect (see tray::run's window-
        // attributes hook), so the EWMH level request behind
        // `set_window_level` has no window manager to act on it — restack
        // directly at the X server instead.
        #[cfg(target_os = "linux")]
        let _ = window.with_winit_window(linux_x11::raise);
        #[cfg(not(target_os = "linux"))]
        let _ = window
            .with_winit_window(|w| w.set_window_level(winit::window::WindowLevel::AlwaysOnTop));
    }

    // ── Locked = click-through (Linux) ──────────────────────────────────────
    //
    // "Locked" historically only disabled the drag TouchArea — the window
    // still swallowed every click over its pixels, so a locked meter sitting
    // on the game blocked mouse input to the game under it. On X11 the input
    // shape makes locked mean what users expect: clicks pass through to the
    // game. Deduplicated per window because the overlay tick loops re-apply
    // state every tick and each X call opens a short-lived connection.
    #[cfg(target_os = "linux")]
    pub fn sync_click_through(window: &slint::Window, passthrough: bool) {
        use slint::winit_030::WinitWindowAccessor;
        use std::collections::HashMap;
        thread_local! {
            static APPLIED: std::cell::RefCell<HashMap<u64, bool>> =
                std::cell::RefCell::new(HashMap::new());
        }
        let _ = window.with_winit_window(|w| {
            let key = u64::from(w.id());
            let stale = APPLIED.with(|a| a.borrow().get(&key) != Some(&passthrough));
            if stale {
                linux_x11::set_input_passthrough(w, passthrough);
                APPLIED.with(|a| {
                    a.borrow_mut().insert(key, passthrough);
                });
            }
        });
    }

    #[cfg(not(target_os = "linux"))]
    pub fn sync_click_through(_window: &slint::Window, _passthrough: bool) {}

    /// Window-relative pointer position and button-1 state, for the manual
    /// drag loop (see `linux_x11::pointer_local` for why window-relative is
    /// load-bearing). `None` off-Linux or on any query failure.
    pub fn pointer_local(window: &slint::Window) -> Option<(i32, i32, bool)> {
        #[cfg(target_os = "linux")]
        {
            use slint::winit_030::WinitWindowAccessor;
            window.with_winit_window(linux_x11::pointer_local).flatten()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = window;
            None
        }
    }

    // ── Position reapplication on show ───────────────────────────────────────
    //
    // Every overlay window calls `set_position()` exactly once, at window
    // creation (`create_alert_window` etc.), using whatever `Config` held at
    // startup. That's the *only* place any of them ever tell the OS "put me
    // here" — after that, the app just trusts the window manager to keep
    // remembering where the window is across every later `.hide()`/`.show()`
    // cycle (e.g. Settings' "Hide All Overlays" / "Show All Overlays", or the
    // normal alert/DPS-meter queue-driven show/hide). A window created before
    // its very first real position (i.e. `Config`'s `x`/`y` were still the
    // `-1`/`-1` "unset" sentinel, true for any overlay that's never been
    // dragged or explicitly positioned yet) never got a real `set_position`
    // call at all — the WM auto-placed it wherever its own default landed
    // (observed: dead-centered). A later drag moves it live and saves the
    // real coordinates to `Config` correctly, but since nothing re-asserts
    // position on the *next* show, the WM's remap during Hide All → Show All
    // (still within the same run, so window creation never happens again to
    // pick up the new value) snaps it right back to that same default —
    // confirmed live: dragging, hiding, and showing again re-centered the
    // window, while a full app restart (which re-reads `Config` at creation
    // time, now with real coordinates) placed it correctly. Re-applying the
    // saved position on every hidden→visible transition — not just the first
    // one — closes that gap regardless of whether the position came from
    // `Config` at startup or from a drag earlier in the same run.
    pub fn apply_saved_position<W>(
        window: &W,
        handle: &std::sync::Arc<crate::tray::tray::AppHandle>,
        kind: crate::overlay_registry::overlay_registry::OverlayKind,
    ) where
        W: slint::ComponentHandle,
    {
        let (x, y) = {
            let cfg = handle.config.lock().unwrap();
            let win = kind.config(&cfg);
            (win.x, win.y)
        };
        // Same sentinel contract as the creation sites: only (-1, -1) means
        // "never positioned" — negative coordinates are real positions on
        // multi-monitor layouts.
        if (x, y) != (-1, -1) {
            window.window().set_position(slint::WindowPosition::Logical(
                slint::LogicalPosition::new(x as f32, y as f32),
            ));
        }
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

    // ── True (non-cached) position query ────────────────────────────────────
    //
    // `overlay_shell::handle_drag_end` needs the window's actual on-screen
    // position right after an interactive `drag_window()` move finishes.
    // Winit's own `Window::position()` is a cache maintained by processing
    // `ConfigureNotify` events through the ordinary event loop — but on
    // X11/xfwm4, the `ConfigureNotify` trailing an EWMH-driven interactive
    // move isn't drained by that ordinary event loop at all; it only gets
    // processed as a side effect of the *next* `drag_window()` call's own
    // internal message pump. Left alone, `position()` reads back stale
    // (specifically: frozen at wherever the window was *before* the drag
    // that just happened) indefinitely, not just briefly — confirmed live
    // against xfwm4 (2026-08-16): dragging window A to a new spot and
    // reading `position()` afterward — even after a multi-second wait with
    // no other interaction — kept returning A's pre-drag coordinates, and a
    // save at that point would silently persist the wrong value forever
    // (each drag's config write lands one full drag behind the real
    // position, exactly matching the "one drag behind" signature
    // `overlay_shell` had previously fixed for a *different*, shorter-lived
    // staleness window — see its own doc comment for that history).
    //
    // Bypassing winit's cache and asking the X server directly via
    // `TranslateCoordinates` (the same primitive `XTranslateCoordinates`
    // wraps) sidesteps the problem entirely: it's a synchronous round trip
    // to the server for the window's *current* geometry, not a locally
    // cached value that depends on which events winit has gotten around to
    // processing.
    /// Physical (unscaled) pixel position of `window`, read directly from
    /// the OS instead of through winit's cache. `None` if unsupported on
    /// this platform/backend (Wayland, any query failure) — callers should
    /// fall back to `Window::position()` in that case.
    pub fn true_window_position(window: &slint::Window) -> Option<(i32, i32)> {
        #[cfg(target_os = "linux")]
        {
            use slint::winit_030::WinitWindowAccessor;
            window
                .with_winit_window(linux_x11::query_position)
                .flatten()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = window;
            None
        }
    }

    #[cfg(target_os = "linux")]
    mod linux_x11 {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask};
        use x11rb::xcb_ffi::XCBConnection;

        fn window_xid(winit_window: &winit::window::Window) -> Option<u32> {
            let handle = winit_window.window_handle().ok()?;
            let RawWindowHandle::Xlib(xlib) = handle.as_raw() else {
                return None;
            };
            Some(xlib.window as u32)
        }

        /// Directly restacks the window to the top. For override-redirect
        /// windows there is no window manager to ask — `_NET_WM_STATE_ABOVE`
        /// client messages go nowhere — so stacking upkeep is a plain
        /// `ConfigureWindow(stack_mode=Above)`, which the X server applies
        /// itself for unmanaged windows.
        pub(super) fn raise(winit_window: &winit::window::Window) {
            let Some(xid) = window_xid(winit_window) else {
                return;
            };
            let go = || -> Result<(), Box<dyn std::error::Error>> {
                let (conn, _) = XCBConnection::connect(None)?;
                conn.configure_window(
                    xid,
                    &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
                )?;
                conn.flush()?;
                Ok(())
            };
            if let Err(e) = go() {
                tracing::warn!("raise: failed to restack overlay: {e}");
            }
        }

        /// Makes the window invisible to pointer input (`passthrough=true`,
        /// every click lands on whatever is underneath — the real meaning of
        /// "locked") or restores normal input. Uses the X Shape extension's
        /// input shape: an empty rectangle list removes the window from
        /// hit-testing entirely; resetting with a `None` mask restores the
        /// default full-window shape.
        pub(super) fn set_input_passthrough(
            winit_window: &winit::window::Window,
            passthrough: bool,
        ) {
            use x11rb::protocol::shape::{self, ConnectionExt as _};
            let Some(xid) = window_xid(winit_window) else {
                return;
            };
            let go = || -> Result<(), Box<dyn std::error::Error>> {
                let (conn, _) = XCBConnection::connect(None)?;
                if passthrough {
                    conn.shape_rectangles(
                        shape::SO::SET,
                        shape::SK::INPUT,
                        xproto::ClipOrdering::UNSORTED,
                        xid,
                        0,
                        0,
                        &[],
                    )?;
                } else {
                    conn.shape_mask(shape::SO::SET, shape::SK::INPUT, xid, 0, 0, x11rb::NONE)?;
                }
                conn.flush()?;
                Ok(())
            };
            if let Err(e) = go() {
                tracing::warn!("set_input_passthrough({passthrough}): {e}");
            }
        }

        /// Window-relative pointer position and whether button 1 is held.
        /// Used by `overlay_shell::begin_drag`'s manual move loop — an
        /// override-redirect window can't use the WM's interactive move
        /// (`_NET_WM_MOVERESIZE` needs a managed window), so the drag polls
        /// the pointer and moves the window itself.
        ///
        /// Window-relative, NOT root coordinates, and the distinction is the
        /// whole bug class: under XWayland, "root" pointer coordinates are
        /// synthesized as window-position + surface-local, and during a
        /// button-held grab COSMIC keeps delivering surface-local coords in
        /// the frame frozen at press — so every move this drag makes feeds
        /// straight back into the next root-coordinate reading and the
        /// window accelerates away exponentially (observed live: pointer
        /// deltas +2, +8, +14, +23, +36 … in lockstep with our own moves).
        /// Asking the server for coords relative to the dragged window
        /// subtracts the current window position right back out, cancelling
        /// XWayland's addition and recovering the stable frozen frame.
        pub(super) fn pointer_local(
            winit_window: &winit::window::Window,
        ) -> Option<(i32, i32, bool)> {
            let xid = window_xid(winit_window)?;
            let (conn, _) = XCBConnection::connect(None).ok()?;
            let reply = conn.query_pointer(xid).ok()?.reply().ok()?;
            let button1 = u16::from(reply.mask) & u16::from(xproto::ButtonMask::M1) != 0;
            Some((i32::from(reply.win_x), i32::from(reply.win_y), button1))
        }

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

        /// Same short-lived-connection approach as `send_skip_taskbar`, but a
        /// query instead of a state change: asks the X server to translate
        /// the window's own origin (0, 0) into root-window (i.e. screen)
        /// coordinates, which is exactly the window's on-screen position.
        pub(super) fn query_position(winit_window: &winit::window::Window) -> Option<(i32, i32)> {
            let handle = winit_window.window_handle().ok()?;
            let RawWindowHandle::Xlib(xlib) = handle.as_raw() else {
                return None;
            };
            translate_to_root(xlib.window as u32).ok()
        }

        fn translate_to_root(window_id: u32) -> Result<(i32, i32), Box<dyn std::error::Error>> {
            let (conn, screen_num) = XCBConnection::connect(None)?;
            let root = conn.setup().roots[screen_num].root;
            let reply = conn.translate_coordinates(window_id, root, 0, 0)?.reply()?;
            Ok((reply.dst_x as i32, reply.dst_y as i32))
        }
    }
}
