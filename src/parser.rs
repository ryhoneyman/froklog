use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::{Datelike, Local, Timelike};
use crossbeam_channel::Receiver;
use tokio::sync::{broadcast, mpsc};
use tracing::trace;

use crate::event::{CombatEvent, MODS_CRIT, MODS_TWINCAST, MODS_LUCKY, MODS_RAMPAGE,
    MODS_STRIKETHROUGH, MODS_RIPOSTE_MOD, MODS_ASSASSINATE, MODS_HEADSHOT,
    MODS_SLAY_UNDEAD, MODS_DOUBLEBOW, MODS_FLURRY};
use crate::patterns::{
    TS_LEN, norm, normalize_article_case, normalize_verb, normalize_miss,
    RE_HIT_BY_SPELL, RE_MELEE, RE_SPELL_ATTR, RE_SPELL_HIT, RE_DOT,
    RE_RIPOSTE, RE_DS, RE_DS_PROC, RE_CAST, RE_HEAL, RE_HAS_TAKEN, RE_EXTRA_DMG,
    RE_SLAY_HAS, RE_SLAY_YOU, RE_SLAIN_BY, RE_DIED,
    RE_MISS, RE_ABSORB_SKIN, RE_ABSORB_RUNE, RE_RESIST, RE_WHO, parse_who_classes,
};
use crate::state::{CombatState, EntityCombatStats, MobSighting};

// ── Entry point ───────────────────────────────────────────────────────────────

/// Parse hit modifier flags from a parenthetical suffix like `(Lucky Critical Twincast)`.
fn parse_mods(suffix: &str) -> u16 {
    let text = suffix.trim_matches(|c| c == '(' || c == ')').trim();
    let mut mods = 0u16;
    if text.contains("Critical") || text.contains("Deadly Strike")
        || text.contains("Crippling Blow") || text.contains("Finishing Blow") {
        mods |= MODS_CRIT;
    }
    if text.contains("Twincast")      { mods |= MODS_TWINCAST; }
    if text.contains("Lucky")         { mods |= MODS_LUCKY; }
    if text.contains("Rampage")       { mods |= MODS_RAMPAGE; }
    if text.contains("Strikethrough") { mods |= MODS_STRIKETHROUGH; }
    if text.contains("Riposte")       { mods |= MODS_RIPOSTE_MOD; }
    if text.contains("Assassinate")   { mods |= MODS_ASSASSINATE; }
    if text.contains("Headshot")      { mods |= MODS_HEADSHOT; }
    if text.contains("Slay Undead")   { mods |= MODS_SLAY_UNDEAD; }
    if text.contains("Double Bow Shot") { mods |= MODS_DOUBLEBOW; }
    if text.contains("Flurry")        { mods |= MODS_FLURRY; }
    mods
}

/// Strip a trailing `(...)` modifier block from a log body line.
/// Returns `(line_without_suffix, mods_bitmask)`.
/// Only strips when the line ends with `)` and a matching `(` is found.
fn strip_mods(line: &str) -> (&str, u16) {
    let trimmed = line.trim_end();
    if trimmed.ends_with(')') {
        if let Some(pos) = trimmed.rfind('(') {
            let suffix = &trimmed[pos..];
            let rest = trimmed[..pos].trim_end();
            return (rest, parse_mods(suffix));
        }
    }
    (trimmed, 0)
}


