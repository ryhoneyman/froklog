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

    /// Starts a drag from an overlay's TouchArea press. One entry point for
    /// every overlay window, replacing the per-site `drag_window()` calls:
    ///
    /// - Windows: the OS-native interactive move (`drag_window()` blocks for
    ///   the drag there), then the existing position read-back.
    /// - Linux: overlays are override-redirect windows (see `tray::run`'s
    ///   window-attributes hook), and `_NET_WM_MOVERESIZE` — the mechanism
    ///   behind `drag_window()` — only works on WM-managed windows. So the
    ///   drag is manual: anchor the pointer and window positions at press,
    ///   then poll ~60 Hz moving the window by the pointer's total delta
    ///   until button 1 is released, then save. Total-delta-from-anchor, not
    ///   move-by-increment: each move changes what "pointer relative to the
    ///   window" means, and incremental feedback loops the window into a
    ///   corner (same reasoning as Slint's own doc comment on
    ///   `create_alert_window`'s use of the native move loop).
    pub fn begin_drag<W>(weak: Weak<W>, handle: Arc<AppHandle>, kind: OverlayKind)
    where
        W: ComponentHandle + 'static,
    {
        #[cfg(target_os = "linux")]
        {
            let anchors = weak.upgrade().and_then(|w| {
                let local = crate::overlay_draw::overlay_draw::pointer_local(w.window());
                let win = crate::overlay_draw::overlay_draw::true_window_position(w.window());
                match (local, win) {
                    (Some((lx, ly, true)), Some(win)) => Some(((lx, ly), win)),
                    _ => None,
                }
            });
            if let Some((anchor_local, anchor_win)) = anchors {
                manual_drag_step(weak, handle, kind, anchor_local, anchor_win, DragMode::Probe, 0);
                return;
            }
            // Pointer already released (or query failed): fall through to
            // the read-back path so at least the position gets saved.
            handle_drag_end(weak, handle, kind);
        }
        #[cfg(not(target_os = "linux"))]
        {
            if let Some(w) = weak.upgrade() {
                use slint::winit_030::WinitWindowAccessor;
                let _ = w.window().with_winit_window(|winit_window| {
                    let _ = winit_window.drag_window();
                });
            }
            handle_drag_end(weak, handle, kind);
        }
    }

    /// How the compositor treats window-relative pointer coordinates while
    /// this drag moves the window under a held button — discovered live,
    /// per drag, by `manual_drag_step`'s probe.
    ///
    /// Two regimes exist:
    /// - A real X server rebases: after the window moves +d, the same
    ///   physical pointer spot reads d lower in window coordinates.
    /// - XWayland on COSMIC does not: during the button-held grab the
    ///   compositor keeps delivering surface-local coordinates in the frame
    ///   frozen at press, so window-relative readings ignore our own moves
    ///   entirely. (Root coordinates are unusable in this regime — XWayland
    ///   synthesizes them as window-position + frozen-local, so our own
    ///   moves feed back and the window accelerates away exponentially;
    ///   that runaway was observed live before this probe existed.)
    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy)]
    enum DragMode {
        /// Waiting for the pointer to move far enough to issue the first
        /// window move (which doubles as the probe).
        Probe,
        /// First move issued: `probe_delta` is that move, `local_at_probe`
        /// the window-relative reading the instant it was issued. Next tick
        /// compares readings to classify the regime.
        Sense {
            probe_delta: (i32, i32),
            local_at_probe: (i32, i32),
        },
        /// Frozen frame (COSMIC/XWayland): total-delta-from-anchor is exact.
        Absolute,
        /// Rebasing server (plain X11): local readings snap back after each
        /// move, so each tick moves by the remaining delta.
        Incremental,
    }

    /// Ceiling on one drag; a stuck button-state reading must not poll
    /// forever. Generous — a deliberate reposition can take a while.
    #[cfg(target_os = "linux")]
    const DRAG_MAX_MS: u64 = 30_000;
    /// Pointer travel (px, either axis) before the probe move fires — big
    /// enough that the Sense comparison isn't drowned by mouse jitter.
    #[cfg(target_os = "linux")]
    const PROBE_MIN_PX: i32 = 6;

    /// One ~16ms step of the Linux manual drag; re-arms itself until button
    /// 1 is released. Same self-rescheduling single-shot-timer shape as
    /// `poll_until_stable` above.
    #[cfg(target_os = "linux")]
    fn manual_drag_step<W>(
        weak: Weak<W>,
        handle: Arc<AppHandle>,
        kind: OverlayKind,
        anchor_local: (i32, i32),
        anchor_win: (i32, i32),
        mode: DragMode,
        elapsed_ms: u64,
    ) where
        W: ComponentHandle + 'static,
    {
        slint::Timer::single_shot(Duration::from_millis(POLL_INTERVAL_MS), move || {
            let Some(w) = weak.upgrade() else { return };
            let finish = |w: &W| {
                let pos = read_position(w);
                save_position(kind, &handle, pos);
            };
            let Some((lx, ly, down)) = crate::overlay_draw::overlay_draw::pointer_local(w.window())
            else {
                finish(&w);
                return;
            };
            let dl = (lx - anchor_local.0, ly - anchor_local.1);

            let mode = match mode {
                DragMode::Sense {
                    probe_delta,
                    local_at_probe,
                } => {
                    // If the server rebased, this tick's reading dropped by
                    // ≈probe_delta relative to the reading at probe time
                    // (mouse jitter aside — the probe is ≥PROBE_MIN_PX, and
                    // the threshold is half of it, on the probe's dominant
                    // axis).
                    let shift = (lx - local_at_probe.0, ly - local_at_probe.1);
                    let (probe_axis, shift_axis) = if probe_delta.0.abs() >= probe_delta.1.abs() {
                        (probe_delta.0, shift.0)
                    } else {
                        (probe_delta.1, shift.1)
                    };
                    let rebased = if probe_axis >= 0 {
                        shift_axis <= -probe_axis / 2
                    } else {
                        shift_axis >= -probe_axis / 2
                    };
                    tracing::debug!(
                        "drag {kind:?}: sense probe={probe_delta:?} shift={shift:?} -> {}",
                        if rebased { "incremental (rebasing X server)" } else { "absolute (frozen frame)" }
                    );
                    if rebased {
                        DragMode::Incremental
                    } else {
                        DragMode::Absolute
                    }
                }
                m => m,
            };

            let next = match mode {
                DragMode::Probe => {
                    if dl.0.abs() >= PROBE_MIN_PX || dl.1.abs() >= PROBE_MIN_PX {
                        w.window().set_position(slint::WindowPosition::Physical(
                            slint::PhysicalPosition::new(anchor_win.0 + dl.0, anchor_win.1 + dl.1),
                        ));
                        DragMode::Sense {
                            probe_delta: dl,
                            local_at_probe: (lx, ly),
                        }
                    } else {
                        DragMode::Probe
                    }
                }
                DragMode::Absolute => {
                    w.window().set_position(slint::WindowPosition::Physical(
                        slint::PhysicalPosition::new(anchor_win.0 + dl.0, anchor_win.1 + dl.1),
                    ));
                    DragMode::Absolute
                }
                DragMode::Incremental => {
                    // Local readings snap back toward the anchor after each
                    // move, so what remains of `dl` is the not-yet-applied
                    // pointer travel — apply it on top of wherever the
                    // window actually is now.
                    if dl != (0, 0) {
                        if let Some((cx, cy)) =
                            crate::overlay_draw::overlay_draw::true_window_position(w.window())
                        {
                            w.window().set_position(slint::WindowPosition::Physical(
                                slint::PhysicalPosition::new(cx + dl.0, cy + dl.1),
                            ));
                        }
                    }
                    DragMode::Incremental
                }
                DragMode::Sense { .. } => unreachable!("Sense resolved above"),
            };

            if down && elapsed_ms < DRAG_MAX_MS {
                manual_drag_step(
                    weak,
                    handle,
                    kind,
                    anchor_local,
                    anchor_win,
                    next,
                    elapsed_ms + POLL_INTERVAL_MS,
                );
            } else {
                finish(&w);
            }
        });
    }

    /// Starts a width resize from an overlay's right-edge grip. Linux runs
    /// the same poll-loop shape as `begin_drag` — but no regime probe: the
    /// window's ORIGIN never moves during a width resize, so window-relative
    /// pointer readings are trustworthy under both the frozen-frame
    /// compositor and a rebasing X server. Elsewhere it hands off to the
    /// OS-native interactive resize.
    ///
    /// `apply` pushes the new logical width into the window's `win-width`
    /// property for instant feedback; the config field is updated in memory
    /// each step (so the tick loop's own `win-width` push agrees mid-drag)
    /// and persisted once on release.
    pub fn begin_width_resize<W>(
        weak: Weak<W>,
        handle: Arc<AppHandle>,
        kind: OverlayKind,
        apply: impl Fn(&W, i32) + 'static,
    ) where
        W: ComponentHandle + 'static,
    {
        #[cfg(target_os = "linux")]
        {
            let anchors = weak.upgrade().and_then(|w| {
                let (lx, _, down) = crate::overlay_draw::overlay_draw::pointer_local(w.window())?;
                if !down {
                    return None;
                }
                Some((lx, w.window().size().width as i32))
            });
            if let Some((anchor_x, anchor_w)) = anchors {
                width_resize_step(weak, handle, kind, anchor_x, anchor_w, apply, 0);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = &apply;
            if let Some(w) = weak.upgrade() {
                use slint::winit_030::WinitWindowAccessor;
                let _ = w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(winit::window::ResizeDirection::East);
                });
                // Native resize has finished (drag_resize_window blocks on
                // Windows) — persist what the OS left us with.
                let scale = w.window().scale_factor();
                let logical = (w.window().size().width as f32 / scale).round() as i32;
                save_width(kind, &handle, logical);
            }
        }
    }

    /// Bounds on an interactively-resized overlay width (logical px).
    const RESIZE_MIN_W: i32 = 160;
    const RESIZE_MAX_W: i32 = 2000;

    #[cfg(target_os = "linux")]
    fn width_resize_step<W>(
        weak: Weak<W>,
        handle: Arc<AppHandle>,
        kind: OverlayKind,
        anchor_x: i32,
        anchor_w: i32,
        apply: impl Fn(&W, i32) + 'static,
        elapsed_ms: u64,
    ) where
        W: ComponentHandle + 'static,
    {
        slint::Timer::single_shot(Duration::from_millis(POLL_INTERVAL_MS), move || {
            let Some(w) = weak.upgrade() else { return };
            let Some((lx, _, down)) = crate::overlay_draw::overlay_draw::pointer_local(w.window())
            else {
                return;
            };
            let scale = w.window().scale_factor();
            let new_phys = anchor_w + (lx - anchor_x);
            let logical =
                ((new_phys as f32 / scale).round() as i32).clamp(RESIZE_MIN_W, RESIZE_MAX_W);
            if let Some(field) = width_field(kind) {
                let mut cfg = handle.config.lock().unwrap();
                *field(&mut cfg) = logical;
            }
            apply(&w, logical);
            if down && elapsed_ms < DRAG_MAX_MS {
                width_resize_step(
                    weak,
                    handle,
                    kind,
                    anchor_x,
                    anchor_w,
                    apply,
                    elapsed_ms + POLL_INTERVAL_MS,
                );
            } else {
                save_width(kind, &handle, logical);
            }
        });
    }

    /// Which config field holds `kind`'s width — `None` for overlays without
    /// a width setting.
    #[allow(clippy::type_complexity)]
    fn width_field(kind: OverlayKind) -> Option<fn(&mut crate::config::Config) -> &mut i32> {
        match kind {
            OverlayKind::Meter => Some(|cfg| &mut cfg.meter_width),
            OverlayKind::History => Some(|cfg| &mut cfg.overlay_history_width),
            _ => None,
        }
    }

    fn save_width(kind: OverlayKind, handle: &Arc<AppHandle>, width: i32) {
        let Some(field) = width_field(kind) else { return };
        let mut cfg = handle.config.lock().unwrap();
        *field(&mut cfg) = width;
        cfg.save();
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
