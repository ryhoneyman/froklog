/// Live DPS/Tank/Heal/Heal-Received meter overlay.
///
/// A second Win32 `WS_EX_LAYERED` popup, structurally a twin of `overlay.rs`'s
/// alert deck, but rendering a fixed ranked table instead of an animated toast
/// stack. Reads `AppHandle.combat_state` directly — the client already builds
/// a fully aggregated `CombatState` locally (see `main.rs::run_engine_once`),
/// so no server round-trip is needed.
///
/// Scope: the mob instance the player is currently engaged with (see
/// `resolve_view_mob_id`/`player_engaged` below), ranked by whichever of
/// the four tabs is selected.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_dps {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::mem;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::config::Config;
    #[cfg(target_os = "windows")]
    use crate::overlay_draw::overlay_draw::apply_lock_style;
    #[cfg(target_os = "windows")]
    use crate::overlay_draw::overlay_draw::{composite_text, make_font};
    use crate::overlay_draw::overlay_draw::{fill_rect, fill_rrect, premult};
    use crate::state::{CombatState, EntityCombatStats};
    use crate::tray::tray::AppHandle;

    // ── Win32 imports ─────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};

    #[cfg(target_os = "windows")]
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint, GetDC,
        ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS,
        DT_END_ELLIPSIS, DT_LEFT, DT_NOCLIP, DT_RIGHT, HBRUSH, HFONT, HGDIOBJ, PAINTSTRUCT,
    };

    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::COLORREF;

    #[cfg(target_os = "windows")]
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;

    #[cfg(target_os = "windows")]
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, LoadCursorW, PostMessageW,
        PostQuitMessage, RegisterClassExW, SetWindowLongPtrW, ShowWindow, TranslateMessage,
        UpdateLayeredWindow, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HTLEFT, HTRIGHT,
        IDC_ARROW, MSG, SM_CXSCREEN, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_CREATE, WM_DESTROY,
        WM_LBUTTONDOWN, WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_PAINT, WM_SIZE, WM_TIMER, WNDCLASSEXW,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    #[cfg(target_os = "windows")]
    use windows::core::PCWSTR;

    // ── Constants ─────────────────────────────────────────────────────────────

    const WM_EXITSIZEMOVE: u32 = 0x0232;
    const TIMER_METER: usize = 1;
    const TIMER_INTERVAL_MS: u32 = 200;

    // Window width is user-resizable (left/right edges); clamped to keep the
    // chrome icons and column layout from breaking at extreme sizes.
    const MIN_METER_WIDTH: i32 = 260;
    const MAX_METER_WIDTH: i32 = 640;
    const TITLE_H: i32 = 26;
    const HEADER_H: i32 = 20;
    const ROW_H: i32 = 20;
    const PAD_X: i32 = 8;
    const SWATCH_SZ: i32 = 12;
    const CORNER_R: i32 = 4;
    const TAB_W: i32 = 42;
    const CHROME_ICON_SZ: i32 = 15;
    /// Point size for the Segoe MDL2 Assets chrome-icon glyphs, independent
    /// of the row/header font size (which is user-configurable).
    const ICON_FONT_PT: i32 = 11;

    const CLASS_NAME: &str = "FroklogDpsMeter\0";

    const PANEL_BG: (u8, u8, u8, u8) = (12, 12, 17, 215);
    const TITLE_BG: (u8, u8, u8, u8) = (22, 22, 30, 235);
    const HEADER_BG: (u8, u8, u8, u8) = (18, 18, 25, 200);
    const TOP_ROW_BG: (u8, u8, u8, u8) = (60, 46, 10, 220);
    const ROW_BG_ALT: (u8, u8, u8, u8) = (255, 255, 255, 10);
    const FOOTER_BG: (u8, u8, u8, u8) = (30, 34, 44, 235);

    // ── Tabs ──────────────────────────────────────────────────────────────────

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MeterTab {
        Dps,
        Tank,
        Heal,
        HealReceived,
    }

    impl MeterTab {
        const ALL: [MeterTab; 4] = [Self::Dps, Self::Tank, Self::Heal, Self::HealReceived];

        fn label(&self) -> &'static str {
            match self {
                Self::Dps => "DPS",
                Self::Tank => "Tank",
                Self::Heal => "Heal",
                Self::HealReceived => "Recv",
            }
        }

        fn amount_col_label(&self) -> &'static str {
            match self {
                Self::Dps | Self::Tank => "Dmg",
                Self::Heal | Self::HealReceived => "Heal",
            }
        }

        fn rate_col_label(&self) -> &'static str {
            match self {
                Self::Dps | Self::Tank => "DPS",
                Self::Heal | Self::HealReceived => "HPS",
            }
        }

        /// The per-mob bucket this tab reads from.
        fn bucket<'a>(
            &self,
            cs: &'a CombatState,
            mob_id: u64,
        ) -> Option<&'a HashMap<String, EntityCombatStats>> {
            match self {
                Self::Dps => cs.mob_damage.get(&mob_id),
                Self::Tank => cs.mob_tanking.get(&mob_id),
                Self::Heal => cs.mob_healing.get(&mob_id),
                Self::HealReceived => cs.mob_healed.get(&mob_id),
            }
        }

        fn total_of(&self, stats: &EntityCombatStats) -> u64 {
            match self {
                Self::Dps | Self::Tank => stats.total_damage,
                Self::Heal => stats.total_heals,
                Self::HealReceived => stats.total_healed_received,
            }
        }
    }

    // ── Row data ──────────────────────────────────────────────────────────────

    struct RowData {
        name: String,
        color: (u8, u8, u8),
        total: u64,
        rate: u64,
    }

    /// All attackers/healers for `mob_id` under `tab`, filtered but not yet
    /// sorted or truncated — used both to build display rows and to sum a
    /// cumulative footer total across *every* contributor, not just the ones
    /// that fit on screen.
    fn tab_entries(
        cs: &CombatState,
        mob_id: u64,
        tab: MeterTab,
    ) -> Vec<(&String, &EntityCombatStats)> {
        let Some(bucket) = tab.bucket(cs, mob_id) else {
            return Vec::new();
        };
        bucket
            .iter()
            .filter(|(name, _)| {
                // Tank tab excludes other mobs (pets/adds hitting a mob) from
                // the "who tanked" ranking, matching to_api_json()'s mob_tanking
                // builder (state.rs).
                tab != MeterTab::Tank || !cs.confirmed_mobs.contains(name.as_str())
            })
            .collect()
    }

    /// How long `mob_id` has actually been under fire: `last_seen - first_seen`
    /// for that specific mob instance, NOT the global fight timer. The global
    /// `CombatState::elapsed_secs()` only freezes once *every* tracked mob is
    /// confirmed dead, so it keeps climbing (and DPS keeps dropping) for as
    /// long as any other mob on the mob list is still alive — even after this
    /// mob stopped taking damage. `MobSighting.last_seen` stops advancing the
    /// moment nothing touches this mob, so this value naturally freezes too.
    fn mob_elapsed_secs(cs: &CombatState, mob_id: u64) -> f64 {
        cs.mob_list
            .iter()
            .find(|m| m.id == mob_id)
            .map(|m| m.last_seen.duration_since(m.first_seen).as_secs_f64())
            .unwrap_or_else(|| cs.elapsed_secs())
            .max(0.001)
    }

    /// Whether the current player (or their pet) has actually traded blows
    /// with this mob instance — dealt it damage or taken damage from it.
    /// Guards auto-follow so a mob only being fought by other people nearby
    /// (group-mates or bystanders on a separate pull) never steals the
    /// display away from the player's own encounter.
    fn player_engaged(cs: &CombatState, mob_id: u64) -> bool {
        let is_player_or_pet = |name: &str| {
            name == cs.player_name
                || cs
                    .known_pets
                    .get(name)
                    .is_some_and(|owner| owner == &cs.player_name)
        };
        cs.mob_damage
            .get(&mob_id)
            .is_some_and(|by_player| by_player.keys().any(|k| is_player_or_pet(k)))
            || cs
                .mob_tanking
                .get(&mob_id)
                .is_some_and(|by_player| by_player.keys().any(|k| is_player_or_pet(k)))
    }

    /// Which mob the meter should currently display: the manually pinned
    /// mob if one is set and still present on the mob list, otherwise the
    /// most recently active mob the player is actually engaged with.
    ///
    /// Deliberately does *not* just read `CombatState.active_mob_id` —
    /// that's a single global "last mob touched by anyone" pointer, so it
    /// flips to whatever a group-mate or nearby player is fighting even
    /// when the current player isn't involved at all. Scanning `mob_list`
    /// for the freshest mob with a `player_engaged` hit keeps the meter on
    /// the player's own fight regardless of what else is happening around
    /// them.
    fn resolve_view_mob_id(cs: &CombatState, pinned: Option<u64>) -> Option<u64> {
        if let Some(pid) = pinned {
            if cs.mob_list.iter().any(|m| m.id == pid) {
                return Some(pid);
            }
        }
        cs.mob_list
            .iter()
            .filter(|m| player_engaged(cs, m.id))
            .max_by_key(|m| m.last_seen)
            .map(|m| m.id)
    }

    const MAX_PICKER_ENTRIES: usize = 6;

    struct MobPickerEntry {
        /// `None` = the "Auto (most recent)" entry, clearing any pin.
        id: Option<u64>,
        label: String,
        dot: (u8, u8, u8),
    }

    /// Build the mob picker's row list: "Auto" first, then up to
    /// `MAX_PICKER_ENTRIES` confirmed mobs sorted most-recently-seen first,
    /// each with an activity dot (green = active <5s, amber = idle <15s,
    /// grey = timed out/dead) — same thresholds `to_api_json()` uses for the
    /// web UI's mob list indicator (state.rs).
    fn build_mob_picker_entries(cs: &CombatState) -> Vec<MobPickerEntry> {
        let mut entries = vec![MobPickerEntry {
            id: None,
            label: "Auto (most recent)".to_string(),
            dot: (120, 120, 130),
        }];

        let mut mobs: Vec<_> = cs
            .mob_list
            .iter()
            .filter(|m| cs.confirmed_mobs.contains(&m.name))
            .collect();
        mobs.sort_unstable_by_key(|m| std::cmp::Reverse(m.last_seen));
        mobs.truncate(MAX_PICKER_ENTRIES);

        for m in mobs {
            let secs_since_last = m.last_seen.elapsed().as_secs_f64();
            let timed_out = secs_since_last >= 15.0;
            let is_dead = cs.dead_mobs.contains(&m.name) || timed_out;
            let is_active = !is_dead && secs_since_last < 5.0;
            let dot = if is_active {
                (80, 200, 100)
            } else if !is_dead {
                (220, 180, 60)
            } else {
                (110, 110, 118)
            };
            entries.push(MobPickerEntry {
                id: Some(m.id),
                label: m.name.clone(),
                dot,
            });
        }
        entries
    }

    fn build_rows(
        cs: &CombatState,
        entries: &[(&String, &EntityCombatStats)],
        tab: MeterTab,
        max_rows: usize,
        elapsed: f64,
    ) -> Vec<RowData> {
        let mut sorted = entries.to_vec();
        sorted.sort_by_key(|(_, s)| std::cmp::Reverse(tab.total_of(s)));
        sorted.truncate(max_rows);

        sorted
            .into_iter()
            .map(|(name, s)| {
                let total = tab.total_of(s);
                let rate = (total as f64 / elapsed).round() as u64;
                let code = cs
                    .player_classes
                    .get(name)
                    .and_then(|c| c.first())
                    .map(|s| s.as_str())
                    .unwrap_or("");
                RowData {
                    name: name.clone(),
                    color: class_color(code),
                    total,
                    rate,
                }
            })
            .collect()
    }

    /// Everything a single render (or a copy-to-clipboard summary) needs for
    /// one mob/tab combination — computed once per use so `WM_TIMER`'s render
    /// pass and the copy-icon click handler can never disagree about what's
    /// "currently on screen."
    struct MeterSnapshot {
        mob_name: String,
        rows: Vec<RowData>,
        footer_total: u64,
        footer_rate: u64,
        elapsed_secs: u64,
    }

    fn compute_snapshot(
        cs: &CombatState,
        mob_id: u64,
        tab: MeterTab,
        max_rows: usize,
    ) -> MeterSnapshot {
        let elapsed = mob_elapsed_secs(cs, mob_id);
        let entries = tab_entries(cs, mob_id, tab);
        let rows = build_rows(cs, &entries, tab, max_rows, elapsed);
        let mob_name = cs
            .mob_list
            .iter()
            .find(|m| m.id == mob_id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| cs.mob_name.clone());
        let footer_total: u64 = entries.iter().map(|(_, s)| tab.total_of(s)).sum();
        let footer_rate = (footer_total as f64 / elapsed).round() as u64;
        MeterSnapshot {
            mob_name,
            rows,
            footer_total,
            footer_rate,
            elapsed_secs: elapsed.round() as u64,
        }
    }

    /// EQ class short-code → accent colour, ported from `CLASS_COLOR` in
    /// `static/stream.html` so the meter matches the web viewer's palette.
    fn class_color(code: &str) -> (u8, u8, u8) {
        let hex = match code {
            "WAR" => 0xaf803c,
            "CLR" => 0x733273,
            "PAL" => 0x7387fa,
            "RNG" => 0x507334,
            "SHD" => 0x4b4b41,
            "DRU" => 0x649150,
            "MNK" => 0xd2b48c,
            "BRD" => 0xb4a032,
            "ROG" => 0x505c55,
            "SHM" => 0xa2a2b0,
            "NEC" => 0x0164fa,
            "WIZ" => 0xaf0a32,
            "MAG" => 0x960a0a,
            "ENC" => 0x0a32c8,
            "BST" => 0x826432,
            "BER" => 0xbe6414,
            _ => 0x464650,
        };
        (
            (hex >> 16 & 0xFF) as u8,
            (hex >> 8 & 0xFF) as u8,
            (hex & 0xFF) as u8,
        )
    }

    /// Abbreviate a count with a K/M suffix, matching `fmtK` in
    /// `static/stream.html` (strips a trailing ".0").
    fn fmt_k(n: u64) -> String {
        let s = if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}K", n as f64 / 1_000.0)
        } else {
            return n.to_string();
        };
        s.strip_suffix(".0K")
            .map(|p| format!("{p}K"))
            .or_else(|| s.strip_suffix(".0M").map(|p| format!("{p}M")))
            .unwrap_or(s)
    }

    /// `185` -> `"3:05"`, `27` -> `"27s"` — mm:ss once a fight runs past a minute.
    fn fmt_duration(secs: u64) -> String {
        if secs >= 60 {
            format!("{}:{:02}", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        }
    }

    /// One raid-chat-friendly line summarizing the current snapshot — plain
    /// ASCII only (no em-dashes/emoji) since this is meant to be pasted into
    /// the EQ chat box or Discord, both of which handle plain text most
    /// reliably.
    fn build_summary_line(snap: &MeterSnapshot, tab: MeterTab) -> String {
        let (amount_label, rate_label) = match tab {
            MeterTab::Dps | MeterTab::Tank => ("dmg", "dps"),
            MeterTab::Heal | MeterTab::HealReceived => ("heal", "hps"),
        };
        let mut line = format!(
            "{}: {} {amount_label} ({} {rate_label}) over {}",
            snap.mob_name,
            fmt_k(snap.footer_total),
            fmt_k(snap.footer_rate),
            fmt_duration(snap.elapsed_secs),
        );
        if !snap.rows.is_empty() {
            let ranked: Vec<String> = snap
                .rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let pct = if snap.footer_total > 0 {
                        r.total as f64 / snap.footer_total as f64 * 100.0
                    } else {
                        0.0
                    };
                    format!("{}. {} {} ({:.0}%)", i + 1, r.name, fmt_k(r.total), pct)
                })
                .collect();
            line.push_str(" | ");
            line.push_str(&ranked.join(", "));
        }
        line
    }

    /// Copy `text` to the Windows clipboard as `CF_UNICODETEXT`. Duplicated
    /// from `tray.rs`'s private `copy_to_clipboard` (same ~30-line Win32
    /// GlobalAlloc/SetClipboardData boilerplate) rather than exposing it
    /// cross-module for one call site.
    #[cfg(target_os = "windows")]
    fn copy_to_clipboard(text: &str) {
        use windows::Win32::Foundation::{HANDLE, HWND as ClipHwnd};
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };

        const CF_UNICODETEXT: u32 = 13;

        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
        let byte_count = wide.len() * 2;

        unsafe {
            let Ok(hglob) = GlobalAlloc(GMEM_MOVEABLE, byte_count) else {
                return;
            };
            let ptr = GlobalLock(hglob) as *mut u16;
            if ptr.is_null() {
                return;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hglob);

            if OpenClipboard(ClipHwnd::default()).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            // Ownership of hglob transfers to the clipboard; do not free it.
            let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(hglob.0));
            let _ = CloseClipboard();
        }
    }

    // ── Chrome hit-test rects ────────────────────────────────────────────────

    /// Title-bar sub-regions, in client coordinates. Shared by rendering and
    /// `WM_LBUTTONDOWN` hit-testing so they can never disagree.
    #[cfg(target_os = "windows")]
    struct ChromeRects {
        tabs: [RECT; 4],
        gear: RECT,
        trash: RECT,
        copy: RECT,
        title_bar: RECT,
    }

    /// Right-aligned title-bar icon slot, counting outward from the right
    /// edge (0 = rightmost). Shared spacing so icons never overlap.
    #[cfg(target_os = "windows")]
    fn icon_slot(width: i32, index_from_right: i32) -> RECT {
        let step = CHROME_ICON_SZ + 4;
        let right_edge = width - PAD_X - index_from_right * step;
        RECT {
            left: right_edge - CHROME_ICON_SZ,
            top: (TITLE_H - CHROME_ICON_SZ) / 2,
            right: right_edge,
            bottom: (TITLE_H - CHROME_ICON_SZ) / 2 + CHROME_ICON_SZ,
        }
    }

    #[cfg(target_os = "windows")]
    fn compute_chrome_rects(width: i32) -> ChromeRects {
        let mut tabs = [RECT::default(); 4];
        for (i, r) in tabs.iter_mut().enumerate() {
            let x1 = PAD_X + i as i32 * TAB_W;
            *r = RECT {
                left: x1,
                top: 0,
                right: x1 + TAB_W,
                bottom: TITLE_H,
            };
        }
        // Right-to-left: gear (settings), trash (clear stats), copy (copy
        // summary) — settings stays rightmost since it's reached most often
        // via the tray-menu escape hatch too.
        let gear = icon_slot(width, 0);
        let trash = icon_slot(width, 1);
        let copy = icon_slot(width, 2);
        let title_bar = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: TITLE_H,
        };
        ChromeRects {
            tabs,
            gear,
            trash,
            copy,
            title_bar,
        }
    }

    #[cfg(target_os = "windows")]
    fn point_in_rect(x: i32, y: i32, r: &RECT) -> bool {
        x >= r.left && x < r.right && y >= r.top && y < r.bottom
    }

    // ── State ─────────────────────────────────────────────────────────────────

    struct MeterState {
        handle: Arc<AppHandle>,
        active_tab: MeterTab,
        locked: bool,
        meter_enabled: bool,
        max_rows: usize,
        idle_secs: u32,
        font_size: i32,
        window_x: i32,
        window_y: i32,
        /// User-resizable via the left/right window edges.
        window_w: i32,
        #[cfg(target_os = "windows")]
        hfont_normal: Option<HFONT>,
        #[cfg(target_os = "windows")]
        hfont_bold: Option<HFONT>,
        /// Segoe MDL2 Assets, for the title-bar chrome icons (gear/trash/copy).
        #[cfg(target_os = "windows")]
        hfont_icon: Option<HFONT>,
        last_active_mob_id: Option<u64>,
        last_footer_total: u64,
        last_change: Instant,
        visible: bool,
        /// Mirrors `AppHandle.force_show_windows` — forces this window
        /// visible (with placeholder rows) so it can be dragged into
        /// position from the Settings dialog even with no combat data.
        force_show: bool,
        /// Manually selected mob instance, overriding auto-follow of
        /// `CombatState.active_mob_id`. Cleared automatically if the pinned
        /// mob falls out of `mob_list` (e.g. after an engine restart).
        pinned_mob_id: Option<u64>,
        /// Whether the mob picker (opened by clicking the footer) is expanded.
        mob_picker_open: bool,
        /// Row count and picker entries from the most recently rendered frame —
        /// cached so `WM_LBUTTONDOWN` hit-tests against exactly what's on
        /// screen without recomputing combat state on every click.
        last_row_count: usize,
        last_picker_entries: Vec<(Option<u64>, String)>,
    }

    #[cfg(target_os = "windows")]
    impl MeterState {
        fn new(handle: &Arc<AppHandle>, wx: i32, wy: i32) -> Self {
            let cfg = handle.config.lock().unwrap();
            let (locked, meter_enabled, max_rows, idle_secs, font_size, window_w) = (
                cfg.meter_locked,
                cfg.meter_enabled,
                cfg.meter_max_rows.max(1),
                cfg.meter_idle_secs,
                cfg.meter_font_size.max(8) as i32,
                cfg.meter_width.clamp(MIN_METER_WIDTH, MAX_METER_WIDTH),
            );
            drop(cfg);
            Self {
                handle: Arc::clone(handle),
                active_tab: MeterTab::Dps,
                locked,
                meter_enabled,
                max_rows,
                idle_secs,
                font_size,
                window_x: wx,
                window_y: wy,
                window_w,
                hfont_normal: None,
                hfont_bold: None,
                hfont_icon: None,
                last_active_mob_id: None,
                last_footer_total: 0,
                last_change: Instant::now(),
                visible: false,
                force_show: handle.force_show_windows.load(Ordering::Relaxed),
                pinned_mob_id: None,
                mob_picker_open: false,
                last_row_count: 0,
                last_picker_entries: Vec::new(),
            }
        }

        unsafe fn ensure_fonts(&mut self) {
            if self.hfont_normal.is_none() {
                self.hfont_normal = Some(make_font("Segoe UI", self.font_size, false));
            }
            if self.hfont_bold.is_none() {
                self.hfont_bold = Some(make_font("Segoe UI", self.font_size, true));
            }
            if self.hfont_icon.is_none() {
                self.hfont_icon = Some(make_font("Segoe MDL2 Assets", ICON_FONT_PT, false));
            }
        }

        unsafe fn drop_fonts(&mut self) {
            for f in [
                self.hfont_normal.take(),
                self.hfont_bold.take(),
                self.hfont_icon.take(),
            ]
            .into_iter()
            .flatten()
            {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
        }

        /// Reload live-tunable settings from config. Returns true if the
        /// click-through lock state changed and needs to be re-applied to the
        /// window style (covers both the in-window pin click and the settings
        /// dialog's checkbox — the window can't receive clicks while locked,
        /// so this poll is the only way an unlock via the dialog takes effect).
        unsafe fn sync_config(&mut self) -> bool {
            let (new_size, meter_enabled, max_rows, idle_secs, meter_locked) = {
                let cfg = self.handle.config.lock().unwrap();
                (
                    cfg.meter_font_size.max(8) as i32,
                    cfg.meter_enabled,
                    cfg.meter_max_rows.max(1),
                    cfg.meter_idle_secs,
                    cfg.meter_locked,
                )
            };
            if new_size != self.font_size {
                self.font_size = new_size;
                self.drop_fonts();
            }
            self.meter_enabled = meter_enabled;
            self.max_rows = max_rows;
            self.idle_secs = idle_secs;
            let lock_changed = meter_locked != self.locked;
            self.locked = meter_locked;
            self.force_show = self.handle.force_show_windows.load(Ordering::Relaxed);
            lock_changed
        }
    }

    // ── Row layout / height ──────────────────────────────────────────────────

    fn window_height(row_count: usize, picker_rows: usize) -> i32 {
        // + 1 row for the footer (current mob name + cumulative total), always shown,
        // + one row per open mob-picker entry.
        TITLE_H + HEADER_H + ROW_H * (row_count.max(1) as i32 + 1) + ROW_H * picker_rows as i32
    }

    // ── Frame rendering ───────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    #[allow(clippy::too_many_arguments)]
    unsafe fn render_frame(
        hwnd: HWND,
        state: &mut MeterState,
        picker_entries: &[MobPickerEntry],
        rows: &[RowData],
        elapsed_secs: u64,
        mob_name: &str,
        footer_total: u64,
        footer_rate: u64,
    ) {
        state.ensure_fonts();
        let Some(font_normal) = state.hfont_normal else {
            return;
        };
        let Some(font_bold) = state.hfont_bold else {
            return;
        };
        let Some(font_icon) = state.hfont_icon else {
            return;
        };

        let w = state.window_w;
        let h = window_height(rows.len(), picker_entries.len());
        let chrome = compute_chrome_rects(w);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };

        let hdc_screen = GetDC(None);
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(hbm) = CreateDIBSection(hdc_screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(None, hdc_screen);
            return;
        };
        let old_bm = SelectObject(hdc_mem, HGDIOBJ(hbm.0));

        let pix = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        pix.fill(0);

        // ── Panel background ─────────────────────────────────────────────
        let (pr, pg, pb, pa) = PANEL_BG;
        fill_rrect(pix, w, h, 0, 0, w, h, CORNER_R, premult(pr, pg, pb, pa));

        // ── Title bar: tabs + pin + gear ─────────────────────────────────
        let (tr, tg, tb, ta) = TITLE_BG;
        fill_rrect(
            pix,
            w,
            h,
            0,
            0,
            w,
            TITLE_H,
            CORNER_R,
            premult(tr, tg, tb, ta),
        );

        for (tab, rect) in MeterTab::ALL.iter().zip(chrome.tabs.iter()) {
            let active = *tab == state.active_tab;
            let (cr, cg, cb) = if active {
                (255u8, 255u8, 255u8)
            } else {
                (140u8, 140u8, 150u8)
            };
            composite_text(
                pix,
                w,
                h,
                tab.label(),
                if active { font_bold } else { font_normal },
                (cr, cg, cb),
                1.0,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                DT_LEFT | DT_NOCLIP,
            );
        }

        // Chrome icons — Segoe MDL2 Assets glyphs (the same icon font Windows'
        // own Settings/Explorer toolbars use): clean, flat, and properly
        // anti-aliased by the OS's font rasterizer, unlike emoji (which plain
        // GDI text drawing can't render in colour — they showed up blank) or
        // hand-drawn pixel shapes (which just look crude at 15px).
        let icon_color = (150u8, 150u8, 160u8);
        for (codepoint, rect, color) in [
            ("\u{E713}", &chrome.gear, icon_color),  // Setting
            ("\u{E74D}", &chrome.trash, icon_color), // Delete
            ("\u{E8C8}", &chrome.copy, icon_color),  // Copy
        ] {
            composite_text(
                pix,
                w,
                h,
                codepoint,
                font_icon,
                color,
                1.0,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                DT_LEFT | DT_NOCLIP,
            );
        }

        // ── Column header row ─────────────────────────────────────────────
        let (hr, hg, hb, ha) = HEADER_BG;
        fill_rect(
            pix,
            w,
            h,
            0,
            TITLE_H,
            w,
            TITLE_H + HEADER_H,
            premult(hr, hg, hb, ha),
        );

        let name_x = PAD_X + SWATCH_SZ + 6 + 20; // rank gutter + swatch + gap
        let col_w = 56;
        let sec_x = w - PAD_X - col_w;
        let rate_x = sec_x - col_w;
        let amt_x = rate_x - col_w;
        let hdr_y = TITLE_H;
        composite_text(
            pix,
            w,
            h,
            "Name",
            font_normal,
            (150, 150, 160),
            1.0,
            name_x,
            hdr_y,
            amt_x - name_x,
            HEADER_H,
            DT_LEFT | DT_NOCLIP,
        );
        composite_text(
            pix,
            w,
            h,
            state.active_tab.amount_col_label(),
            font_normal,
            (150, 150, 160),
            1.0,
            amt_x,
            hdr_y,
            col_w - 4,
            HEADER_H,
            DT_RIGHT | DT_NOCLIP,
        );
        composite_text(
            pix,
            w,
            h,
            state.active_tab.rate_col_label(),
            font_normal,
            (150, 150, 160),
            1.0,
            rate_x,
            hdr_y,
            col_w - 4,
            HEADER_H,
            DT_RIGHT | DT_NOCLIP,
        );
        composite_text(
            pix,
            w,
            h,
            "Sec",
            font_normal,
            (150, 150, 160),
            1.0,
            sec_x,
            hdr_y,
            col_w - PAD_X,
            HEADER_H,
            DT_RIGHT | DT_NOCLIP,
        );

        // ── Data rows ──────────────────────────────────────────────────────
        for (i, row) in rows.iter().enumerate() {
            let ry = TITLE_H + HEADER_H + i as i32 * ROW_H;
            let is_top = i == 0;

            if is_top {
                let (br, bg, bb, ba) = TOP_ROW_BG;
                fill_rect(pix, w, h, 0, ry, w, ry + ROW_H, premult(br, bg, bb, ba));
            } else if i % 2 == 1 {
                let (br, bg, bb, ba) = ROW_BG_ALT;
                fill_rect(pix, w, h, 0, ry, w, ry + ROW_H, premult(br, bg, bb, ba));
            }

            let font = if is_top { font_bold } else { font_normal };
            let (nr, ng, nb) = if is_top {
                (255, 225, 140)
            } else {
                (225, 225, 230)
            };

            // Rank number.
            composite_text(
                pix,
                w,
                h,
                &format!("{}.", i + 1),
                font_normal,
                (120, 120, 130),
                1.0,
                PAD_X,
                ry,
                18,
                ROW_H,
                DT_LEFT | DT_NOCLIP,
            );
            // Class-colour swatch.
            let (swr, swg, swb) = row.color;
            fill_rrect(
                pix,
                w,
                h,
                PAD_X + 18,
                ry + (ROW_H - SWATCH_SZ) / 2,
                PAD_X + 18 + SWATCH_SZ,
                ry + (ROW_H - SWATCH_SZ) / 2 + SWATCH_SZ,
                2,
                premult(swr, swg, swb, 230),
            );
            // Name (ellipsis-truncated to fit its column).
            composite_text(
                pix,
                w,
                h,
                &row.name,
                font,
                (nr, ng, nb),
                1.0,
                name_x,
                ry,
                amt_x - name_x,
                ROW_H,
                DT_LEFT | DT_END_ELLIPSIS,
            );
            // Damage / Heal total.
            composite_text(
                pix,
                w,
                h,
                &fmt_k(row.total),
                font,
                (nr, ng, nb),
                1.0,
                amt_x,
                ry,
                col_w - 4,
                ROW_H,
                DT_RIGHT | DT_NOCLIP,
            );
            // Rate (DPS/HPS).
            composite_text(
                pix,
                w,
                h,
                &fmt_k(row.rate),
                font,
                (nr, ng, nb),
                1.0,
                rate_x,
                ry,
                col_w - 4,
                ROW_H,
                DT_RIGHT | DT_NOCLIP,
            );
            // Elapsed seconds — same value on every row.
            composite_text(
                pix,
                w,
                h,
                &elapsed_secs.to_string(),
                font,
                (150, 150, 160),
                1.0,
                sec_x,
                ry,
                col_w - PAD_X,
                ROW_H,
                DT_RIGHT | DT_NOCLIP,
            );
        }

        // ── Footer: current mob + cumulative total across all contributors ──
        let footer_y = TITLE_H + HEADER_H + rows.len().max(1) as i32 * ROW_H;
        let (fr, fg, fb, fa) = FOOTER_BG;
        fill_rect(
            pix,
            w,
            h,
            0,
            footer_y,
            w,
            footer_y + ROW_H,
            premult(fr, fg, fb, fa),
        );
        // Footer is clickable — pin icon (if manually selected) + caret hint
        // that it opens the mob picker.
        let caret = if picker_entries.is_empty() {
            "\u{25BE}"
        } else {
            "\u{25B4}"
        };
        let pin_prefix = if state.pinned_mob_id.is_some() {
            "\u{1F4CC} "
        } else {
            ""
        };
        let mob_label = format!("{pin_prefix}{mob_name} {caret}");
        composite_text(
            pix,
            w,
            h,
            &mob_label,
            font_bold,
            (200, 210, 230),
            1.0,
            PAD_X,
            footer_y,
            amt_x - PAD_X,
            ROW_H,
            DT_LEFT | DT_END_ELLIPSIS,
        );
        composite_text(
            pix,
            w,
            h,
            &fmt_k(footer_total),
            font_bold,
            (255, 255, 255),
            1.0,
            amt_x,
            footer_y,
            col_w - 4,
            ROW_H,
            DT_RIGHT | DT_NOCLIP,
        );
        composite_text(
            pix,
            w,
            h,
            &fmt_k(footer_rate),
            font_bold,
            (255, 255, 255),
            1.0,
            rate_x,
            footer_y,
            col_w - 4,
            ROW_H,
            DT_RIGHT | DT_NOCLIP,
        );
        composite_text(
            pix,
            w,
            h,
            &elapsed_secs.to_string(),
            font_normal,
            (150, 150, 160),
            1.0,
            sec_x,
            footer_y,
            col_w - PAD_X,
            ROW_H,
            DT_RIGHT | DT_NOCLIP,
        );

        // ── Mob picker (expanded on footer click) ─────────────────────────
        for (i, entry) in picker_entries.iter().enumerate() {
            let py = footer_y + ROW_H + i as i32 * ROW_H;
            if i % 2 == 1 {
                fill_rect(pix, w, h, 0, py, w, py + ROW_H, premult(255, 255, 255, 8));
            }
            let (dr, dg, db) = entry.dot;
            fill_rrect(
                pix,
                w,
                h,
                PAD_X,
                py + (ROW_H - SWATCH_SZ) / 2,
                PAD_X + SWATCH_SZ,
                py + (ROW_H - SWATCH_SZ) / 2 + SWATCH_SZ,
                SWATCH_SZ / 2,
                premult(dr, dg, db, 230),
            );
            let selected = entry.id == state.pinned_mob_id
                || (entry.id.is_none() && state.pinned_mob_id.is_none());
            let (tr2, tg2, tb2) = if selected {
                (255, 255, 255)
            } else {
                (200, 200, 210)
            };
            composite_text(
                pix,
                w,
                h,
                &entry.label,
                if selected { font_bold } else { font_normal },
                (tr2, tg2, tb2),
                1.0,
                PAD_X + SWATCH_SZ + 8,
                py,
                w - PAD_X - SWATCH_SZ - 8 - PAD_X,
                ROW_H,
                DT_LEFT | DT_END_ELLIPSIS,
            );
        }

        let dst_pt = POINT {
            x: state.window_x,
            y: state.window_y,
        };
        let win_size = SIZE { cx: w, cy: h };
        let src_pt = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: 0,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            hdc_screen,
            Some(&dst_pt),
            Some(&win_size),
            hdc_mem,
            Some(&src_pt),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        SelectObject(hdc_mem, old_bm);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
    }

    // ── Entry point ───────────────────────────────────────────────────────────

    pub fn spawn_dps_meter(handle: Arc<AppHandle>) {
        std::thread::Builder::new()
            .name("froklog-dps-meter".into())
            .spawn(move || run_meter_thread(handle))
            .expect("spawn dps meter thread");
    }

    #[cfg(target_os = "windows")]
    fn run_meter_thread(handle: Arc<AppHandle>) {
        unsafe {
            let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let hinstance = windows::Win32::Foundation::HINSTANCE(hmodule.0);
            let class_w: Vec<u16> = CLASS_NAME.encode_utf16().collect();

            let wc = WNDCLASSEXW {
                cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(meter_wnd_proc),
                hInstance: hinstance,
                lpszClassName: PCWSTR(class_w.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH(std::ptr::null_mut()),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let (wx, wy) = initial_position(&handle.config.lock().unwrap());
            let state = Box::new(MeterState::new(&handle, wx, wy));
            let initial_w = state.window_w;
            let state_ptr = Box::into_raw(state);

            // No WS_VISIBLE — the first WM_TIMER tick decides whether to show it,
            // so a disabled/idle meter never flashes on screen at startup.
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(class_w.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                wx,
                wy,
                initial_w,
                1,
                None,
                None,
                hinstance,
                Some(state_ptr as *const c_void),
            )
            .expect("CreateWindowExW dps meter");

            handle.meter_hwnd.store(hwnd.0 as isize, Ordering::Relaxed);

            windows::Win32::UI::WindowsAndMessaging::SetTimer(
                hwnd,
                TIMER_METER,
                TIMER_INTERVAL_MS,
                None,
            );

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn run_meter_thread(_handle: Arc<AppHandle>) {}

    // ── Window procedure ──────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe extern "system" fn meter_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MeterState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_TIMER => {
                if state.handle.quit.load(Ordering::Relaxed) {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }

                let lock_changed = state.sync_config();
                if lock_changed {
                    apply_lock_style(hwnd, state.locked);
                }

                let cs = state.handle.combat_state.load();
                // Drop a stale pin before resolving — a pinned mob that fell
                // off mob_list (e.g. an engine restart) should silently fall
                // back to auto-follow rather than leaving the meter stuck.
                if let Some(pid) = state.pinned_mob_id {
                    if !cs.mob_list.iter().any(|m| m.id == pid) {
                        state.pinned_mob_id = None;
                    }
                }
                let view_mob_id = resolve_view_mob_id(&cs, state.pinned_mob_id);

                let show = state.force_show || (state.meter_enabled && view_mob_id.is_some());
                if !show {
                    if state.visible {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        state.visible = false;
                    }
                    state.mob_picker_open = false;
                    return LRESULT(0);
                }

                let Some(mob_id) = view_mob_id else {
                    // force_show with no real combat data yet — draw a
                    // placeholder so there's something to grab and drag.
                    if !state.visible {
                        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        state.visible = true;
                    }
                    render_frame(
                        hwnd,
                        state,
                        &[],
                        &[],
                        0,
                        "Drag to position — DPS Meter",
                        0,
                        0,
                    );
                    return LRESULT(0);
                };
                let snap = compute_snapshot(&cs, mob_id, state.active_tab, state.max_rows);
                let MeterSnapshot {
                    ref mob_name,
                    ref rows,
                    footer_total,
                    footer_rate,
                    elapsed_secs,
                } = snap;

                let picker_entries = if state.mob_picker_open {
                    build_mob_picker_entries(&cs)
                } else {
                    Vec::new()
                };
                state.last_row_count = rows.len();
                state.last_picker_entries = picker_entries
                    .iter()
                    .map(|e| (e.id, e.label.clone()))
                    .collect();

                // Idle detection: hide if nothing has changed for idle_secs.
                // Suppressed while the picker is open so it can't close itself
                // mid-selection.
                if Some(mob_id) != state.last_active_mob_id
                    || footer_total != state.last_footer_total
                {
                    state.last_change = Instant::now();
                    state.last_active_mob_id = Some(mob_id);
                    state.last_footer_total = footer_total;
                }
                if !state.force_show
                    && !state.mob_picker_open
                    && state.idle_secs > 0
                    && state.last_change.elapsed() > Duration::from_secs(state.idle_secs as u64)
                {
                    if state.visible {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        state.visible = false;
                    }
                    return LRESULT(0);
                }

                if !state.visible {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    state.visible = true;
                }
                render_frame(
                    hwnd,
                    state,
                    &picker_entries,
                    rows,
                    elapsed_secs,
                    mob_name,
                    footer_total,
                    footer_rate,
                );
                LRESULT(0)
            }

            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }

            WM_LBUTTONDOWN => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let chrome = compute_chrome_rects(state.window_w);

                // Footer / mob-picker hit-testing — uses the row count and
                // picker entries cached from the last render so it matches
                // exactly what's on screen right now.
                let footer_y = TITLE_H + HEADER_H + state.last_row_count.max(1) as i32 * ROW_H;
                let footer_rect = RECT {
                    left: 0,
                    top: footer_y,
                    right: state.window_w,
                    bottom: footer_y + ROW_H,
                };
                let clicked_footer = point_in_rect(x, y, &footer_rect);
                if state.mob_picker_open {
                    let picker_top = footer_y + ROW_H;
                    let picker_bottom = picker_top + state.last_picker_entries.len() as i32 * ROW_H;
                    if y >= picker_top && y < picker_bottom {
                        let idx = ((y - picker_top) / ROW_H) as usize;
                        if let Some((id, _)) = state.last_picker_entries.get(idx) {
                            state.pinned_mob_id = *id;
                        }
                        state.mob_picker_open = false;
                        return LRESULT(0);
                    }
                    // Clicking the footer again (or anywhere else) closes the
                    // picker. A click elsewhere (tab/gear/pin/title bar) also
                    // falls through below so it still takes effect this click.
                    state.mob_picker_open = false;
                    if clicked_footer {
                        return LRESULT(0);
                    }
                } else if clicked_footer {
                    state.mob_picker_open = true;
                    return LRESULT(0);
                }

                if let Some((tab, _)) = MeterTab::ALL
                    .iter()
                    .zip(chrome.tabs.iter())
                    .find(|(_, r)| point_in_rect(x, y, r))
                {
                    state.active_tab = *tab;
                    return LRESULT(0);
                }
                if point_in_rect(x, y, &chrome.gear) {
                    let was_open = state.handle.settings_open.swap(true, Ordering::Relaxed);
                    if was_open {
                        // Already open — bring it to the front and switch it to
                        // the DPS Meter tab instead of no-op'ing on a second
                        // click. PostMessageW (not SendMessageW) since that
                        // window pumps its own message loop on a different
                        // thread.
                        let hwnd_val = state.handle.settings_hwnd.load(Ordering::Relaxed);
                        if hwnd_val != 0 {
                            let _ = PostMessageW(
                                HWND(hwnd_val as *mut c_void),
                                crate::overlay_config_win::overlay_config::WM_SWITCH_TAB,
                                WPARAM(
                                    crate::overlay_config_win::overlay_config::TAB_DPS_METER
                                        as usize,
                                ),
                                LPARAM(0),
                            );
                        }
                    } else {
                        crate::overlay_config_win::overlay_config::open_settings(
                            Arc::clone(&state.handle),
                            crate::overlay_config_win::overlay_config::TAB_DPS_METER,
                        );
                    }
                    return LRESULT(0);
                }
                if point_in_rect(x, y, &chrome.trash) {
                    // Clears all combat totals (parser-side), preserving
                    // lines_parsed/player identity — same reset the rest of
                    // the app would use, just triggered from the meter.
                    state.handle.reset_flag.store(true, Ordering::Relaxed);
                    state.pinned_mob_id = None;
                    state.mob_picker_open = false;
                    return LRESULT(0);
                }
                if point_in_rect(x, y, &chrome.copy) {
                    let cs = state.handle.combat_state.load();
                    if let Some(mob_id) = resolve_view_mob_id(&cs, state.pinned_mob_id) {
                        let snap = compute_snapshot(&cs, mob_id, state.active_tab, state.max_rows);
                        let line = build_summary_line(&snap, state.active_tab);
                        copy_to_clipboard(&line);
                    }
                    return LRESULT(0);
                }
                if point_in_rect(x, y, &chrome.title_bar) {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                    let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                        hwnd,
                        WM_NCLBUTTONDOWN,
                        WPARAM(windows::Win32::UI::WindowsAndMessaging::HTCAPTION as usize),
                        LPARAM(0),
                    );
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }

            // Report the left/right few pixels as resize-grip regions so
            // DefWindowProcW's built-in interactive-resize loop kicks in —
            // this works without WS_THICKFRAME, the same way our title bar
            // drags via a manual HTCAPTION hand-off below. Top/bottom aren't
            // offered: height stays content-driven (row count).
            WM_NCHITTEST => {
                let sx = (lparam.0 & 0xFFFF) as i16 as i32;
                let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut rect = RECT::default();
                if !state.locked && GetWindowRect(hwnd, &mut rect).is_ok() {
                    const MARGIN: i32 = 6;
                    if sy >= rect.top && sy < rect.bottom {
                        if sx >= rect.left && sx < rect.left + MARGIN {
                            return LRESULT(HTLEFT as isize);
                        }
                        if sx < rect.right && sx >= rect.right - MARGIN {
                            return LRESULT(HTRIGHT as isize);
                        }
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }

            // Repaint immediately on width changes (rather than waiting for
            // the next ~200ms WM_TIMER tick) so an interactive resize drag
            // doesn't look stale/stretched mid-drag.
            WM_SIZE => {
                let new_w = (lparam.0 & 0xFFFF) as i32;
                if new_w > 0 {
                    state.window_w = new_w.clamp(MIN_METER_WIDTH, MAX_METER_WIDTH);
                    let cs = state.handle.combat_state.load();
                    if let Some(mob_id) = resolve_view_mob_id(&cs, state.pinned_mob_id) {
                        let snap = compute_snapshot(&cs, mob_id, state.active_tab, state.max_rows);
                        let picker_entries = if state.mob_picker_open {
                            build_mob_picker_entries(&cs)
                        } else {
                            Vec::new()
                        };
                        render_frame(
                            hwnd,
                            state,
                            &picker_entries,
                            &snap.rows,
                            snap.elapsed_secs,
                            &snap.mob_name,
                            snap.footer_total,
                            snap.footer_rate,
                        );
                    }
                }
                LRESULT(0)
            }

            WM_EXITSIZEMOVE => {
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    state.window_x = rect.left;
                    state.window_y = rect.top;
                    state.window_w =
                        (rect.right - rect.left).clamp(MIN_METER_WIDTH, MAX_METER_WIDTH);
                    let mut cfg = state.handle.config.lock().unwrap();
                    cfg.meter_x = rect.left;
                    cfg.meter_y = rect.top;
                    cfg.meter_width = state.window_w;
                    cfg.save();
                }
                LRESULT(0)
            }

            WM_DESTROY => {
                state.drop_fonts();
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    /// Returns `(x, y)` — top-left position. Defaults to a fixed offset near
    /// the right edge of the primary monitor if never moved.
    fn initial_position(cfg: &Config) -> (i32, i32) {
        #[cfg(target_os = "windows")]
        {
            if cfg.meter_x >= 0 && cfg.meter_y >= 0 {
                return (cfg.meter_x, cfg.meter_y);
            }
            unsafe {
                let sw = GetSystemMetrics(SM_CXSCREEN);
                let w = cfg.meter_width.clamp(MIN_METER_WIDTH, MAX_METER_WIDTH);
                (sw - w - 40, 160)
            }
        }
        #[cfg(not(target_os = "windows"))]
        (cfg.meter_x.max(0), cfg.meter_y.max(0))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fmt_k_values() {
            assert_eq!(fmt_k(0), "0");
            assert_eq!(fmt_k(999), "999");
            assert_eq!(fmt_k(1000), "1K");
            assert_eq!(fmt_k(1500), "1.5K");
            assert_eq!(fmt_k(1_000_000), "1M");
            assert_eq!(fmt_k(2_500_000), "2.5M");
        }

        #[test]
        fn class_color_known_and_unknown() {
            assert_eq!(class_color("WAR"), (0xaf, 0x80, 0x3c));
            assert_eq!(class_color("NEC"), (0x01, 0x64, 0xfa));
            assert_eq!(class_color(""), (0x46, 0x46, 0x50));
            assert_eq!(class_color("XYZ"), (0x46, 0x46, 0x50));
        }
    }
}
