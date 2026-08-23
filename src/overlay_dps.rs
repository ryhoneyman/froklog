/// Live DPS/Tank/Heal/Heal-Received meter overlay.
///
/// Real Slint window (`ui/overlay_dps.slint`, `DpsMeterWindow`) replacing
/// the old `WS_EX_LAYERED` popup. Reads `AppHandle.combat_state` directly —
/// the client already builds a fully aggregated `CombatState` locally (see
/// `main.rs::run_engine_once`), so no server round-trip is needed. Same
/// "keep the business logic, replace the paint step" approach as
/// overlay.rs; see its module doc for the fuller explanation of the
/// slint::Timer-on-the-UI-thread change.
///
/// Scope: the mob instance the player is currently engaged with (see
/// `resolve_view_mob_id`/`player_engaged` below), ranked by whichever of
/// the four tabs is selected — or a manually pinned mob instance, picked
/// from the mob-picker popup that expands below the footer (see
/// `build_mob_picker_entries`).
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_dps {
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use once_cell::sync::Lazy;
    use slint::{Color, ComponentHandle, Model, ModelRc, VecModel};
    use std::sync::Mutex;

    use crate::overlay_registry::overlay_registry::OverlayKind;
    use crate::overlay_shell;
    use crate::state::{CombatState, EntityCombatStats};
    use crate::tray::tray::AppHandle;

    include!(concat!(env!("OUT_DIR"), "/overlay_dps.rs"));

    const TIMER_INTERVAL_MS: u64 = 200;

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

        fn from_index(i: usize) -> Self {
            Self::ALL.get(i).copied().unwrap_or(Self::Dps)
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

        fn bucket<'a>(
            &self,
            cs: &'a CombatState,
            mob_id: u64,
        ) -> Option<&'a std::collections::HashMap<String, EntityCombatStats>> {
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

        /// Per-row accordion contents: (label, amount, is_spell), melee/skill
        /// types and spells merged into ONE list sorted highest-to-lowest
        /// together (not grouped by type-then-spell) — Tank has no spell
        /// side (parser.rs never populates `damage_by_spell` for the
        /// tanking bucket, only `damage_by_type`, since incoming spell
        /// damage is filed under the "dot"/"ds" *types*, not by spell name),
        /// so it's effectively already homogeneous.
        fn breakdown_of(&self, stats: &EntityCombatStats) -> Vec<(String, u64, bool)> {
            fn collect(map: &HashMap<String, u64>, is_spell: bool) -> Vec<(String, u64, bool)> {
                map.iter().map(|(k, n)| (k.clone(), *n, is_spell)).collect()
            }
            let mut v = match self {
                Self::Dps => {
                    let mut v = collect(&stats.damage_by_type, false);
                    v.extend(collect(&stats.damage_by_spell, true));
                    v
                }
                Self::Tank => collect(&stats.damage_by_type, false),
                Self::Heal => collect(&stats.heals_by_spell, true),
                Self::HealReceived => collect(&stats.healed_received_by_spell, true),
            };
            v.sort_by_key(|(_, n, _)| std::cmp::Reverse(*n));
            v
        }
    }

    // ── Row data ──────────────────────────────────────────────────────────────

    struct RowData {
        name: String,
        color: (u8, u8, u8),
        /// One entry per class (EQ Legends multiclass = up to 3), for the
        /// gradient bar. Empty = class unknown.
        colors: Vec<(u8, u8, u8)>,
        total: u64,
        rate: u64,
        /// Fraction of the GROUP total (all contributors, not just the rows
        /// that fit on screen), for the share bar.
        share: f32,
        /// Whether this row's accordion is open — from `expanded_names`,
        /// keyed by entity name so it survives tab/mob switches until the
        /// user clicks it closed again.
        expanded: bool,
        /// Per-type/spell breakdown (label, amount, rate, is_spell), only
        /// computed when `expanded` (no point building it for rows nobody's
        /// looking at).
        breakdown: Vec<(String, u64, u64, bool)>,
    }

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
            .filter(|(name, _)| tab != MeterTab::Tank || !cs.confirmed_mobs.contains(name.as_str()))
            .collect()
    }

    fn mob_elapsed_secs(cs: &CombatState, mob_id: u64) -> f64 {
        cs.mob_list
            .iter()
            .find(|m| m.id == mob_id)
            .map(|m| m.last_seen.duration_since(m.first_seen).as_secs_f64())
            .unwrap_or_else(|| cs.elapsed_secs())
            .max(0.001)
    }

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

    /// Internal computation of a mob-picker row — kept distinct from the
    /// Slint-generated `MobPickerEntry` (which can't represent
    /// `Option<u64>`), same split as `RowData`/`MeterRow` below.
    struct MobPickerCandidate {
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
    fn build_mob_picker_entries(cs: &CombatState) -> Vec<MobPickerCandidate> {
        let mut entries = vec![MobPickerCandidate {
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
            entries.push(MobPickerCandidate {
                id: Some(m.id),
                label: m.name.clone(),
                dot,
            });
        }
        entries
    }

    fn mob_picker_entries_model(
        entries: &[MobPickerCandidate],
        pinned: Option<u64>,
    ) -> ModelRc<MobPickerEntry> {
        let rows: Vec<MobPickerEntry> = entries
            .iter()
            .map(|e| MobPickerEntry {
                id: e.id.map(|id| id as i32).unwrap_or(-1),
                label: e.label.as_str().into(),
                dot_color: Color::from_rgb_u8(e.dot.0, e.dot.1, e.dot.2),
                selected: e.id == pinned,
            })
            .collect();
        ModelRc::new(VecModel::from(rows))
    }

    fn build_rows(
        cs: &CombatState,
        entries: &[(&String, &EntityCombatStats)],
        tab: MeterTab,
        max_rows: usize,
        elapsed: f64,
        expanded_names: &HashSet<String>,
    ) -> Vec<RowData> {
        let mut sorted = entries.to_vec();
        sorted.sort_by_key(|(_, s)| std::cmp::Reverse(tab.total_of(s)));
        // Group total BEFORE truncation — a row's share is its slice of the
        // whole fight, matching the footer's cumulative total, not just the
        // rows that fit on screen.
        let group_total: u64 = sorted.iter().map(|(_, s)| tab.total_of(s)).sum();
        sorted.truncate(max_rows);

        sorted
            .into_iter()
            .map(|(name, s)| {
                let total = tab.total_of(s);
                let rate = (total as f64 / elapsed).round() as u64;
                let classes = resolve_classes(cs, name);
                let colors: Vec<(u8, u8, u8)> =
                    classes.iter().take(3).map(|c| class_color(c)).collect();
                let expanded = expanded_names.contains(name.as_str());
                RowData {
                    name: name.clone(),
                    color: colors.first().copied().unwrap_or_else(|| class_color("")),
                    colors,
                    total,
                    rate,
                    share: if group_total > 0 {
                        total as f32 / group_total as f32
                    } else {
                        0.0
                    },
                    expanded,
                    breakdown: if expanded {
                        tab.breakdown_of(s)
                            .into_iter()
                            .map(|(label, amount, is_spell)| {
                                let rate = (amount as f64 / elapsed).round() as u64;
                                (label, amount, rate, is_spell)
                            })
                            .collect()
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect()
    }

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
        expanded_names: &HashSet<String>,
    ) -> MeterSnapshot {
        let elapsed = mob_elapsed_secs(cs, mob_id);
        let entries = tab_entries(cs, mob_id, tab);
        let rows = build_rows(cs, &entries, tab, max_rows, elapsed, expanded_names);
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

    /// EQ class short-code -> accent colour, matching `CLASS_COLOR` in
    /// static/stream.html exactly (verified directly against the current
    /// source while porting this file — see the note in ui/theme.slint's
    /// ClassColors global about an earlier stale copy of this table).
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

    // ── Last-seen class cache ────────────────────────────────────────────
    //
    // `CombatState.player_classes` only fills from /who output, so until
    // the player runs /who in a session every row renders in the default
    // gray. This sidecar remembers each name's last known classes across
    // restarts (classes-cache.toml beside config.toml); a fresh /who
    // observation overwrites the entry. Only written when an entry actually
    // changes — steady state does no disk IO.
    static CLASS_CACHE: Lazy<Mutex<HashMap<String, Vec<String>>>> = Lazy::new(|| {
        Mutex::new(
            std::fs::read_to_string(crate::config::classes_cache_path())
                .ok()
                .and_then(|s| toml::from_str(&s).ok())
                .unwrap_or_default(),
        )
    });

    /// Live classes win and refresh the cache; unknown names fall back to
    /// their cached classes (empty if never seen).
    fn resolve_classes(cs: &CombatState, name: &str) -> Vec<String> {
        let mut cache = CLASS_CACHE.lock().unwrap();
        match cs.player_classes.get(name) {
            Some(live) if !live.is_empty() => {
                if cache.get(name) != Some(live) {
                    cache.insert(name.to_string(), live.clone());
                    if let Ok(body) = toml::to_string(&*cache) {
                        let path = crate::config::classes_cache_path();
                        if let Some(dir) = path.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let _ = std::fs::write(path, body);
                    }
                }
                live.clone()
            }
            _ => cache.get(name).cloned().unwrap_or_default(),
        }
    }

    fn fmt_k(n: u64) -> String {
        if n >= 1_000_000 {
            let s = format!("{:.1}M", n as f64 / 1_000_000.0);
            s.strip_suffix(".0M").map(|p| format!("{p}M")).unwrap_or(s)
        } else if n >= 1_000 {
            let s = format!("{:.1}K", n as f64 / 1_000.0);
            s.strip_suffix(".0K").map(|p| format!("{p}K")).unwrap_or(s)
        } else {
            n.to_string()
        }
    }

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

    // ── State ─────────────────────────────────────────────────────────────────

    struct MeterState {
        handle: Arc<AppHandle>,
        meter_enabled: bool,
        max_rows: usize,
        idle_secs: u32,
        font_size: f32,
        last_active_mob_id: Option<u64>,
        last_footer_total: u64,
        last_change: Instant,
        visible: bool,
        force_show: bool,
        locked: bool,
        hide_inactive_secs: u32,
        width: i32,
        share_bars: bool,
        amount_w: i32,
        rate_w: i32,
        bar_w: i32,
        sec_w: i32,
        tick_count: u64,
    }

    impl MeterState {
        fn new(handle: &Arc<AppHandle>) -> Self {
            let cfg = handle.config.lock().unwrap();
            Self {
                handle: Arc::clone(handle),
                meter_enabled: cfg.overlay_meter.enabled,
                max_rows: cfg.meter_max_rows.max(1),
                idle_secs: cfg.meter_idle_secs,
                font_size: cfg.meter_font_size.max(8) as f32,
                last_active_mob_id: None,
                last_footer_total: 0,
                last_change: Instant::now(),
                visible: false,
                force_show: handle.force_show_windows.load(Ordering::Relaxed),
                locked: cfg.overlay_meter.locked,
                hide_inactive_secs: cfg.overlay_hide_inactive_secs,
                width: cfg.meter_width,
                share_bars: cfg.meter_share_bars,
                amount_w: cfg.meter_amount_w,
                rate_w: cfg.meter_rate_w,
                bar_w: cfg.meter_bar_w,
                sec_w: cfg.meter_sec_w,
                tick_count: 0,
            }
        }

        fn sync_config(&mut self) {
            let cfg = self.handle.config.lock().unwrap();
            self.meter_enabled = cfg.overlay_meter.enabled;
            self.max_rows = cfg.meter_max_rows.max(1);
            self.idle_secs = cfg.meter_idle_secs;
            self.font_size = cfg.meter_font_size.max(8) as f32;
            self.locked = cfg.overlay_meter.locked;
            self.hide_inactive_secs = cfg.overlay_hide_inactive_secs;
            self.width = cfg.meter_width;
            self.share_bars = cfg.meter_share_bars;
            self.amount_w = cfg.meter_amount_w;
            self.rate_w = cfg.meter_rate_w;
            self.bar_w = cfg.meter_bar_w;
            self.sec_w = cfg.meter_sec_w;
            drop(cfg);
            let new_force_show = self.handle.force_show_windows.load(Ordering::Relaxed);
            if new_force_show != self.force_show {
                tracing::info!(
                    "dps meter: force_show {} -> {}",
                    self.force_show,
                    new_force_show
                );
            }
            self.force_show = new_force_show;
        }
    }

    /// `colors[i]` at the bar's alpha, clamped to the last entry (or the
    /// unknown-class gray for an empty list).
    fn bar_col(colors: &[(u8, u8, u8)], i: usize) -> Color {
        let (r, g, b) = colors
            .get(i.min(colors.len().saturating_sub(1)))
            .copied()
            .unwrap_or((70, 70, 80));
        Color::from_argb_u8(200, r, g, b)
    }

    fn to_meter_row(r: &RowData) -> MeterRow {
        MeterRow {
            name: r.name.as_str().into(),
            class_color: Color::from_rgb_u8(r.color.0, r.color.1, r.color.2),
            amount_label: fmt_k(r.total).into(),
            rate_label: fmt_k(r.rate).into(),
            share: r.share,
            share_label: format!("{:.0}%", r.share * 100.0).into(),
            // Fill colors, alpha pre-applied; the .slint side blends them
            // with the interim client's gradient stop recipe when 2–3
            // classes are present (EQ Legends multiclass).
            bar_class_count: r.colors.len().min(3) as i32,
            bar_c0: bar_col(&r.colors, 0),
            bar_c1: bar_col(&r.colors, 1),
            bar_c2: bar_col(&r.colors, 2),
            expanded: r.expanded,
            breakdown: breakdown_model(r.total, &r.breakdown),
        }
    }

    /// Capitalizes a melee/skill type key ("backstab" -> "Backstab") for
    /// display — the parser always stores these lowercase (see
    /// `patterns::normalize_verb` and the "dot"/"ds"/"riposte" literals in
    /// parser.rs). "ds" (damage shield) is the one key that reads better as
    /// an all-caps initialism than a capitalized word.
    fn capitalize_type_label(s: &str) -> String {
        if s.eq_ignore_ascii_case("ds") {
            return "DS".to_string();
        }
        let mut chars = s.chars();
        match chars.next() {
            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }

    /// White for melee/skill types, orange for spells — lets the accordion
    /// distinguish the two at a glance without a separate label/column. The
    /// %-bar's own fill color is NOT one of these — see
    /// overlay_dps.slint's breakdown accordion, which hardcodes one fixed
    /// uniform color for every entry's bar regardless of melee-vs-spell (a
    /// two-tone bar, or one keyed to the row's class color, both read as
    /// distracting once tried — the bar just needs to contrast with its own
    /// "NN%" text, not echo the label beside it).
    const MELEE_LABEL_COLOR: (u8, u8, u8) = (255, 255, 255);
    const SPELL_LABEL_COLOR: (u8, u8, u8) = (255, 167, 51);

    /// Builds the accordion rows for one meter row. Each entry's `bar_frac`
    /// is its share of that row's own total (not the group total the outer
    /// share bar uses), same denominator `total_of()`/`build_rows` already
    /// used for `RowData.total`.
    fn breakdown_model(
        row_total: u64,
        entries: &[(String, u64, u64, bool)],
    ) -> ModelRc<BreakdownEntry> {
        let v: Vec<BreakdownEntry> = entries
            .iter()
            .map(|(label, amount, rate, is_spell)| {
                let bar_frac = if row_total > 0 {
                    (*amount as f32 / row_total as f32).min(1.0)
                } else {
                    0.0
                };
                let pct = if row_total > 0 {
                    *amount as f64 / row_total as f64 * 100.0
                } else {
                    0.0
                };
                let (r, g, b) = if *is_spell {
                    SPELL_LABEL_COLOR
                } else {
                    MELEE_LABEL_COLOR
                };
                let display_label = if *is_spell {
                    label.clone()
                } else {
                    capitalize_type_label(label)
                };
                BreakdownEntry {
                    label: display_label.as_str().into(),
                    amount_label: fmt_k(*amount).into(),
                    rate_label: fmt_k(*rate).into(),
                    pct_label: format!("{pct:.0}%").into(),
                    bar_frac,
                    label_color: Color::from_rgb_u8(r, g, b),
                }
            })
            .collect();
        ModelRc::new(VecModel::from(v))
    }

    /// Updates `model` to match `new_rows`, mutating existing rows' DATA in
    /// place via `set_row_data` rather than replacing the model wholesale
    /// every tick. Slint's `for` loop keeps a row's already-instantiated
    /// component tree alive across a `set_row_data` call (only its bound
    /// properties change) but tears the whole thing down on a full model
    /// reset — which is what destroyed the swatch/name TouchArea's
    /// in-flight press state on every 200ms tick before this fix, forcing a
    /// click's press-then-release to land within a single tick by luck to
    /// register at all. Rows are only pushed/removed at the tail when the
    /// row COUNT itself changes (the ranked top-N gaining or losing a
    /// contributor) — unavoidable there since there's no existing row
    /// component to preserve.
    fn sync_rows_model(model: &VecModel<MeterRow>, new_rows: &[RowData]) {
        let old_len = model.row_count();
        let new_len = new_rows.len();
        for (i, r) in new_rows.iter().enumerate().take(old_len) {
            model.set_row_data(i, to_meter_row(r));
        }
        for r in new_rows.iter().skip(old_len) {
            model.push(to_meter_row(r));
        }
        for _ in new_len..old_len {
            model.remove(model.row_count() - 1);
        }
    }

    // ── Window creation & tick ────────────────────────────────────────────────

    /// Creates the DPS meter window and starts its refresh timer. Must run
    /// on the Slint UI thread — see tray.rs's module doc.
    pub fn create_meter_window(handle: Arc<AppHandle>) {
        tracing::info!("dps meter created, handle={:p}", Arc::as_ptr(&handle));
        let window = DpsMeterWindow::new().expect("create dps meter window");
        // NOT called here — see overlay.rs's create_alert_window for why
        // (the native window doesn't exist yet at creation time). Called on
        // first `.show()` below instead (both places this window shows).
        window.set_amount_col_label("Dmg".into());
        window.set_rate_col_label("DPS".into());
        // A single long-lived model, set ONCE here and mutated in place
        // every tick (see `sync_rows_model`) rather than replaced with a
        // fresh `VecModel` each time — swapping the whole model every
        // 200ms destroyed and recreated every row's component tree
        // (including the swatch/name TouchArea that opens its accordion),
        // which killed any in-flight press-then-release gesture that
        // straddled two ticks. A real click needed to land entirely within
        // one 200ms window by luck, hence needing several tries. Mutating
        // row DATA in place keeps each row's TouchArea instance alive
        // across ticks, so a click completes reliably regardless of timing.
        let rows_model_handle: Rc<VecModel<MeterRow>> =
            Rc::new(VecModel::from(Vec::<MeterRow>::new()));
        window.set_rows(ModelRc::from(rows_model_handle.clone()));

        let cfg_x_y = {
            let cfg = handle.config.lock().unwrap();
            (cfg.overlay_meter.x, cfg.overlay_meter.y)
        };
        // (-1, -1) is the never-positioned sentinel; any OTHER value is a
        // real saved position — including negative coordinates, which are
        // routine on multi-monitor layouts (a monitor left of or above the
        // primary) and whenever a window is nudged past a screen's top/left
        // edge. Gating on >= 0 silently discarded those on every restart.
        if cfg_x_y != (-1, -1) {
            window.window().set_position(slint::WindowPosition::Logical(
                slint::LogicalPosition::new(cfg_x_y.0 as f32, cfg_x_y.1 as f32),
            ));
        }

        // Drag-to-move — see overlay.rs's create_alert_window for why this
        // exists and how it works (same pattern, same reasoning).
        //
        // Unlike the other two overlays, this window's height is
        // content-driven (`height: panel.preferred-height` in
        // overlay_dps.slint) and its content refreshes every
        // `TIMER_INTERVAL_MS` (200ms) from live combat data — including
        // while a drag is in progress, since the OS's interactive-move
        // loop (entered via `drag_window()` below) keeps pumping this
        // thread's timers the whole time it's active (same mechanism
        // documented in overlay.rs's `dragging` flag history). If the
        // row count changes mid-drag (very likely if you're repositioning
        // this window while actively in combat, which is the whole point
        // of it), the window resizes itself while Windows is simultaneously
        // tracking it relative to the cursor — reported as the dropped-at Y
        // position not matching where the window actually ends up. `dragging`
        // freezes all content/size updates for the timer tick's duration so
        // nothing changes under the OS's drag operation.
        let dragging = Rc::new(Cell::new(false));
        // Manually selected mob instance, overriding auto-follow — set by
        // `on_pick_mob` below, read/cleared (if the pinned mob falls off
        // `mob_list`, e.g. after an engine restart) by the timer tick.
        // Shared via `Rc<Cell<_>>` rather than living on `MeterState`
        // because `on_pick_mob`/`on_mob_picker_clicked` and the timer tick
        // are separate closures — same reasoning as `dragging` above.
        let pinned_mob_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        // Whether the mob picker (opened by clicking the footer) is expanded.
        let mob_picker_open: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Names whose row accordion (damage/heal-by-type breakdown) is
        // pinned open — set by clicking a row's class swatch or name in
        // `on_row_clicked` below. Deliberately never cleared on tab/mob
        // switch or by anything else: the whole point is that it stays open
        // until the user clicks it closed again.
        let expanded_names: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));

        window.on_drag_requested({
            let weak = window.as_weak();
            let handle = Arc::clone(&handle);
            let dragging = Rc::clone(&dragging);
            move || {
                // On Windows `begin_drag` blocks for the native move loop,
                // so this brackets the drag exactly as before; on Linux the
                // manual move is timer-driven and returns immediately (same
                // as `drag_window()` already did there).
                dragging.set(true);
                overlay_shell::overlay_shell::begin_drag(
                    weak.clone(),
                    Arc::clone(&handle),
                    OverlayKind::Meter,
                );
                dragging.set(false);
            }
        });

        // Header column dividers: live drags update config in memory (so
        // the tick loop's own property pushes agree mid-drag); the commit
        // on release persists to disk.
        window.on_col_widths_changed({
            let handle = Arc::clone(&handle);
            move |amount_w, rate_w, bar_w, sec_w| {
                let mut cfg = handle.config.lock().unwrap();
                cfg.meter_amount_w = amount_w;
                cfg.meter_rate_w = rate_w;
                cfg.meter_bar_w = bar_w;
                cfg.meter_sec_w = sec_w;
            }
        });
        window.on_col_widths_commit({
            let handle = Arc::clone(&handle);
            move || {
                handle.config.lock().unwrap().save();
            }
        });

        window.on_resize_requested({
            let weak = window.as_weak();
            let handle = Arc::clone(&handle);
            move || {
                overlay_shell::overlay_shell::begin_width_resize(
                    weak.clone(),
                    Arc::clone(&handle),
                    OverlayKind::Meter,
                    |w, px| w.set_win_width(px),
                );
            }
        });

        window.on_tab_clicked({
            let weak = window.as_weak();
            move |tab| {
                if let Some(ui) = weak.upgrade() {
                    let tab = MeterTab::from_index(tab as usize);
                    ui.set_amount_col_label(tab.amount_col_label().into());
                    ui.set_rate_col_label(tab.rate_col_label().into());
                }
            }
        });
        window.on_open_settings({
            let handle = Arc::clone(&handle);
            move || {
                if !handle.settings_open.swap(true, Ordering::Relaxed) {
                    crate::settings_window::settings_window::open_settings(
                        Arc::clone(&handle),
                        TAB_OVERLAYS,
                    );
                } else {
                    crate::settings_window::settings_window::raise_settings(TAB_OVERLAYS);
                }
            }
        });
        window.on_clear_stats({
            let handle = Arc::clone(&handle);
            move || {
                handle.reset_flag.store(true, Ordering::Relaxed);
            }
        });
        window.on_copy_clicked({
            let handle = Arc::clone(&handle);
            let weak = window.as_weak();
            let pinned_mob_id = Rc::clone(&pinned_mob_id);
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let cs = handle.combat_state.load();
                let tab_idx = ui.get_active_tab();
                let tab = MeterTab::from_index(tab_idx as usize);
                if let Some(mob_id) = resolve_view_mob_id(&cs, pinned_mob_id.get()) {
                    let max_rows = handle.config.lock().unwrap().meter_max_rows.max(1);
                    // Summary line doesn't show per-row breakdowns, so no
                    // need to compute any — empty set, same as "collapsed".
                    let snap = compute_snapshot(&cs, mob_id, tab, max_rows, &HashSet::new());
                    crate::tray::tray::copy_to_clipboard(&build_summary_line(&snap, tab));
                }
            }
        });
        window.on_mob_picker_clicked({
            let mob_picker_open = Rc::clone(&mob_picker_open);
            move || {
                mob_picker_open.set(!mob_picker_open.get());
            }
        });
        window.on_pick_mob({
            let pinned_mob_id = Rc::clone(&pinned_mob_id);
            let mob_picker_open = Rc::clone(&mob_picker_open);
            move |id| {
                pinned_mob_id.set((id >= 0).then_some(id as u64));
                mob_picker_open.set(false);
            }
        });
        window.on_row_clicked({
            let expanded_names = Rc::clone(&expanded_names);
            move |name| {
                let mut names = expanded_names.borrow_mut();
                let name = name.to_string();
                if !names.remove(&name) {
                    names.insert(name);
                }
            }
        });

        let mut state = MeterState::new(&handle);
        let weak = window.as_weak();
        let dragging_tick = Rc::clone(&dragging);
        let pinned_mob_id_tick = Rc::clone(&pinned_mob_id);
        let mob_picker_open_tick = Rc::clone(&mob_picker_open);
        let expanded_names_tick = Rc::clone(&expanded_names);
        let rows_model_tick = Rc::clone(&rows_model_handle);
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(TIMER_INTERVAL_MS),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                if state.handle.quit.load(Ordering::Relaxed) {
                    return;
                }
                // See `dragging`'s doc comment above `on_drag_requested` —
                // frozen here, before anything that could change row count
                // (and therefore this window's content-driven height).
                if dragging_tick.get() {
                    return;
                }
                state.tick_count += 1;
                if state.tick_count.is_multiple_of(25) {
                    tracing::info!(
                        "dps meter heartbeat: force_show={}, visible={}, handle={:p}",
                        state.force_show,
                        state.visible,
                        Arc::as_ptr(&state.handle)
                    );
                }
                state.sync_config();
                ui.set_locked(state.locked);
                ui.set_force_show(state.force_show);
                ui.set_win_width(state.width);
                ui.set_row_font_size(state.font_size);
                ui.set_share_bars(state.share_bars);
                ui.set_amount_w_px(state.amount_w);
                ui.set_rate_w_px(state.rate_w);
                ui.set_bar_w_px(state.bar_w);
                ui.set_sec_w_px(state.sec_w);
                // Locked means click-through for real on Linux (input
                // shape) — except while Show All Windows has the drag
                // TouchArea re-enabled, when the window must stay clickable.
                crate::overlay_draw::overlay_draw::sync_click_through(
                    ui.window(),
                    state.locked && !state.force_show,
                );

                let cs = state.handle.combat_state.load();
                // Drop a stale pin before resolving — a pinned mob that fell
                // off mob_list (e.g. an engine restart) should silently fall
                // back to auto-follow rather than leaving the meter stuck.
                if let Some(pid) = pinned_mob_id_tick.get() {
                    if !cs.mob_list.iter().any(|m| m.id == pid) {
                        pinned_mob_id_tick.set(None);
                    }
                }
                // Fall back to the last mob we were actually engaged with
                // when not currently engaged, rather than treating "not
                // engaged right now" as "no data" — the player is rarely in
                // combat every single tick (gaps between pulls, walking to
                // loot, etc.), and without this the meter blanked its rows
                // to zero and could vanish immediately on every such gap
                // instead of respecting `idle_secs` below.
                let view_mob_id =
                    resolve_view_mob_id(&cs, pinned_mob_id_tick.get()).or(state.last_active_mob_id);

                // Log-inactivity hide wins over everything else, including
                // Show All Windows — see `AppHandle::log_inactive`'s doc
                // comment.
                let show = !state.handle.log_inactive(state.hide_inactive_secs)
                    && (state.force_show || (state.meter_enabled && view_mob_id.is_some()));
                if !show {
                    if state.visible {
                        tracing::info!("dps meter: hiding");
                        let _ = ui.hide();
                        state.visible = false;
                        // See overlay.rs's matching comment on
                        // `set_alpha(0.0)` on hide — Slint's Property::set()
                        // no-ops when the new value equals the old one.
                        // `mob_label` alone only re-dirties its own Text
                        // element on the next show; `panel_opacity` re-dirties
                        // the whole `panel` container (title bar, column
                        // header, rows, footer), which is what actually
                        // needed to come back — see overlay_dps.slint's
                        // `panel-opacity` doc comment.
                        ui.set_mob_label("".into());
                        ui.set_panel_opacity(0.0);
                    }
                    mob_picker_open_tick.set(false);
                    return;
                }

                let Some(mob_id) = view_mob_id else {
                    // force_show with no combat data at all yet (this
                    // session has had no engagements) — a placeholder so
                    // there's something to see while positioning.
                    if !state.visible {
                        tracing::info!(
                            "dps meter: showing placeholder (force_show={})",
                            state.force_show
                        );
                        crate::overlay_draw::overlay_draw::apply_saved_position(
                            &ui,
                            &state.handle,
                            crate::overlay_registry::overlay_registry::OverlayKind::Meter,
                        );
                        let _ = ui.show();
                        state.visible = true;
                        crate::overlay_draw::overlay_draw::hide_from_taskbar(weak.clone());
                        crate::overlay_draw::overlay_draw::set_no_activate(weak.clone());
                        crate::overlay_draw::overlay_draw::force_repaint(weak.clone());
                    }
                    // See `overlay_draw::reassert_topmost`'s doc comment.
                    if state.tick_count.is_multiple_of(25) {
                        crate::overlay_draw::overlay_draw::reassert_topmost(ui.window());
                    }
                    ui.set_mob_label("DPS Meter (drag)".into());
                    rows_model_tick.set_vec(Vec::new());
                    ui.set_footer_total_label("0".into());
                    ui.set_footer_rate_label("0".into());
                    ui.set_elapsed_label("0s".into());
                    ui.set_panel_opacity(1.0);
                    return;
                };

                let tab = MeterTab::from_index(ui.get_active_tab() as usize);
                let snap = compute_snapshot(
                    &cs,
                    mob_id,
                    tab,
                    state.max_rows,
                    &expanded_names_tick.borrow(),
                );

                // Idle detection: hide if nothing has changed for idle_secs.
                if Some(mob_id) != state.last_active_mob_id
                    || snap.footer_total != state.last_footer_total
                {
                    state.last_change = Instant::now();
                    state.last_active_mob_id = Some(mob_id);
                    state.last_footer_total = snap.footer_total;
                }
                if !state.force_show
                    && state.idle_secs > 0
                    && state.last_change.elapsed() > Duration::from_secs(state.idle_secs as u64)
                {
                    if state.visible {
                        tracing::info!("dps meter: hiding (idle)");
                        let _ = ui.hide();
                        state.visible = false;
                        ui.set_panel_opacity(0.0);
                    }
                    return;
                }

                if !state.visible {
                    tracing::info!(
                        "dps meter: showing (real data, force_show={})",
                        state.force_show
                    );
                    crate::overlay_draw::overlay_draw::apply_saved_position(
                        &ui,
                        &state.handle,
                        crate::overlay_registry::overlay_registry::OverlayKind::Meter,
                    );
                    let _ = ui.show();
                    state.visible = true;
                    crate::overlay_draw::overlay_draw::hide_from_taskbar(weak.clone());
                    crate::overlay_draw::overlay_draw::set_no_activate(weak.clone());
                    crate::overlay_draw::overlay_draw::force_repaint(weak.clone());
                    ui.set_panel_opacity(1.0);
                }
                // See `overlay_draw::reassert_topmost`'s doc comment.
                if state.tick_count.is_multiple_of(25) {
                    crate::overlay_draw::overlay_draw::reassert_topmost(ui.window());
                }

                // Footer label: pin icon (if manually selected) + caret hint
                // that it opens the mob picker.
                let picker_open = mob_picker_open_tick.get();
                let caret = if picker_open { "\u{25B4}" } else { "\u{25BE}" };
                let pin_prefix = if pinned_mob_id_tick.get().is_some() {
                    "\u{1F4CC} "
                } else {
                    ""
                };
                ui.set_mob_label(format!("{pin_prefix}{} {caret}", snap.mob_name).into());
                sync_rows_model(&rows_model_tick, &snap.rows);
                ui.set_footer_total_label(fmt_k(snap.footer_total).into());
                ui.set_footer_rate_label(fmt_k(snap.footer_rate).into());
                ui.set_elapsed_label(format!("{}s", snap.elapsed_secs).into());
                ui.set_picker_open(picker_open);
                if picker_open {
                    let candidates = build_mob_picker_entries(&cs);
                    ui.set_picker_entries(mob_picker_entries_model(
                        &candidates,
                        pinned_mob_id_tick.get(),
                    ));
                }
            },
        );
        // Both must outlive this function — see overlay.rs's
        // create_alert_window for why the window itself needs forgetting
        // too, not just the timer (it's never `.show()`n here, so nothing
        // else keeps its Rc-based component alive).
        std::mem::forget(timer);
        std::mem::forget(window);
    }

    /// Overlays' index in SettingsWindow's `tab-names` array — the DPS
    /// meter's gear icon opens here since its settings live in the "DPS
    /// Meter Overlay" card on that tab, not on a tab of their own. See
    /// settings_window.rs / ui/settings_shell.slint.
    pub(crate) const TAB_OVERLAYS: i32 = 3;
}