pub fn run(
    rx: Receiver<String>,
    shared: Arc<ArcSwap<CombatState>>,
    reset_flag: Arc<AtomicBool>,
    broadcast_tx: broadcast::Sender<Arc<CombatState>>,
    event_tx: mpsc::UnboundedSender<CombatEvent>,
    player_name: String,
) {
    let mut state = CombatState { player_name: player_name.clone(), ..Default::default() };

    let mut spell_caster: HashMap<String, String> = HashMap::new();
    let mut mob_candidates: HashMap<String, HashSet<String>> = HashMap::new();

    let mut last_publish = Instant::now();
    // Unix timestamp (seconds) of the most recently parsed EQ log line.
    let mut current_ts: u32 = 0;

    for raw_line in &rx {
        if reset_flag.swap(false, Ordering::Relaxed) {
            let lines = state.lines_parsed;
            let player = state.player_name.clone();
            let player_classes = std::mem::take(&mut state.player_classes);
            state = CombatState { lines_parsed: lines, player_name: player, player_classes, ..Default::default() };
            spell_caster.clear();
            mob_candidates.clear();
            publish(&shared, &broadcast_tx, &state);
            last_publish = Instant::now();
        }

        state.lines_parsed += 1;

        let line = if raw_line.len() > TS_LEN && raw_line.starts_with('[') {
            if let Some(dt) = crate::tailer::parse_eq_timestamp(&raw_line) {
                state.last_log_time = format!("{} {:02}:{:02}", dt.format("%b %-d"), dt.hour(), dt.minute());
                // Treat the naive log datetime as-is (no timezone conversion).
                // The log records local streamer time with no offset, so we
                // store it as a "fake UTC" unix timestamp and display it as
                // UTC on the frontend, preserving the original clock value.
                current_ts = dt.and_utc().timestamp() as u32;
            }
            &raw_line[TS_LEN..]
        } else {
            raw_line.as_str()
        };

        trace!(line, "parsing");

        // Strip trailing `(Lucky Critical Twincast)` modifier blocks before matching.
        let (line, mods) = strip_mods(line);

        let mut matched = false;

        // ── Attributed DD spell hit ("X hit Y for N of TYPE dmg by Spell") ───────
        // Intercept before RE_MELEE so "hit" isn't misread as a mob melee verb.
        if let Some(caps) = RE_HIT_BY_SPELL.captures(line) {
            let src = norm(caps["src"].trim(), &player_name);
            let tgt = norm(caps["tgt"].trim(), &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);
            let spell = caps["spell"].to_owned();

            // Determine direction: mob→player or player→mob.
            // src with a space is always a mob (article prefix). If tgt is a known
            // player (in known_players — damage dealers only, not mob healers), the
            // src must be a mob even if single-word.
            let src_is_mob = src.contains(' ')
                || state.confirmed_mobs.contains(&src)
                || state.known_players.contains(tgt.as_str())
                || tgt == player_name
                || mob_candidates.contains_key(src.as_str());

            if src_is_mob {
                // Mob spell hitting a player — record as tanking damage.
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src.clone());
                }
                let mob_id = update_mob_list(&mut state, &src, current_ts);
                let tank = state.mob_tanking.entry(mob_id).or_default()
                    .entry(tgt.clone()).or_default();
                tank.total_damage += dmg;
                *tank.damage_by_type.entry("hit".to_owned()).or_default() += dmg;
                emit(&event_tx, CombatEvent::Spell {
                    ts: current_ts, mob: mob_id as u32,
                    src, tgt, dmg: dmg as u32, sp: spell, tank: true, mods,
                });
            } else {
                // Player spell hitting a mob — record as player damage.
                state.known_players.insert(src.clone());
                let stats = entity_stats(&mut state, &src);
                stats.total_damage += dmg;
                *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                if mods & MODS_CRIT != 0      { stats.crit_count += 1; }
                if mods & MODS_TWINCAST != 0  { stats.twincast_count += 1; }

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                let mob_p = state.mob_damage.entry(mob_id).or_default()
                    .entry(src.clone()).or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                emit(&event_tx, CombatEvent::Spell {
                    ts: current_ts, mob: mob_id as u32,
                    src, tgt, dmg: dmg as u32, sp: spell, tank: false, mods,
                });
            }
            touch_fight_start(&mut state);
            matched = true;

        // ── Melee ──────────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_MELEE.captures(line) {
            let src = norm(caps["src"].trim(), &player_name);
            let verb = &caps["verb"];
            let tgt = norm(&caps["tgt"], &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);
            let typ = normalize_verb(verb).to_owned();

            // "hits"/"hit" is an exclusively mob verb in EQ.
            // Also treat as mob attack if src has been targeted before (in mob_candidates),
            // if src name contains a space (multi-word = always a mob), if src was already
            // confirmed as a mob via another combat path, or if tgt is a known player
            // (a known player being hit means the attacker must be a mob).
            // However, if src is a confirmed player (in known_players), always treat as player
            // regardless of verb — some procs/items produce a bare "hit" line from players.
            let src_is_mob = !state.known_players.contains(&src)
                && (verb == "hit" || verb == "hits"
                    || src.contains(' ')
                    || state.confirmed_mobs.contains(&src)
                    || mob_candidates.contains_key(src.as_str())
                    || state.known_players.contains(tgt.as_str())
                    || tgt == player_name);

            if src_is_mob {
                // Tanking: mob (src) damages player (tgt)
                let mob_id = update_mob_list(&mut state, &src, current_ts);
                let tank = state.mob_tanking.entry(mob_id).or_default()
                    .entry(tgt.clone()).or_default();
                tank.total_damage += dmg;
                *tank.damage_by_type.entry(typ.clone()).or_default() += dmg;
                emit(&event_tx, CombatEvent::Melee {
                    ts: current_ts, mob: mob_id as u32,
                    src: src.clone(), tgt, dmg: dmg as u32, typ, tank: true, mods,
                });
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src);
                }
            } else {
                // Damage: player (src) attacks mob (tgt)
                state.known_players.insert(src.clone());
                let stats = entity_stats(&mut state, &src);
                stats.total_damage += dmg;
                *stats.damage_by_type.entry(typ.clone()).or_default() += dmg;
                if mods & MODS_CRIT != 0      { stats.crit_count += 1; }
                if mods & MODS_TWINCAST != 0  { stats.twincast_count += 1; }

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                let mob_p = state.mob_damage.entry(mob_id).or_default()
                    .entry(src.clone()).or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_type.entry(typ.clone()).or_default() += dmg;
                emit(&event_tx, CombatEvent::Melee {
                    ts: current_ts, mob: mob_id as u32,
                    src, tgt, dmg: dmg as u32, typ, tank: false, mods,
                });
            }
            touch_fight_start(&mut state);
            matched = true;

        // ── Attributed spell / proc ("Player's Spell hit Mob for X") ──────────
        } else if let Some(caps) = RE_SPELL_ATTR.captures(line) {
            let raw_src = norm(&caps["src"], &player_name);
            let raw_spell = caps["spell"].to_owned();
            let tgt = norm(&caps["tgt"], &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            // "Denon's Disruptive Discord hit Mob for X" — the regex splits this into
            // src="Denon", spell="Disruptive Discord", but the full possessive is itself
            // the spell name.  Only accept the split attribution when:
            //   (a) spell_caster has the combined form mapped to a real caster, or
            //   (b) raw_src is already a confirmed player.
            // Otherwise we'd create a phantom "Denon" player from a spell name prefix.
            let combined = format!("{}'s {}", raw_src, raw_spell);
            let real_caster_opt = spell_caster.get(&combined).cloned();
            let is_attributed = real_caster_opt.is_some()
                || state.known_players.contains(&raw_src);

            if is_attributed {
                let (src, spell) = if let Some(real_caster) = real_caster_opt {
                    (real_caster, combined)
                } else {
                    (raw_src, raw_spell)
                };

                state.known_players.insert(src.clone());
                let stats = entity_stats(&mut state, &src);
                stats.total_damage += dmg;
                *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                if mods & MODS_CRIT != 0      { stats.crit_count += 1; }
                if mods & MODS_TWINCAST != 0  { stats.twincast_count += 1; }

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                let mob_p = state.mob_damage.entry(mob_id).or_default()
                    .entry(src.clone()).or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                emit(&event_tx, CombatEvent::Spell {
                    ts: current_ts, mob: mob_id as u32,
                    src, tgt, dmg: dmg as u32, sp: spell, tank: false, mods,
                });
            }

            touch_fight_start(&mut state);
            matched = true;

        // ── Unattributed spell hit ("SpellName hit Mob for X") ────────────────
        } else if let Some(caps) = RE_SPELL_HIT.captures(line) {
            let spell = caps["spell"].to_owned();
            let tgt = norm(&caps["tgt"], &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            if let Some(caster) = spell_caster.get(&spell).cloned() {
                // If the caster is mob-like (multi-word name or already a confirmed mob)
                // the target is a player being hit by a mob spell — don't track as mob.
                let caster_is_mob = caster.contains(' ')
                    || state.confirmed_mobs.contains(&caster);

                if caster_is_mob {
                    // Confirm the mob caster; ignore the player target for mob tracking.
                    if !state.known_players.contains(&caster) {
                        state.confirmed_mobs.insert(caster);
                    }
                } else {
                    state.known_players.insert(caster.clone());
                    let stats = entity_stats(&mut state, &caster);
                    stats.total_damage += dmg;
                    *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    if mods & MODS_CRIT != 0      { stats.crit_count += 1; }
                    if mods & MODS_TWINCAST != 0  { stats.twincast_count += 1; }

                    track_mob_candidate(&mut mob_candidates, tgt.clone(), &caster);
                    let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                    let mob_p = state.mob_damage.entry(mob_id).or_default()
                        .entry(caster.clone()).or_default();
                    mob_p.total_damage += dmg;
                    *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    emit(&event_tx, CombatEvent::Spell {
                        ts: current_ts, mob: mob_id as u32,
                        src: caster, tgt, dmg: dmg as u32, sp: spell, tank: false, mods,
                    });
                }
                touch_fight_start(&mut state);
                matched = true;
            }

        // ── DoT tick ("Mob has been damaged by Player's Spell for X") ─────────
        } else if let Some(caps) = RE_DOT.captures(line) {
            let src = norm(&caps["src"], &player_name);
            let spell = caps["spell"].to_owned();
            let tgt = norm(&caps["tgt"], &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            state.known_players.insert(src.clone());
            let stats = entity_stats(&mut state, &src);
            stats.total_damage += dmg;
            *stats.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
            *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            if mods & MODS_CRIT != 0      { stats.crit_count += 1; }
            if mods & MODS_TWINCAST != 0  { stats.twincast_count += 1; }

            track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
            let mob_id = update_mob_list(&mut state, &tgt, current_ts);
            let mob_p = state.mob_damage.entry(mob_id).or_default()
                .entry(src.clone()).or_default();
            mob_p.total_damage += dmg;
            *mob_p.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
            *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            emit(&event_tx, CombatEvent::Dot {
                ts: current_ts, mob: mob_id as u32,
                src, tgt, dmg: dmg as u32, sp: spell, mods,
            });

            touch_fight_start(&mut state);
            matched = true;

        // ── Riposte ────────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_RIPOSTE.captures(line) {
            let src = norm(&caps["src"], &player_name);
            let tgt = norm(&caps["tgt"], &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            // A mob can riposte a player: "Player was injured by Mob's riposte for N"
            let src_is_mob = src.contains(' ')
                || state.confirmed_mobs.contains(&src)
                || state.known_players.contains(tgt.as_str())
                || tgt == player_name;

            if src_is_mob {
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src.clone());
                }
                let mob_id = update_mob_list(&mut state, &src, current_ts);
                let tank = state.mob_tanking.entry(mob_id).or_default()
                    .entry(tgt.clone()).or_default();
                tank.total_damage += dmg;
                *tank.damage_by_type.entry("riposte".to_owned()).or_default() += dmg;
                emit(&event_tx, CombatEvent::Spell {
                    ts: current_ts, mob: mob_id as u32,
                    src, tgt, dmg: dmg as u32, sp: "riposte".to_owned(), tank: true, mods,
                });
            } else {
                state.known_players.insert(src.clone());
                let stats = entity_stats(&mut state, &src);
                stats.total_damage += dmg;
                *stats.damage_by_type.entry("riposte".to_owned()).or_default() += dmg;
                if mods & MODS_CRIT != 0 { stats.crit_count += 1; }

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                let mob_p = state.mob_damage.entry(mob_id).or_default()
                    .entry(src.clone()).or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_type.entry("riposte".to_owned()).or_default() += dmg;
                emit(&event_tx, CombatEvent::Rip {
                    ts: current_ts, mob: mob_id as u32,
                    src, tgt, dmg: dmg as u32, mods,
                });
            }

            touch_fight_start(&mut state);
            matched = true;

        // ── Damage shield ──────────────────────────────────────────────────────
        } else if let Some(caps) = RE_DS.captures(line) {
            let src = norm(&caps["src"], &player_name);
            let tgt = norm(&caps["tgt"], &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            // A mob can have a damage shield: "Player was struck by Mob's damage shield for N"
            let src_is_mob = src.contains(' ')
                || state.confirmed_mobs.contains(&src)
                || state.known_players.contains(tgt.as_str())
                || tgt == player_name;

            if src_is_mob {
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src.clone());
                }
                let mob_id = update_mob_list(&mut state, &src, current_ts);
                let tank = state.mob_tanking.entry(mob_id).or_default()
                    .entry(tgt.clone()).or_default();
                tank.total_damage += dmg;
                *tank.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
                emit(&event_tx, CombatEvent::Spell {
                    ts: current_ts, mob: mob_id as u32,
                    src, tgt, dmg: dmg as u32, sp: "ds".to_owned(), tank: true, mods: 0,
                });
            } else {
                state.known_players.insert(src.clone());
                let stats = entity_stats(&mut state, &src);
                stats.total_damage += dmg;
                *stats.damage_by_type.entry("ds".to_owned()).or_default() += dmg;

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                let mob_p = state.mob_damage.entry(mob_id).or_default()
                    .entry(src.clone()).or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
                emit(&event_tx, CombatEvent::Ds {
                    ts: current_ts, mob: mob_id as u32,
                    src, tgt, dmg: dmg as u32,
                });
            }

            touch_fight_start(&mut state);
            matched = true;

        // ── Outbound DS proc ("Mob is burned by YOUR/Player's flames for N…") ──
        } else if let Some(caps) = RE_DS_PROC.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &player_name);
            let src = if let Some(m) = caps.name("src") {
                norm(m.as_str(), &player_name)
            } else {
                player_name.clone() // YOUR
            };
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            state.known_players.insert(src.clone());
            let stats = entity_stats(&mut state, &src);
            stats.total_damage += dmg;
            *stats.damage_by_type.entry("ds".to_owned()).or_default() += dmg;

            track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
            let mob_id = update_mob_list(&mut state, &tgt, current_ts);
            let mob_p = state.mob_damage.entry(mob_id).or_default()
                .entry(src.clone()).or_default();
            mob_p.total_damage += dmg;
            *mob_p.damage_by_type.entry("ds".to_owned()).or_default() += dmg;

            emit(&event_tx, CombatEvent::Ds {
                ts: current_ts, mob: mob_id as u32,
                src, tgt, dmg: dmg as u32,
            });

            touch_fight_start(&mut state);
            matched = true;

        // ── Spell cast — record caster for attribution ─────────────────────────
        } else if let Some(caps) = RE_CAST.captures(line) {
            let src = norm(&caps["src"], &player_name);
            let spell = caps["spell"].to_owned();
            spell_caster.insert(spell.clone(), src.clone());
            state.active_casts.insert(src.clone(), (spell.clone(), Instant::now()));
            emit(&event_tx, CombatEvent::Cast { ts: current_ts, src, sp: spell });

        // ── Kill messages ──────────────────────────────────────────────────────

        // "You have slain Y!"
        } else if let Some(caps) = RE_SLAY_YOU.captures(line) {
            let tgt = normalize_article_case(&caps["tgt"]);
            let killer = player_name.clone();
            handle_slay(&mut state, &event_tx, tgt, killer, current_ts);

        // "X has slain Y!"
        } else if let Some(caps) = RE_SLAY_HAS.captures(line) {
            let tgt = normalize_article_case(&caps["tgt"]);
            let killer = norm(caps["killer"].trim(), &player_name);
            handle_slay(&mut state, &event_tx, tgt, killer, current_ts);

        // "Y was slain by X!" / "Y has been slain by X!"
        } else if let Some(caps) = RE_SLAIN_BY.captures(line) {
            let tgt = normalize_article_case(&caps["tgt"]);
            let killer = normalize_article_case(caps["killer"].trim());
            handle_slay(&mut state, &event_tx, tgt, killer, current_ts);

        // "X died."  (no explicit killer)
        } else if let Some(caps) = RE_DIED.captures(line) {
            let tgt = normalize_article_case(&caps["tgt"]);
            handle_slay(&mut state, &event_tx, tgt, String::new(), current_ts);

        // ── "X has taken N damage from [Player's/your] Spell [by Player]." ────
        } else if let Some(caps) = RE_HAS_TAKEN.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);
            let spell = caps["spell"].trim().trim_end_matches('.').to_owned();

            // Resolve attacker and full spell name.
            // "by X" is the explicit caster and takes priority over the possessive
            // prefix — e.g. "Denon's Disruptive Discord by Rysk" has src="Denon" and
            // by_src="Rysk" from the regex, but Rysk is the real caster and the full
            // spell name is "Denon's Disruptive Discord".
            let (attacker, spell) = if caps.name("your").is_some() {
                (Some(player_name.clone()), spell)
            } else if let Some(m) = caps.name("by_src") {
                let s = m.as_str().trim().trim_end_matches('.');
                let real_src = if s.is_empty() { None } else { Some(norm(s, &player_name)) };
                let full_spell = if let Some(pfx) = caps.name("src") {
                    format!("{}'s {}", pfx.as_str(), spell)
                } else {
                    spell
                };
                (real_src, full_spell)
            } else if let Some(m) = caps.name("src") {
                (Some(norm(m.as_str(), &player_name)), spell)
            } else {
                (None, spell)
            };

            if let Some(src) = attacker {
                let src_is_mob = src.contains(' ')
                    || state.confirmed_mobs.contains(&src)
                    || state.known_players.contains(tgt.as_str())
                    || tgt == player_name;
                if src_is_mob && src != player_name {
                    // Mob DoT/spell hitting a player — tanking
                    if !state.known_players.contains(&src) {
                        state.confirmed_mobs.insert(src.clone());
                    }
                    let mob_id = update_mob_list(&mut state, &src, current_ts);
                    let tank = state.mob_tanking.entry(mob_id).or_default()
                        .entry(tgt.clone()).or_default();
                    tank.total_damage += dmg;
                    *tank.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                    emit(&event_tx, CombatEvent::Spell {
                        ts: current_ts, mob: mob_id as u32,
                        src, tgt, dmg: dmg as u32, sp: spell, tank: true, mods,
                    });
                } else {
                    // Player DoT hitting a mob
                    state.known_players.insert(src.clone());
                    let stats = entity_stats(&mut state, &src);
                    stats.total_damage += dmg;
                    *stats.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                    *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    if mods & MODS_CRIT != 0     { stats.crit_count += 1; }
                    if mods & MODS_TWINCAST != 0 { stats.twincast_count += 1; }

                    track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                    let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                    let mob_p = state.mob_damage.entry(mob_id).or_default()
                        .entry(src.clone()).or_default();
                    mob_p.total_damage += dmg;
                    *mob_p.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                    *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    emit(&event_tx, CombatEvent::Dot {
                        ts: current_ts, mob: mob_id as u32,
                        src, tgt, dmg: dmg as u32, sp: spell, mods,
                    });
                }
                touch_fight_start(&mut state);
                matched = true;
            }

        // ── Bane/extra damage ("X has taken an extra N points of non-melee…") ─
        } else if let Some(caps) = RE_EXTRA_DMG.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);
            let spell = caps["spell"].trim().to_owned();
            let src = if caps.name("your").is_some() {
                player_name.clone()
            } else if let Some(m) = caps.name("src") {
                norm(m.as_str(), &player_name)
            } else {
                player_name.clone()
            };

            state.known_players.insert(src.clone());
            let stats = entity_stats(&mut state, &src);
            stats.total_damage += dmg;
            *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            if mods & MODS_CRIT != 0     { stats.crit_count += 1; }
            if mods & MODS_TWINCAST != 0 { stats.twincast_count += 1; }

            track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
            let mob_id = update_mob_list(&mut state, &tgt, current_ts);
            let mob_p = state.mob_damage.entry(mob_id).or_default()
                .entry(src.clone()).or_default();
            mob_p.total_damage += dmg;
            *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            emit(&event_tx, CombatEvent::Spell {
                ts: current_ts, mob: mob_id as u32,
                src, tgt, dmg: dmg as u32, sp: spell, tank: false, mods,
            });
            touch_fight_start(&mut state);
            matched = true;

        // ── Miss / avoidance ("X tries to Y Z, but Z dodges!") ───────────────
        } else if let Some(caps) = RE_MISS.captures(line) {
            let src = norm(caps["src"].trim(), &player_name);
            let tgt = norm(caps["tgt"].trim(), &player_name);
            let miss_type = normalize_miss(&caps["miss"]).to_owned();

            // Track avoidance on the defender (tgt).
            let def_stats = entity_stats(&mut state, &tgt);
            *def_stats.avoidance_by_type.entry(miss_type.clone()).or_default() += 1;

            // If src is a mob, also record on mob_tanking avoidance.
            let src_is_mob = src.contains(' ')
                || state.confirmed_mobs.contains(&src)
                || state.known_players.contains(tgt.as_str())
                || tgt == player_name;
            let mob_id = if src_is_mob {
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src.clone());
                }
                let id = update_mob_list(&mut state, &src, current_ts);
                let tank = state.mob_tanking.entry(id).or_default()
                    .entry(tgt.clone()).or_default();
                *tank.avoidance_by_type.entry(miss_type.clone()).or_default() += 1;
                id
            } else {
                state.active_mob_id.unwrap_or(0)
            };

            emit(&event_tx, CombatEvent::Miss {
                ts: current_ts, mob: mob_id as u32, src, tgt, typ: miss_type,
            });

        // ── Absorb: magical skin ("X's magical skin absorbs the damage…") ─────
        } else if let Some(caps) = RE_ABSORB_SKIN.captures(line) {
            let tgt = if let Some(m) = caps.name("tgt") {
                norm(m.as_str(), &player_name)
            } else {
                player_name.clone() // YOUR magical skin
            };
            let src = norm(caps["src"].trim(), &player_name);
            let mob_id = state.active_mob_id.unwrap_or(0);
            emit(&event_tx, CombatEvent::Absorb {
                ts: current_ts, mob: mob_id as u32, tgt, src,
            });

        // ── Absorb: rune shield ("X has shielded itself from N points…") ──────
        } else if let Some(caps) = RE_ABSORB_RUNE.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &player_name);
            let mob_id = state.active_mob_id.unwrap_or(0);
            emit(&event_tx, CombatEvent::Absorb {
                ts: current_ts, mob: mob_id as u32, tgt, src: String::new(),
            });

        // ── Spell resist ("NPC resisted your Spell!" / "NPC resisted X's Spell!")
        } else if let Some(caps) = RE_RESIST.captures(line) {
            let tgt = normalize_article_case(&caps["tgt"]);
            let spell = caps["spell"].trim().to_owned();
            let src = if let Some(m) = caps.name("src") {
                norm(m.as_str(), &player_name)
            } else {
                player_name.clone() // "your"
            };
            // Track resist count on the caster's stats.
            let stats = entity_stats(&mut state, &src);
            *stats.resists_by_spell.entry(spell.clone()).or_default() += 1;
            emit(&event_tx, CombatEvent::Resist { ts: current_ts, src, tgt, sp: spell });

        // ── /who player listing ("[65 Warrior Monk Rogue] Name (Race)") ─────────
        } else if let Some(caps) = RE_WHO.captures(line) {
            let name = caps["name"].to_owned();
            let classes = parse_who_classes(&caps["classes"]);
            if !classes.is_empty() {
                state.player_classes.insert(name.clone(), classes.clone());
                emit(&event_tx, CombatEvent::Who { ts: current_ts, name, classes });
            }

        // ── Healing ────────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_HEAL.captures(line) {
            let src = norm(&caps["src"], &player_name);
            let raw_tgt = caps["tgt"].trim().trim_end_matches(" over time");
            // Normalize reflexive pronouns to the healer.
            let tgt = match raw_tgt {
                "himself" | "herself" | "itself" | "yourself" => src.clone(),
                other => norm(other, &player_name),
            };
            let amt: u64 = caps["amt"].parse().unwrap_or(0);
            let spell = caps.name("spell")
                .map(|m| m.as_str().trim_end_matches('.').trim_end_matches(" over time").to_owned())
                .unwrap_or_else(|| "Unknown".to_owned());

            // Global aggregate healing
            let stats = entity_stats(&mut state, &src);
            stats.total_heals += amt;
            *stats.heals_by_spell.entry(spell.clone()).or_default() += amt;
            let tgt_stats = entity_stats(&mut state, &tgt);
            tgt_stats.total_healed_received += amt;
            *tgt_stats.healed_received_by_spell.entry(spell.clone()).or_default() += amt;

            // Per-mob-instance healing attribution
            let active_mob = state.active_mob_id;
            if let Some(mob_id) = active_mob {
                let heal_stats = state.mob_healing.entry(mob_id).or_default()
                    .entry(src.clone()).or_default();
                heal_stats.total_heals += amt;
                *heal_stats.heals_by_spell.entry(spell.clone()).or_default() += amt;

                let healed_stats = state.mob_healed.entry(mob_id).or_default()
                    .entry(tgt.clone()).or_default();
                healed_stats.total_healed_received += amt;
                *healed_stats.healed_received_by_spell.entry(spell.clone()).or_default() += amt;
            }
            emit(&event_tx, CombatEvent::Heal {
                ts: current_ts,
                mob: active_mob.map(|id| id as u32),
                src, tgt, amt: amt as u32, sp: spell, mods,
            });
        }

        // Update mob name and confirmed set from candidate tracking.
        if matched {
            update_mob_name(&mut state, &mob_candidates);
        }

        // Throttled publish: flush at most every 100 ms.
        if matched {
            let now = Instant::now();
            if now.duration_since(last_publish).as_millis() >= 100 {
                publish(&shared, &broadcast_tx, &state);
                last_publish = now;
            }
        } else if state.lines_parsed % 200 == 0 {
            shared.store(Arc::new(state.clone()));
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Record a mob death, freeze the fight timer if all confirmed mobs are down,
/// and emit a Slay event.  `killer` is empty string when unknown.
fn handle_slay(
    state: &mut CombatState,
    event_tx: &mpsc::UnboundedSender<CombatEvent>,
    tgt: String,
    killer: String,
    ts: u32,
) {
    state.dead_mobs.insert(tgt.clone());
    let all_dead = !state.mob_list.is_empty()
        && state.mob_list.iter()
            .filter(|m| state.confirmed_mobs.contains(&m.name))
            .all(|m| state.dead_mobs.contains(&m.name));
    if all_dead && state.fight_end.is_none() {
        state.fight_end = Some(Instant::now());
    }
    let mob_id = state.mob_list.iter()
        .find(|m| m.name == tgt)
        .map(|m| m.id as u32)
        .unwrap_or(0);
    emit(event_tx, CombatEvent::Slay { ts, mob: mob_id, tgt, killer });
}

fn update_mob_list(state: &mut CombatState, tgt: &str, log_ts: u32) -> u64 {
    // If this entity is a known player (has dealt damage), never add it to the mob list,
    // and remove it if it somehow got there. Use known_players rather than entities
    // because entities is also populated by mob healers.
    if state.known_players.contains(tgt) {
        state.mob_list.retain(|m| m.name != tgt);
        return state.active_mob_id.unwrap_or(u64::MAX);
    }

    let now = Instant::now();
    const GAP: Duration = Duration::from_secs(15);

    // If the mob was already confirmed dead, always start a fresh sighting for
    // the new instance even if it spawns within the 15-second gap window. This
    // prevents sequential same-named mobs from chaining into a single enormous
    // "encounter".  Clear the dead flag so the new sighting renders as alive.
    let was_dead = state.dead_mobs.remove(tgt);

    let id = if !was_dead {
        if let Some(s) = state.mob_list.iter_mut()
            .find(|m| m.name == tgt && now.duration_since(m.last_seen) < GAP)
        {
            s.last_seen = now;
            if log_ts != 0 { s.last_log_ts = log_ts; }
            s.id
        } else {
            let id = state.next_mob_id;
            state.next_mob_id += 1;
            state.mob_list.push(MobSighting { id, name: tgt.to_owned(), first_seen: now, last_seen: now, first_log_ts: log_ts, last_log_ts: log_ts });
            id
        }
    } else {
        let id = state.next_mob_id;
        state.next_mob_id += 1;
        state.mob_list.push(MobSighting { id, name: tgt.to_owned(), first_seen: now, last_seen: now, first_log_ts: log_ts, last_log_ts: log_ts });
        id
    };

    state.mob_list.sort_unstable_by(|a, b| b.last_seen.cmp(&a.last_seen));
    state.active_mob_id = Some(id);
    id
}

fn entity_stats<'a>(state: &'a mut CombatState, name: &str) -> &'a mut EntityCombatStats {
    state.entities.entry(name.to_owned()).or_default()
}

