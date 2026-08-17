# Linux client rollup — August 2026

Changes developed and live-tested on Pop!_OS COSMIC (Wayland) against EQ
Legends running fullscreen under Proton, alongside the Windows client's
behavior as the reference. Everything here is Linux-motivated, but several
fixes are cross-platform bugs that happen to bite Linux first. Windows code
paths are untouched except where a shared call site was refactored (drag),
and the Windows behavior at those sites is preserved.

Supersedes PR #4 and PR #5 (their commits are included here verbatim).
Icons (PR #6) remain separate.

## Summary

| Change | Platforms affected | Kind |
|---|---|---|
| Trigger engine strips the log timestamp before matching | all | bug fix |
| Overlays become override-redirect windows on Linux | Linux | fix (COSMIC) |
| Locked = real click-through (input shape) | Linux | fix |
| Drag reimplemented for unmanaged windows, regime-probing | Linux | fix |
| Saved positions with negative coordinates restore correctly | all | bug fix |
| Tray icon only rewritten on a real state change | all (bites Linux) | bug fix |
| Width settings (meter + history) actually apply | all | bug fix |
| Interactive width resize (right-edge grip) | all | feature |
| DPS meter share bars (checkbox, off by default) | all | feature |
| Per-column widths with pane-splitter header dividers | all | feature |
| Class cache — colors survive restarts until the next /who | all | feature |
| Multiclass gradient bars (EQ Legends) | all | feature |

## Details and how to test

### 1. Trigger timestamp strip
`Match` conditions ran against the raw line including the
`[Fri Feb 27 22:00:07 2026] ` prefix, so a pattern anchored on the message
(`^You have slain ...`) loaded fine and could never fire. The engine now
strips the prefix exactly like the parser does (`patterns::TS_LEN`) before
evaluating conditions. All built-in presets are unanchored and behave
identically; anchoring becomes possible (it's the standard defense against
chat-pasted copies of combat lines firing triggers). Four tests pin the
contract.

**Test:** add a trigger with pattern `^You have slain` — it fires on kills
and does *not* fire when someone pastes a kill line into chat.

### 2. Override-redirect overlays (Linux)
On COSMIC, overlays could not stay above the fullscreen game, and no EWMH
request can fix it: COSMIC ignores `_NET_WM_STATE_ABOVE` for managed X11
windows (verified live — winit's level request *and* a manual
`wmctrl -b add,above` both leave the state atom empty despite
`_NET_SUPPORTED` advertising it), and the game itself is a native Wayland
surface absent from the X11 window tree entirely, so X11 stacking cannot
order against it. Override-redirect windows ride the compositor's
unmanaged-surface layer (menus/tooltips) which provably stays above the
fullscreen game — probe-tested first, then confirmed through hours of live
play. Settings keeps its normal managed window.

What it costs, and the replacements:

- **Drag** — `_NET_WM_MOVERESIZE` needs a managed window, so
  `overlay_shell::begin_drag` reimplements it on Linux. Two subtleties,
  both battle-tested:
  - The pointer must be read in **window-relative** coordinates, never
    root: under XWayland, root coords are synthesized as window-position +
    surface-local, and during a button-held grab COSMIC delivers
    surface-local coords in a frame *frozen at press* — a root-coordinate
    drag loop feeds its own moves back and the window accelerates away
    exponentially (observed: pointer deltas +2, +8, +14, +23, +36… in
    lockstep with the window's own motion).
  - A real X server *rebases* window-relative readings after each move —
    the opposite regime. The first move of every drag doubles as a probe;
    the next reading classifies the regime, then the drag runs
    total-delta-from-anchor (frozen frame) or move-by-remaining-delta
    (rebasing). **Please verify a drag on your xfwm4 setup** — the
    rebasing path follows core X11 semantics but was only reasoned, not
    run.
- **Stacking upkeep** — `reassert_topmost`'s Linux arm is a direct
  `ConfigureWindow(stack_mode=Above)`; there is no WM to accept a level
  request.
- **Locked = real click-through** — an empty Shape-extension input region
  removes a locked overlay from hit-testing entirely, so clicks land on
  the game. Previously "locked" only disabled the drag TouchArea and the
  window still ate every click over its pixels. Show All Windows keeps
  windows clickable while arranging.

**Test (Linux):** overlays stay above the fullscreen game across focus
clicks; drag tracks the mouse 1:1; lock an overlay and click through it.
**Test (Windows):** drag still uses the native move loop — no behavior
change expected at the four drag call sites.

### 3. Negative-coordinate position restore
The never-positioned sentinel is exactly `(-1, -1)`, but every restore site
gated on `x >= 0 && y >= 0` — silently discarding real positions that are
negative. Negative coordinates are routine on multi-monitor layouts (any
monitor left of or above the primary) and whenever a window sits past a
screen's top/left edge. Symptom: position saves fine, window recenters on
every restart. All five sites now test for the sentinel pair itself.

**Test:** drag an overlay onto a monitor left/above the primary (or nudge
it past the top edge), restart — it comes back exactly where it was.

### 4. Tray icon churn
Every `set_icon` on Linux writes a NEW counter-named PNG, deletes the
previous one, and re-points the StatusNotifierItem at the new path. The
status timer re-set an unchanged icon every slow tick, handing the tray
applet a moving target — a read that lost the race landed on a deleted
file and rendered no icon at all (observed live on COSMIC: the item's
IconName wedged one generation behind the file on disk). The icon choice
is now a comparable key cached across ticks; `set_icon` runs only on a
real transition. Also collapses the not-registered / disconnected branches
(both orange) into one arm.

**Test:** tray icon present and stable for hours; flips color promptly on
logging toggle / connection loss.

### 5. Meter and history width — settings, grip, and column dividers
`cfg.meter_width` and `cfg.overlay_history_width` (and history's Settings
"Width" field) existed but were never applied — the `.slint` roots pinned
`width: 320px`. Now:

- The roots take a `win-width` property pushed by each tick loop
  (`<= 0` keeps the historical width); height stays content-driven.
- The DPS Meter card gains the missing "Width" field.
- Both windows get an 8px right-edge **resize grip** (ew-resize cursor,
  subtle highlight): drag to resize, saved on release. Linux uses the same
  poll-loop shape as the drag (no regime probe needed — the window origin
  never moves during a width resize, so window-relative readings are safe
  in both regimes); elsewhere it hands off to the OS-native interactive
  resize (`drag_resize_window(East)`).
- The meter header gains **pane-splitter column dividers** — one per
  column boundary. Interior dividers *transfer* width between their two
  neighbors, so dragging the DPS|% boundary grows one and shrinks the
  other while every other column keeps both width and position. The
  Name|Dmg divider is single-sided (the stretchy Name column absorbs).
  Dividers anchor drags in window coordinates (`absolute-position +
  mouse-x`) — element-relative deltas oscillate because the divider moves
  with the boundary it drags. Per-column widths persist
  (`meter_amount_w/rate_w/bar_w/sec_w`, 0 = auto).

**Test:** resize both windows by grip and by Settings field; drag each
header divider; restart and confirm all widths persist.

### 6. DPS meter share bars (the requested checkbox)
"Share bars" checkbox on the DPS Meter card, **off by default — off is
pixel-identical to the current meter** (the bar column and % header are
conditional elements, not restyled ones). On: each row gets the Linux
client's bar cell — a short rounded track, class-colored proportional
fill, "NN%" inside — where the fraction is the row's slice of the GROUP
total (all contributors, matching the footer's cumulative number, not just
the rows that fit on screen).

### 7. Class cache and multiclass gradients
- `CombatState.player_classes` only fills from /who, so every session
  started gray until someone ran /who. A sidecar cache
  (`classes-cache.toml` beside config.toml) remembers each name's last
  known classes across restarts; a fresh /who observation overwrites the
  entry; only written when an entry actually changes.
- The bar fill blends up to three class colors with the web viewer's
  `classGradient` stop recipe (2 classes at 0/.35/.65/1, 3 at
  0/.25/.40/.60/.75/1). A single-class character renders a solid bar, so
  classic-EQ servers are automatically correct with no mode switch.
- Implementation note: Slint 1.17 does not export `LinearGradientBrush` /
  `GradientStop` to Rust, so the gradient lives in the `.slint` markup as
  conditional `@linear-gradient` expressions over three color fields.

**Test:** enable share bars, /who a group with multiclass characters —
bars blend; restart — colors return from cache before any /who.
