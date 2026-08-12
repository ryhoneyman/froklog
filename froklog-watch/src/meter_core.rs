//! DPS-meter scoping and aggregation, ported from the Windows client's
//! `overlay_dps.rs` (the pure-logic half; the GDI rendering half is replaced
//! by egui in `meter_ui.rs`). Kept behaviorally identical so both clients
//! agree about what the meter shows: same auto-follow rule, same per-mob
//! fight timer, same picker thresholds, same class palette, same
//! copy-to-chat summary format.

use std::collections::HashMap;

use froklog::state::{CombatState, EntityCombatStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterTab {
    Dps,
    Tank,
    Heal,
    HealReceived,
}

impl MeterTab {
    pub const ALL: [MeterTab; 4] = [Self::Dps, Self::Tank, Self::Heal, Self::HealReceived];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Dps => "DPS",
            Self::Tank => "Tank",
            Self::Heal => "Heal",
            Self::HealReceived => "Recv",
        }
    }

    pub fn amount_col_label(&self) -> &'static str {
        match self {
            Self::Dps | Self::Tank => "Dmg",
            Self::Heal | Self::HealReceived => "Heal",
        }
    }

    pub fn rate_col_label(&self) -> &'static str {
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

#[derive(Clone)]
pub struct RowData {
    pub name: String,
    pub color: (u8, u8, u8),
    /// One color per known class (1–3 from /who), for the share bar's
    /// gradient — same idea as the web viewer's multi-class row bars.
    /// Falls back to `[color]` when classes are unknown.
    pub colors: Vec<(u8, u8, u8)>,
    /// The owning player, when this row is a pet whose owner the parser
    /// has associated (warder possessive, Burnout correlation).
    pub owner: Option<String>,
    pub total: u64,
    pub rate: u64,
}

/// All attackers/healers for `mob_id` under `tab`, filtered but not yet
/// sorted or truncated — used both to build display rows and to sum a
/// cumulative footer total across *every* contributor, not just the ones
/// that fit on screen.
fn tab_entries(cs: &CombatState, mob_id: u64, tab: MeterTab) -> Vec<(&String, &EntityCombatStats)> {
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
/// when the current player isn't involved at all.
pub fn resolve_view_mob_id(cs: &CombatState, pinned: Option<u64>) -> Option<u64> {
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

pub const MAX_PICKER_ENTRIES: usize = 6;

/// One finished fight, frozen for the meter's local history.
///
/// All four tabs are captured at the moment the mob dies or times out, so
/// flipping Dmg/Heal/Tank while reviewing shows what each looked like then.
/// This lives in the meter (RAM, ring of `FIGHT_MEMORY`) and owes the server
/// nothing — reviewing your last pulls works with no stream at all.
#[derive(Clone)]
pub struct FightEntry {
    pub mob_name: String,
    pub ended: std::time::Instant,
    pub duration_secs: u64,
    /// Indexed by `MeterTab::ALL` order.
    pub tabs: [MeterSnapshot; 4],
}

/// How many finished fights the meter remembers.
pub const FIGHT_MEMORY: usize = 5;

/// Capture any confirmed mob that has just finished — died, or gone 15 s
/// without a combat line (the picker's own timeout) — into `mem`, newest
/// first, keeping `FIGHT_MEMORY`. `seen` prevents recapture while the mob
/// lingers on the list. Fights with no damage at all (a /who sighting, a
/// parked mez) are not history worth keeping.
pub fn capture_finished_fights(
    cs: &CombatState,
    mem: &mut Vec<FightEntry>,
    seen: &mut std::collections::HashSet<(u64, u32)>,
) {
    for m in cs.mob_list.iter() {
        if !cs.confirmed_mobs.contains(&m.name) || m.parked {
            continue;
        }
        let key = (m.id, m.first_log_ts);
        if seen.contains(&key) {
            continue;
        }
        let done = cs.dead_mobs.contains(&m.name) || m.last_seen.elapsed().as_secs_f64() >= 15.0;
        if !done {
            continue;
        }
        let tabs = [
            compute_snapshot(cs, m.id, MeterTab::ALL[0], usize::MAX),
            compute_snapshot(cs, m.id, MeterTab::ALL[1], usize::MAX),
            compute_snapshot(cs, m.id, MeterTab::ALL[2], usize::MAX),
            compute_snapshot(cs, m.id, MeterTab::ALL[3], usize::MAX),
        ];
        if tabs[0].footer_total == 0 {
            continue;
        }
        seen.insert(key);
        mem.insert(
            0,
            FightEntry {
                mob_name: m.name.clone(),
                ended: std::time::Instant::now(),
                duration_secs: tabs[0].elapsed_secs,
                tabs,
            },
        );
        mem.truncate(FIGHT_MEMORY);
    }
    // A reset (or prune) empties the mob list; drop stale keys so ids reused
    // later, paired with fresh first_log_ts values, stay collision-free and
    // the set cannot grow without bound.
    if cs.mob_list.is_empty() {
        seen.clear();
    }
}

pub struct MobPickerEntry {
    /// `None` = the "Auto (most recent)" entry, clearing any pin.
    pub id: Option<u64>,
    pub label: String,
    pub dot: (u8, u8, u8),
}

/// Build the mob picker's row list: "Auto" first, then up to
/// `MAX_PICKER_ENTRIES` confirmed mobs sorted most-recently-seen first,
/// each with an activity dot (green = active <5s, amber = idle <15s,
/// grey = timed out/dead) — same thresholds `to_api_json()` uses for the
/// web UI's mob list indicator (state.rs).
pub fn build_mob_picker_entries(cs: &CombatState) -> Vec<MobPickerEntry> {
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
            let classes = cs.player_classes.get(name);
            let code = classes
                .and_then(|c| c.first())
                .map(|s| s.as_str())
                .unwrap_or("");
            let colors: Vec<(u8, u8, u8)> = classes
                .map(|c| c.iter().take(3).map(|k| class_color(k)).collect())
                .filter(|v: &Vec<_>| !v.is_empty())
                .unwrap_or_else(|| vec![class_color(code)]);
            RowData {
                name: name.clone(),
                color: class_color(code),
                colors,
                owner: cs.known_pets.get(name).cloned(),
                total,
                rate,
            }
        })
        .collect()
}