fn track_mob_candidate(
    candidates: &mut HashMap<String, HashSet<String>>,
    tgt: String,
    src: &str,
) {
    candidates.entry(tgt).or_default().insert(src.to_owned());
}

/// Confirm mob names from candidate tracking and pick the active mob for the header.
fn update_mob_name(state: &mut CombatState, candidates: &HashMap<String, HashSet<String>>) {
    // Any target that has been attacked and isn't a known player (damage dealer) is a mob.
    // Use known_players rather than entities — entities is also populated by mob healers
    // and would incorrectly promote healed mobs to player status.
    for tgt in candidates.keys() {
        if !state.known_players.contains(tgt.as_str()) {
            state.confirmed_mobs.insert(tgt.clone());
        } else {
            // Name is a known player (has dealt damage) — remove any incorrect mob tag
            // and evict from mob_list immediately.
            state.confirmed_mobs.remove(tgt.as_str());
            state.mob_list.retain(|m| m.name != tgt.as_str());
        }
    }

    let best = candidates
        .iter()
        .filter(|(tgt, _)| !state.known_players.contains(tgt.as_str()))
        .max_by_key(|(_, srcs)| srcs.len());

    if let Some((name, _)) = best {
        if state.mob_name != *name {
            state.mob_name = name.clone();
        }
    }
}

fn touch_fight_start(state: &mut CombatState) {
    if state.fight_start.is_none() {
        state.fight_start = Some(Instant::now());
        let now = Local::now();
        state.fight_start_display = format!(
            "{}/{}/{}, {}:{:02}:{:02} {}",
            now.month(), now.day(), now.year(),
            { let h = now.hour12().1; if h == 0 { 12 } else { h } },
            now.minute(),
            now.second(),
            if now.hour() < 12 { "AM" } else { "PM" },
        );
    }
}

fn emit(tx: &mpsc::UnboundedSender<CombatEvent>, ev: CombatEvent) {
    let _ = tx.send(ev);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn re_melee_comma_name_src() {
        let line = "Innoruuk, the Prince of Hate hits YOU for 170 points of damage.";
        let caps = RE_MELEE.captures(line).expect("RE_MELEE should match comma-named mob");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
        assert_eq!(&caps["tgt"], "YOU");
        assert_eq!(&caps["dmg"], "170");
    }

    #[test]
    fn re_melee_comma_name_bash() {
        let line = "Innoruuk, the Prince of Hate bashes YOU for 136 points of damage.";
        let caps = RE_MELEE.captures(line).expect("RE_MELEE should match comma-named bash");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
    }

    #[test]
    fn re_hit_by_spell_comma_name() {
        let line = "Innoruuk, the Prince of Hate hit Talodar for 100 points of unresistable damage by Avatar Power.";
        let caps = RE_HIT_BY_SPELL.captures(line).expect("RE_HIT_BY_SPELL should match comma-named mob");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
        assert_eq!(&caps["tgt"], "Talodar");
    }

    #[test]
    fn re_miss_comma_name() {
        let line = "Innoruuk, the Prince of Hate tries to bash YOU, but YOU dodge!";
        let caps = RE_MISS.captures(line).expect("RE_MISS should match comma-named mob");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
    }

    #[test]
    fn re_cast_comma_name() {
        let line = "Innoruuk, the Prince of Hate begins casting Avatar Power.";
        let caps = RE_CAST.captures(line).expect("RE_CAST should match comma-named mob");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
    }
}

fn publish(
    shared: &Arc<ArcSwap<CombatState>>,
    tx: &broadcast::Sender<Arc<CombatState>>,
    state: &CombatState,
) {
    let snap = Arc::new(state.clone());
    shared.store(Arc::clone(&snap));
    let _ = tx.send(snap);
}