/// Everything a single render (or a copy-to-clipboard summary) needs for
/// one mob/tab combination — computed once per use so the render pass and
/// the copy-icon click handler can never disagree about what's "currently
/// on screen."
#[derive(Clone)]
pub struct MeterSnapshot {
    pub mob_name: String,
    pub rows: Vec<RowData>,
    pub footer_total: u64,
    pub footer_rate: u64,
    pub elapsed_secs: u64,
}

pub fn compute_snapshot(
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
pub fn class_color(code: &str) -> (u8, u8, u8) {
    let hex: u32 = match code {
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
pub fn fmt_k(n: u64) -> String {
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
pub fn fmt_duration(secs: u64) -> String {
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
pub fn build_summary_line(snap: &MeterSnapshot, tab: MeterTab) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_k_values() {
        assert_eq!(fmt_k(999), "999");
        assert_eq!(fmt_k(1000), "1K");
        assert_eq!(fmt_k(1500), "1.5K");
        assert_eq!(fmt_k(1_000_000), "1M");
        assert_eq!(fmt_k(2_400_000), "2.4M");
    }

    #[test]
    fn fmt_duration_values() {
        assert_eq!(fmt_duration(27), "27s");
        assert_eq!(fmt_duration(185), "3:05");
    }

    #[test]
    fn class_color_known_and_unknown() {
        assert_eq!(class_color("WAR"), (0xaf, 0x80, 0x3c));
        assert_eq!(class_color(""), (0x46, 0x46, 0x50));
        assert_eq!(class_color("XYZ"), (0x46, 0x46, 0x50));
    }

    #[test]
    fn summary_line_shape() {
        let snap = MeterSnapshot {
            mob_name: "an orc taskmaster".into(),
            rows: vec![
                RowData {
                    name: "Zari".into(),
                    color: (0, 0, 0),
                    colors: vec![(0, 0, 0)],
                    owner: None,
                    total: 7000,
                    rate: 350,
                },
                RowData {
                    name: "Zary".into(),
                    color: (0, 0, 0),
                    colors: vec![(0, 0, 0)],
                    owner: None,
                    total: 3000,
                    rate: 150,
                },
            ],
            footer_total: 10000,
            footer_rate: 500,
            elapsed_secs: 20,
        };
        let line = build_summary_line(&snap, MeterTab::Dps);
        assert_eq!(
            line,
            "an orc taskmaster: 10K dmg (500 dps) over 20s | 1. Zari 7K (70%), 2. Zary 3K (30%)"
        );
    }
}

#[cfg(test)]
mod live_probe {
    use super::*;
    use froklog::state::CombatState;
    use std::sync::{atomic::AtomicBool, Arc};

    /// Manual diagnostic, never run by default: replay the tail of a real
    /// log through the real parser and report what each meter tab would
    /// show. Answers "is the tab empty because the button is broken or
    /// because there is no healing in this log?" — which is exactly the
    /// question a dead-looking Heal tab raises.
    ///
    ///   PROBE_LOG=/path/to/eqlog.txt cargo test probe_tabs -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_tabs_against_real_log() {
        let path = std::env::var("PROBE_LOG").expect("PROBE_LOG");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let slice: Vec<String> = lines[lines.len().saturating_sub(4000)..]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let (ltx, lrx) = crossbeam_channel::unbounded();
        for l in &slice {
            ltx.send(l.clone()).unwrap();
        }
        drop(ltx);
        let shared = Arc::new(arc_swap::ArcSwap::from_pointee(CombatState::default()));
        let reset = Arc::new(AtomicBool::new(false));
        let (btx, _brx) = tokio::sync::broadcast::channel(16);
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || while erx.blocking_recv().is_some() {});
        froklog::parser::run(lrx, Arc::clone(&shared), reset, btx, etx, "Izzin".into());

        let cs = shared.load();
        println!("mob_list: {}", cs.mob_list.len());
        println!(
            "maps: damage={} tanking={} healing={} healed={}",
            cs.mob_damage.len(),
            cs.mob_tanking.len(),
            cs.mob_healing.len(),
            cs.mob_healed.len()
        );
        for m in cs.mob_list.iter().take(4) {
            println!("--- mob {} '{}'", m.id, m.name);
            for tab in MeterTab::ALL {
                let snap = compute_snapshot(&cs, m.id, tab, 10);
                println!(
                    "   {:>5}: {} rows, total={}",
                    tab.label(),
                    snap.rows.len(),
                    snap.footer_total
                );
            }
        }
    }
}

#[cfg(test)]
mod fight_memory_tests {
    use super::*;
    use froklog::state::CombatState;
    use std::sync::{atomic::AtomicBool, Arc};

    fn state_from(lines: &[&str]) -> Arc<CombatState> {
        let (ltx, lrx) = crossbeam_channel::unbounded();
        for l in lines {
            ltx.send(l.to_string()).unwrap();
        }
        drop(ltx);
        let shared = Arc::new(arc_swap::ArcSwap::from_pointee(CombatState::default()));
        let reset = Arc::new(AtomicBool::new(false));
        let (btx, _brx) = tokio::sync::broadcast::channel(16);
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || while erx.blocking_recv().is_some() {});
        froklog::parser::run(lrx, Arc::clone(&shared), reset, btx, etx, "Izzin".into());
        shared.load_full()
    }

    /// A killed mob is remembered exactly once, with its damage frozen — the
    /// meter's own history, no server involved.
    #[test]
    fn a_dead_mob_is_captured_once_with_its_numbers() {
        let cs = state_from(&[
            "[Tue Feb 27 22:00:07 2026] You slash a rat for 25 points of damage.",
            "[Tue Feb 27 22:00:09 2026] You slash a rat for 30 points of damage.",
            "[Tue Feb 27 22:00:10 2026] You have slain a rat!",
        ]);
        let mut mem = Vec::new();
        let mut seen = std::collections::HashSet::new();
        capture_finished_fights(&cs, &mut mem, &mut seen);
        assert_eq!(mem.len(), 1, "one finished fight");
        assert_eq!(mem[0].mob_name, "a rat");
        assert_eq!(mem[0].tabs[0].footer_total, 55, "damage frozen at death");

        capture_finished_fights(&cs, &mut mem, &mut seen);
        assert_eq!(mem.len(), 1, "same fight is never captured twice");
    }

    /// Only the last FIGHT_MEMORY fights are kept, newest first.
    #[test]
    fn the_ring_keeps_the_newest_five() {
        let mut lines = Vec::new();
        for i in 0..7 {
            lines.push(format!(
                "[Tue Feb 27 22:0{i}:07 2026] You slash a gnoll for {} points of damage.",
                10 + i
            ));
            lines.push(format!(
                "[Tue Feb 27 22:0{i}:09 2026] You have slain a gnoll!"
            ));
        }
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let cs = state_from(&refs);
        let mut mem = Vec::new();
        let mut seen = std::collections::HashSet::new();
        capture_finished_fights(&cs, &mut mem, &mut seen);
        assert!(
            mem.len() <= FIGHT_MEMORY,
            "capped at {FIGHT_MEMORY}: {}",
            mem.len()
        );
        assert!(!mem.is_empty());
    }
}
