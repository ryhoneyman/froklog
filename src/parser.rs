use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::{Datelike, Local, Timelike};
use crossbeam_channel::Receiver;
use tokio::sync::{broadcast, mpsc};
use tracing::trace;

use crate::event::{
    CombatEvent, MODS_ASSASSINATE, MODS_CRIT, MODS_DOUBLEBOW, MODS_FLURRY, MODS_HEADSHOT,
    MODS_LUCKY, MODS_RAMPAGE, MODS_RIPOSTE_MOD, MODS_SLAY_UNDEAD, MODS_STRIKETHROUGH,
    MODS_TWINCAST,
};
use crate::patterns::{
    norm, normalize_article_case, normalize_miss, normalize_verb, parse_copper, parse_warder_owner,
    parse_who_classes, RE_ABSORB_RUNE, RE_ABSORB_SKIN, RE_CAST, RE_CC_PARK, RE_CC_WAKE,
    RE_CURRENCY_CORPSE, RE_DIED, RE_DOT, RE_DS, RE_DS_BURN_YOU, RE_DS_PROC, RE_EXTRA_DMG,
    RE_HAS_TAKEN, RE_HEAL, RE_HEARTBEAT, RE_HIT_BY_SPELL, RE_ITEM_MERGE, RE_LOOT_ENHANCE,
    RE_LOOT_HOARD, RE_LOOT_KEPT, RE_LOOT_SOLD, RE_MELEE, RE_MISS, RE_RESIST, RE_RIPOSTE,
    RE_SLAIN_BY, RE_SLAY_HAS, RE_SLAY_YOU, RE_SPELL_ATTR, RE_SPELL_HIT, RE_WHO, TS_LEN,
};
use crate::state::{CombatState, EntityCombatStats, MobSighting};

// ── Entry point ───────────────────────────────────────────────────────────────

/// Parse hit modifier flags from a parenthetical suffix like `(Lucky Critical Twincast)`.
fn parse_mods(suffix: &str) -> u16 {
    let text = suffix.trim_matches(|c| c == '(' || c == ')').trim();
    let mut mods = 0u16;
    if text.contains("Critical")
        || text.contains("Deadly Strike")
        || text.contains("Crippling Blow")
        || text.contains("Finishing Blow")
    {
        mods |= MODS_CRIT;
    }
    if text.contains("Twincast") {
        mods |= MODS_TWINCAST;
    }
    if text.contains("Lucky") {
        mods |= MODS_LUCKY;
    }
    if text.contains("Rampage") {
        mods |= MODS_RAMPAGE;
    }
    if text.contains("Strikethrough") {
        mods |= MODS_STRIKETHROUGH;
    }
    if text.contains("Riposte") {
        mods |= MODS_RIPOSTE_MOD;
    }
    if text.contains("Assassinate") {
        mods |= MODS_ASSASSINATE;
    }
    if text.contains("Headshot") {
        mods |= MODS_HEADSHOT;
    }
    if text.contains("Slay Undead") {
        mods |= MODS_SLAY_UNDEAD;
    }
    if text.contains("Double Bow Shot") {
        mods |= MODS_DOUBLEBOW;
    }
    if text.contains("Flurry") {
        mods |= MODS_FLURRY;
    }
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
            let mods = parse_mods(suffix);
            if mods != 0 {
                let rest = trimmed[..pos].trim_end();
                return (rest, mods);
            }
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
    let mut state = CombatState {
        player_name: player_name.clone(),
        ..Default::default()
    };

    let mut spell_caster: HashMap<String, String> = HashMap::new();
    let mut mob_candidates: HashMap<String, HashSet<String>> = HashMap::new();

    let mut last_publish = Instant::now();
    // Unix timestamp (seconds) of the most recently parsed EQ log line.
    let mut current_ts: u32 = 0;
    // Last log-second a combat heartbeat was emitted (throttle: stun spam
    // can produce many lines per second).
    let mut last_heartbeat_ts: u32 = 0;
    // Most recent Burnout-family cast: (caster, log-ts). The pet that "goes
    // berserk" within the correlation window belongs to this caster.
    let mut pending_pet_buff: Option<(String, u32)> = None;
    // Instanced zone currently occupied — consecutive re-entries into the
    // SAME instance (a corpse run, popping out to sell) must not cut a new
    // segment; only arriving somewhere different does.
    let mut current_instance: Option<String> = None;
    // Most recent pet-summon cast: the never-before-seen generated-name pet
    // whose first attack lands within the window belongs to this caster.
    // Covers classes without a Burnout tell (enchanter, necro).
    let mut pending_pet_summon: Option<(String, u32)> = None;

    for raw_line in &rx {
        if reset_flag.swap(false, Ordering::Relaxed) {
            let lines = state.lines_parsed;
            let player = state.player_name.clone();
            let player_classes = std::mem::take(&mut state.player_classes);
            let player_levels = std::mem::take(&mut state.player_levels);
            let known_pets = std::mem::take(&mut state.known_pets);
            state = CombatState {
                lines_parsed: lines,
                player_name: player,
                player_classes,
                player_levels,
                known_pets,
                ..Default::default()
            };
            spell_caster.clear();
            mob_candidates.clear();
            publish(&shared, &broadcast_tx, &state);
            last_publish = Instant::now();
        }

        state.lines_parsed += 1;

        let line = if raw_line.len() > TS_LEN && raw_line.starts_with('[') {
            if let Some(dt) = crate::tailer::parse_eq_timestamp(&raw_line) {
                state.last_log_time = format!(
                    "{} {:02}:{:02}",
                    dt.format("%b %-d"),
                    dt.hour(),
                    dt.minute()
                );
                // Convert the naive local log time to a TRUE unix epoch:
                // per-date timezone rules (correct DST for historical
                // imports) plus the measured server clock skew. Viewers
                // render this in their own local timezone.
                current_ts = crate::clock::naive_log_time_to_epoch(dt).max(0) as u32;
            }
            &raw_line[TS_LEN..]
        } else {
            raw_line.as_str()
        };

        trace!(line, "parsing");

        // Session boundary: player just logged in.
        if line == "Welcome to EverQuest Legends!" {
            emit(&event_tx, CombatEvent::Login { ts: current_ts });
        }

        // Entering an instance divides the pull list, exactly like a called
        // raid start — it rides the same marker event (kind "instance"), so
        // no new wire format and no deploy-order constraint.
        if let Some(caps) = crate::patterns::RE_INSTANCE_ENTER.captures(line) {
            let zone = caps[1].to_string();
            if current_instance.as_deref() != Some(zone.as_str()) {
                current_instance = Some(zone.clone());
                emit(
                    &event_tx,
                    CombatEvent::RaidMark {
                        ts: current_ts,
                        kind: "instance".to_string(),
                        label: crate::patterns::instance_label(&zone),
                    },
                );
            }
        }

        // Raid boundary called in chat. Checked before the combat patterns
        // because a chat line can quote anything, including something that
        // looks like a hit ("Zyro says, 'you slash it for 90'").
        if let Some(caps) = crate::patterns::RE_CHAT.captures(line) {
            if let Some((kind, label)) = crate::patterns::raid_mark(&caps[1]) {
                emit(
                    &event_tx,
                    CombatEvent::RaidMark {
                        ts: current_ts,
                        kind: kind.to_string(),
                        label,
                    },
                );
            }
            continue;
        }

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

            // Register Beastlord warders as pets before mob/player classification.
            if let Some(owner) = parse_warder_owner(&src) {
                let owner = owner.to_owned();
                register_warder(&mut state, &event_tx, &src, &owner, current_ts);
            }

            // Determine direction: mob→player or player→mob.
            // Known players are never mobs. Otherwise, src with a space is always a
            // mob (article prefix). If tgt is a known player the src must be a mob.
            let src_is_mob = !state.known_players.contains(&src)
                && (src.contains(' ')
                    || state.confirmed_mobs.contains(&src)
                    || state.known_players.contains(tgt.as_str())
                    || tgt == player_name
                    || mob_candidates.contains_key(src.as_str()));

            if src_is_mob {
                // Mob spell hitting a player — record as tanking damage.
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src.clone());
                }
                let mob_id = update_mob_list(&mut state, &src, current_ts);
                if let Some(mob_id) = mob_id {
                    let tank = state
                        .mob_tanking
                        .entry(mob_id)
                        .or_default()
                        .entry(tgt.clone())
                        .or_default();
                    tank.total_damage += dmg;
                    *tank.damage_by_type.entry("hit".to_owned()).or_default() += dmg;
                }
                emit(
                    &event_tx,
                    CombatEvent::Spell {
                        ts: current_ts,
                        mob: mob_id.unwrap_or(0) as u32,
                        src,
                        tgt,
                        dmg: dmg as u32,
                        sp: spell,
                        tank: true,
                        mods,
                    },
                );
            } else {
                // Player spell hitting a mob — record as player damage.
                state.known_players.insert(src.clone());
                let stats = entity_stats(&mut state, &src);
                stats.total_damage += dmg;
                *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                if mods & MODS_CRIT != 0 {
                    stats.crit_count += 1;
                }
                if mods & MODS_TWINCAST != 0 {
                    stats.twincast_count += 1;
                }

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                if let Some(mob_id) = mob_id {
                    let mob_p = state
                        .mob_damage
                        .entry(mob_id)
                        .or_default()
                        .entry(src.clone())
                        .or_default();
                    mob_p.total_damage += dmg;
                    *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                }
                emit(
                    &event_tx,
                    CombatEvent::Spell {
                        ts: current_ts,
                        mob: mob_id.unwrap_or(0) as u32,
                        src,
                        tgt,
                        dmg: dmg as u32,
                        sp: spell,
                        tank: false,
                        mods,
                    },
                );
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

            maybe_associate_summoned_pet(
                &mut state,
                &event_tx,
                &mut pending_pet_summon,
                &src,
                current_ts,
            );

            // Register Beastlord warders as pets before mob/player classification.
            if let Some(owner) = parse_warder_owner(&src) {
                let owner = owner.to_owned();
                register_warder(&mut state, &event_tx, &src, &owner, current_ts);
            }

            // "hits"/"hit" is an exclusively mob verb in EQ.
            // Also treat as mob attack if src has been targeted before (in mob_candidates),
            // if src name contains a space (multi-word = always a mob), if src was already
            // confirmed as a mob via another combat path, or if tgt is a known player
            // (a known player being hit means the attacker must be a mob).
            // However, if src is a confirmed player (in known_players), always treat as player
            // regardless of verb — some procs/items produce a bare "hit" line from players.
            let src_is_mob = !state.known_players.contains(&src)
                && (verb == "hit"
                    || verb == "hits"
                    || src.contains(' ')
                    || state.confirmed_mobs.contains(&src)
                    || mob_candidates.contains_key(src.as_str())
                    || state.known_players.contains(tgt.as_str())
                    || tgt == player_name);

            if src_is_mob {
                // Tanking: mob (src) damages player (tgt)
                let mob_id = update_mob_list(&mut state, &src, current_ts);
                if let Some(mob_id) = mob_id {
                    let tank = state
                        .mob_tanking
                        .entry(mob_id)
                        .or_default()
                        .entry(tgt.clone())
                        .or_default();
                    tank.total_damage += dmg;
                    *tank.damage_by_type.entry(typ.clone()).or_default() += dmg;
                }
                emit(
                    &event_tx,
                    CombatEvent::Melee {
                        ts: current_ts,
                        mob: mob_id.unwrap_or(0) as u32,
                        src: src.clone(),
                        tgt,
                        dmg: dmg as u32,
                        typ,
                        tank: true,
                        mods,
                    },
                );
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src);
                }
            } else {
                // Damage: player (src) attacks mob (tgt)
                state.known_players.insert(src.clone());
                let stats = entity_stats(&mut state, &src);
                stats.total_damage += dmg;
                *stats.damage_by_type.entry(typ.clone()).or_default() += dmg;
                if mods & MODS_CRIT != 0 {
                    stats.crit_count += 1;
                }
                if mods & MODS_TWINCAST != 0 {
                    stats.twincast_count += 1;
                }

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                if let Some(mob_id) = mob_id {
                    let mob_p = state
                        .mob_damage
                        .entry(mob_id)
                        .or_default()
                        .entry(src.clone())
                        .or_default();
                    mob_p.total_damage += dmg;
                    *mob_p.damage_by_type.entry(typ.clone()).or_default() += dmg;
                }
                emit(
                    &event_tx,
                    CombatEvent::Melee {
                        ts: current_ts,
                        mob: mob_id.unwrap_or(0) as u32,
                        src,
                        tgt,
                        dmg: dmg as u32,
                        typ,
                        tank: false,
                        mods,
                    },
                );
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
            let is_attributed = real_caster_opt.is_some() || state.known_players.contains(&raw_src);

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
                if mods & MODS_CRIT != 0 {
                    stats.crit_count += 1;
                }
                if mods & MODS_TWINCAST != 0 {
                    stats.twincast_count += 1;
                }

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                if let Some(mob_id) = mob_id {
                    let mob_p = state
                        .mob_damage
                        .entry(mob_id)
                        .or_default()
                        .entry(src.clone())
                        .or_default();
                    mob_p.total_damage += dmg;
                    *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                }
                emit(
                    &event_tx,
                    CombatEvent::Spell {
                        ts: current_ts,
                        mob: mob_id.unwrap_or(0) as u32,
                        src,
                        tgt,
                        dmg: dmg as u32,
                        sp: spell,
                        tank: false,
                        mods,
                    },
                );
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
                let caster_is_mob = !state.known_players.contains(&caster)
                    && (caster.contains(' ') || state.confirmed_mobs.contains(&caster));

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
                    if mods & MODS_CRIT != 0 {
                        stats.crit_count += 1;
                    }
                    if mods & MODS_TWINCAST != 0 {
                        stats.twincast_count += 1;
                    }

                    track_mob_candidate(&mut mob_candidates, tgt.clone(), &caster);
                    let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                    if let Some(mob_id) = mob_id {
                        let mob_p = state
                            .mob_damage
                            .entry(mob_id)
                            .or_default()
                            .entry(caster.clone())
                            .or_default();
                        mob_p.total_damage += dmg;
                        *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    }
                    emit(
                        &event_tx,
                        CombatEvent::Spell {
                            ts: current_ts,
                            mob: mob_id.unwrap_or(0) as u32,
                            src: caster,
                            tgt,
                            dmg: dmg as u32,
                            sp: spell,
                            tank: false,
                            mods,
                        },
                    );
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
            if mods & MODS_CRIT != 0 {
                stats.crit_count += 1;
            }
            if mods & MODS_TWINCAST != 0 {
                stats.twincast_count += 1;
            }

            track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
            let mob_id = update_mob_list(&mut state, &tgt, current_ts);
            if let Some(mob_id) = mob_id {
                let mob_p = state
                    .mob_damage
                    .entry(mob_id)
                    .or_default()
                    .entry(src.clone())
                    .or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            }
            emit(
                &event_tx,
                CombatEvent::Dot {
                    ts: current_ts,
                    mob: mob_id.unwrap_or(0) as u32,
                    src,
                    tgt,
                    dmg: dmg as u32,
                    sp: spell,
                    mods,
                },
            );

            touch_fight_start(&mut state);
            matched = true;

        // ── Riposte ────────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_RIPOSTE.captures(line) {
            let src = norm(&caps["src"], &player_name);
            let tgt = norm(&caps["tgt"], &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            // Register Beastlord warders as pets before mob/player classification.
            if let Some(owner) = parse_warder_owner(&src) {
                let owner = owner.to_owned();
                register_warder(&mut state, &event_tx, &src, &owner, current_ts);
            }

            // A mob can riposte a player: "Player was injured by Mob's riposte for N"
            let src_is_mob = !state.known_players.contains(&src)
                && (src.contains(' ')
                    || state.confirmed_mobs.contains(&src)
                    || state.known_players.contains(tgt.as_str())
                    || tgt == player_name);

            if src_is_mob {
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src.clone());
                }
                let mob_id = update_mob_list(&mut state, &src, current_ts);
                if let Some(mob_id) = mob_id {
                    let tank = state
                        .mob_tanking
                        .entry(mob_id)
                        .or_default()
                        .entry(tgt.clone())
                        .or_default();
                    tank.total_damage += dmg;
                    *tank.damage_by_type.entry("riposte".to_owned()).or_default() += dmg;
                }
                emit(
                    &event_tx,
                    CombatEvent::Spell {
                        ts: current_ts,
                        mob: mob_id.unwrap_or(0) as u32,
                        src,
                        tgt,
                        dmg: dmg as u32,
                        sp: "riposte".to_owned(),
                        tank: true,
                        mods,
                    },
                );
            } else {
                state.known_players.insert(src.clone());
                let stats = entity_stats(&mut state, &src);
                stats.total_damage += dmg;
                *stats
                    .damage_by_type
                    .entry("riposte".to_owned())
                    .or_default() += dmg;
                if mods & MODS_CRIT != 0 {
                    stats.crit_count += 1;
                }

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                if let Some(mob_id) = mob_id {
                    let mob_p = state
                        .mob_damage
                        .entry(mob_id)
                        .or_default()
                        .entry(src.clone())
                        .or_default();
                    mob_p.total_damage += dmg;
                    *mob_p
                        .damage_by_type
                        .entry("riposte".to_owned())
                        .or_default() += dmg;
                }
                emit(
                    &event_tx,
                    CombatEvent::Rip {
                        ts: current_ts,
                        mob: mob_id.unwrap_or(0) as u32,
                        src,
                        tgt,
                        dmg: dmg as u32,
                        mods,
                    },
                );
            }

            touch_fight_start(&mut state);
            matched = true;

        // ── Damage shield ──────────────────────────────────────────────────────
        } else if let Some(caps) = RE_DS.captures(line) {
            let src = norm(&caps["src"], &player_name);
            let tgt = norm(&caps["tgt"], &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            // Register Beastlord warders as pets before mob/player classification.
            if let Some(owner) = parse_warder_owner(&src) {
                let owner = owner.to_owned();
                register_warder(&mut state, &event_tx, &src, &owner, current_ts);
            }

            // A mob can have a damage shield: "Player was struck by Mob's damage shield for N"
            let src_is_mob = !state.known_players.contains(&src)
                && (src.contains(' ')
                    || state.confirmed_mobs.contains(&src)
                    || state.known_players.contains(tgt.as_str())
                    || tgt == player_name);

            if src_is_mob {
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src.clone());
                }
                let mob_id = update_mob_list(&mut state, &src, current_ts);
                if let Some(mob_id) = mob_id {
                    let tank = state
                        .mob_tanking
                        .entry(mob_id)
                        .or_default()
                        .entry(tgt.clone())
                        .or_default();
                    tank.total_damage += dmg;
                    *tank.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
                }
                emit(
                    &event_tx,
                    CombatEvent::Spell {
                        ts: current_ts,
                        mob: mob_id.unwrap_or(0) as u32,
                        src,
                        tgt,
                        dmg: dmg as u32,
                        sp: "ds".to_owned(),
                        tank: true,
                        mods: 0,
                    },
                );
            } else {
                state.known_players.insert(src.clone());
                let stats = entity_stats(&mut state, &src);
                stats.total_damage += dmg;
                *stats.damage_by_type.entry("ds".to_owned()).or_default() += dmg;

                track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                if let Some(mob_id) = mob_id {
                    let mob_p = state
                        .mob_damage
                        .entry(mob_id)
                        .or_default()
                        .entry(src.clone())
                        .or_default();
                    mob_p.total_damage += dmg;
                    *mob_p.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
                }
                emit(
                    &event_tx,
                    CombatEvent::Ds {
                        ts: current_ts,
                        mob: mob_id.unwrap_or(0) as u32,
                        src,
                        tgt,
                        dmg: dmg as u32,
                    },
                );
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
            if let Some(mob_id) = mob_id {
                let mob_p = state
                    .mob_damage
                    .entry(mob_id)
                    .or_default()
                    .entry(src.clone())
                    .or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
            }

            emit(
                &event_tx,
                CombatEvent::Ds {
                    ts: current_ts,
                    mob: mob_id.unwrap_or(0) as u32,
                    src,
                    tgt,
                    dmg: dmg as u32,
                },
            );

            touch_fight_start(&mut state);
            matched = true;

        // ── Inbound DS proc: mob's damage shield burning the player ───────────
        } else if let Some(caps) = RE_DS_BURN_YOU.captures(line) {
            let src = norm(caps["src"].trim(), &player_name);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            state.confirmed_mobs.insert(src.clone());
            let mob_id = update_mob_list(&mut state, &src, current_ts);
            if let Some(mob_id) = mob_id {
                let tank = state
                    .mob_tanking
                    .entry(mob_id)
                    .or_default()
                    .entry(player_name.clone())
                    .or_default();
                tank.total_damage += dmg;
                *tank.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
            }
            emit(
                &event_tx,
                CombatEvent::Melee {
                    ts: current_ts,
                    mob: mob_id.unwrap_or(0) as u32,
                    src,
                    tgt: player_name.clone(),
                    dmg: dmg as u32,
                    typ: "ds".to_owned(),
                    tank: true,
                    mods: 0,
                },
            );

            touch_fight_start(&mut state);
            matched = true;

        // ── Crowd control: mob parked (mesmerized/enthralled) ─────────────────
        } else if let Some(caps) = RE_CC_PARK.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &player_name);
            // Registers an unengaged add as a pull member the moment CC lands,
            // and suspends its idle/gap timers until it wakes.
            let mob_id = update_mob_list(&mut state, &tgt, current_ts);
            if let Some(id) = mob_id {
                if let Some(s) = state.mob_list.iter_mut().find(|m| m.id == id) {
                    s.parked = true;
                }
                emit(
                    &event_tx,
                    CombatEvent::Cc {
                        ts: current_ts,
                        mob: id as u32,
                        tgt,
                        off: false,
                    },
                );
            }
            matched = true;

        // ── Crowd control broken ("X has been awakened by Y") ─────────────────
        } else if let Some(caps) = RE_CC_WAKE.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &player_name);
            // update_mob_list clears `parked` on match.
            let mob_id = update_mob_list(&mut state, &tgt, current_ts);
            if let Some(id) = mob_id {
                emit(
                    &event_tx,
                    CombatEvent::Cc {
                        ts: current_ts,
                        mob: id as u32,
                        tgt,
                        off: true,
                    },
                );
            }
            matched = true;

        // ── Combat heartbeat: player stunned / OOM / interrupted ──────────────
        } else if RE_HEARTBEAT.is_match(line) {
            if current_ts != 0 && current_ts != last_heartbeat_ts {
                last_heartbeat_ts = current_ts;
                emit(&event_tx, CombatEvent::Heartbeat { ts: current_ts });
            }
            matched = true;

        // ── Spell cast — record caster for attribution ─────────────────────────
        } else if let Some(caps) = RE_CAST.captures(line) {
            let src = norm(&caps["src"], &player_name);
            let spell = caps["spell"].to_owned();
            // Burnout can only target the caster's own pet — remember the
            // caster so the "goes berserk" landing can attribute the pet.
            // Single-token gate: player names have no spaces, NPCs do.
            if crate::patterns::is_pet_buff_spell(&spell) && !src.contains(' ') {
                pending_pet_buff = Some((src.clone(), current_ts));
            }
            if crate::patterns::is_pet_summon_spell(&spell) && !src.contains(' ') {
                pending_pet_summon = Some((src.clone(), current_ts));
            }
            spell_caster.insert(spell.clone(), src.clone());
            state
                .active_casts
                .insert(src.clone(), (spell.clone(), Instant::now()));
            emit(
                &event_tx,
                CombatEvent::Cast {
                    ts: current_ts,
                    src,
                    sp: spell,
                },
            );

        // ── Fizzles (own only — EQ doesn't log other players') ─────────────────
        } else if let Some(caps) = crate::patterns::RE_FIZZLE.captures(line) {
            emit(
                &event_tx,
                CombatEvent::Fizzle {
                    ts: current_ts,
                    src: player_name.clone(),
                    sp: caps["sp"].to_owned(),
                },
            );

        // ── Pet ownership: Burnout landing ─────────────────────────────────────
        // "<Pet> goes berserk." right after "<Player> begins casting Burnout"
        // → that generated-name pet belongs to that player. Re-learned on
        // every rebuff, so per-summon name changes take care of themselves.
        } else if let Some(caps) = crate::patterns::RE_PET_BERSERK.captures(line) {
            let pet = caps["name"].to_owned();
            if let Some((owner, cast_ts)) = pending_pet_buff.clone() {
                if current_ts.saturating_sub(cast_ts) <= 10
                    && owner != pet
                    && crate::patterns::is_generated_pet_name(&pet)
                {
                    state.known_pets.insert(pet.clone(), owner.clone());
                    state.known_players.insert(pet.clone());
                    emit(
                        &event_tx,
                        CombatEvent::Pet {
                            ts: current_ts,
                            name: pet,
                            owner,
                        },
                    );
                    pending_pet_buff = None;
                }
            }

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

        // ── Loot and currency ──────────────────────────────────────────────────

        // "You receive 6 platinum, 1 gold, 8 silver and 3 copper from the corpse."
        // NOTE: EQL emits currency BEFORE the kill message, so we use active_mob_id
        // (the mob currently engaged in combat) rather than the post-slay pending mob.
        } else if let Some(caps) = RE_CURRENCY_CORPSE.captures(line) {
            let copper = parse_copper(&caps["amounts"]);
            let mob = state.active_mob_id.unwrap_or(0) as u32;
            emit(
                &event_tx,
                CombatEvent::CurrencyLoot {
                    ts: current_ts,
                    mob,
                    copper,
                },
            );

        // "--You have looted X from mob's corpse.--"
        } else if let Some(caps) = RE_LOOT_KEPT.captures(line) {
            let item = caps["item"].to_owned();
            let mob_name = normalize_article_case(&caps["mob"]);
            let mob = resolve_loot_mob(&mut state, &mob_name, current_ts);
            emit(
                &event_tx,
                CombatEvent::ItemLoot {
                    ts: current_ts,
                    mob,
                    item,
                    qty: 1,
                },
            );

        // "You looted [N] X from mob's corpse and sold it for Y."
        } else if let Some(caps) = RE_LOOT_SOLD.captures(line) {
            let qty: u32 = caps
                .name("qty")
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(1);
            let item = caps["item"].to_owned();
            let mob_name = normalize_article_case(&caps["mob"]);
            let price_str = &caps["price"];
            let copper = if price_str == "free" {
                0
            } else {
                parse_copper(price_str)
            };
            let mob = resolve_loot_mob(&mut state, &mob_name, current_ts);
            emit(
                &event_tx,
                CombatEvent::ItemSell {
                    ts: current_ts,
                    mob,
                    item,
                    qty,
                    copper,
                },
            );

        // "You looted X from mob's corpse and stored it in your Dragon Hoard"
        } else if let Some(caps) = RE_LOOT_HOARD.captures(line) {
            let item = caps["item"].to_owned();
            let mob_name = normalize_article_case(&caps["mob"]);
            let mob = resolve_loot_mob(&mut state, &mob_name, current_ts);
            emit(
                &event_tx,
                CombatEvent::ItemHoard {
                    ts: current_ts,
                    mob,
                    item,
                },
            );

        // "You looted X from mob's corpse to create Y"
        } else if let Some(caps) = RE_LOOT_ENHANCE.captures(line) {
            let item = caps["item"].to_owned();
            let result = caps.name("result").map(|m| m.as_str().trim().to_owned());
            let mob_name = normalize_article_case(&caps["mob"]);
            let mob = resolve_loot_mob(&mut state, &mob_name, current_ts);
            emit(
                &event_tx,
                CombatEvent::ItemEnhance {
                    ts: current_ts,
                    mob,
                    item,
                    result,
                },
            );

        // "You have successfully merged two items together to create a new item: X +N"
        } else if let Some(caps) = RE_ITEM_MERGE.captures(line) {
            emit(
                &event_tx,
                CombatEvent::ItemMerge {
                    ts: current_ts,
                    mob: 0,
                    result: caps["result"].trim().to_owned(),
                },
            );

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
                let real_src = if s.is_empty() {
                    None
                } else {
                    Some(norm(s, &player_name))
                };
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
                // Register Beastlord warders as pets before mob/player classification.
                if let Some(owner) = parse_warder_owner(&src) {
                    state
                        .known_pets
                        .entry(src.clone())
                        .or_insert_with(|| owner.to_owned());
                    state.known_players.insert(src.clone());
                }

                let src_is_mob = !state.known_players.contains(&src)
                    && (src.contains(' ')
                        || state.confirmed_mobs.contains(&src)
                        || state.known_players.contains(tgt.as_str())
                        || tgt == player_name);
                if src_is_mob && src != player_name {
                    // Mob DoT/spell hitting a player — tanking
                    if !state.known_players.contains(&src) {
                        state.confirmed_mobs.insert(src.clone());
                    }
                    let mob_id = update_mob_list(&mut state, &src, current_ts);
                    if let Some(mob_id) = mob_id {
                        let tank = state
                            .mob_tanking
                            .entry(mob_id)
                            .or_default()
                            .entry(tgt.clone())
                            .or_default();
                        tank.total_damage += dmg;
                        *tank.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                    }
                    emit(
                        &event_tx,
                        CombatEvent::Spell {
                            ts: current_ts,
                            mob: mob_id.unwrap_or(0) as u32,
                            src,
                            tgt,
                            dmg: dmg as u32,
                            sp: spell,
                            tank: true,
                            mods,
                        },
                    );
                } else {
                    // Player DoT hitting a mob
                    state.known_players.insert(src.clone());
                    let stats = entity_stats(&mut state, &src);
                    stats.total_damage += dmg;
                    *stats.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                    *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    if mods & MODS_CRIT != 0 {
                        stats.crit_count += 1;
                    }
                    if mods & MODS_TWINCAST != 0 {
                        stats.twincast_count += 1;
                    }

                    track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
                    let mob_id = update_mob_list(&mut state, &tgt, current_ts);
                    if let Some(mob_id) = mob_id {
                        let mob_p = state
                            .mob_damage
                            .entry(mob_id)
                            .or_default()
                            .entry(src.clone())
                            .or_default();
                        mob_p.total_damage += dmg;
                        *mob_p.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                        *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    }
                    emit(
                        &event_tx,
                        CombatEvent::Dot {
                            ts: current_ts,
                            mob: mob_id.unwrap_or(0) as u32,
                            src,
                            tgt,
                            dmg: dmg as u32,
                            sp: spell,
                            mods,
                        },
                    );
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
            if mods & MODS_CRIT != 0 {
                stats.crit_count += 1;
            }
            if mods & MODS_TWINCAST != 0 {
                stats.twincast_count += 1;
            }

            track_mob_candidate(&mut mob_candidates, tgt.clone(), &src);
            let mob_id = update_mob_list(&mut state, &tgt, current_ts);
            if let Some(mob_id) = mob_id {
                let mob_p = state
                    .mob_damage
                    .entry(mob_id)
                    .or_default()
                    .entry(src.clone())
                    .or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            }
            emit(
                &event_tx,
                CombatEvent::Spell {
                    ts: current_ts,
                    mob: mob_id.unwrap_or(0) as u32,
                    src,
                    tgt,
                    dmg: dmg as u32,
                    sp: spell,
                    tank: false,
                    mods,
                },
            );
            touch_fight_start(&mut state);
            matched = true;

        // ── Miss / avoidance ("X tries to Y Z, but Z dodges!") ───────────────
        } else if let Some(caps) = RE_MISS.captures(line) {
            let src = norm(caps["src"].trim(), &player_name);
            let tgt = norm(caps["tgt"].trim(), &player_name);
            let miss_type = normalize_miss(&caps["miss"]).to_owned();

            maybe_associate_summoned_pet(
                &mut state,
                &event_tx,
                &mut pending_pet_summon,
                &src,
                current_ts,
            );

            // Track avoidance on the defender (tgt).
            let def_stats = entity_stats(&mut state, &tgt);
            *def_stats
                .avoidance_by_type
                .entry(miss_type.clone())
                .or_default() += 1;

            // Register Beastlord warders as pets before mob/player classification.
            if let Some(owner) = parse_warder_owner(&src) {
                let owner = owner.to_owned();
                register_warder(&mut state, &event_tx, &src, &owner, current_ts);
            }

            // If src is a mob, also record on mob_tanking avoidance.
            let src_is_mob = !state.known_players.contains(&src)
                && (src.contains(' ')
                    || state.confirmed_mobs.contains(&src)
                    || state.known_players.contains(tgt.as_str())
                    || tgt == player_name);
            let mob_id: u64 = if src_is_mob {
                if !state.known_players.contains(&src) {
                    state.confirmed_mobs.insert(src.clone());
                }
                let id = update_mob_list(&mut state, &src, current_ts);
                if let Some(id) = id {
                    let tank = state
                        .mob_tanking
                        .entry(id)
                        .or_default()
                        .entry(tgt.clone())
                        .or_default();
                    *tank.avoidance_by_type.entry(miss_type.clone()).or_default() += 1;
                }
                id.unwrap_or(0)
            } else {
                state.active_mob_id.unwrap_or(0)
            };

            emit(
                &event_tx,
                CombatEvent::Miss {
                    ts: current_ts,
                    mob: mob_id as u32,
                    src,
                    tgt,
                    typ: miss_type,
                },
            );

        // ── Absorb: magical skin ("X's magical skin absorbs the damage…") ─────
        } else if let Some(caps) = RE_ABSORB_SKIN.captures(line) {
            let tgt = if let Some(m) = caps.name("tgt") {
                norm(m.as_str(), &player_name)
            } else {
                player_name.clone() // YOUR magical skin
            };
            let src = norm(caps["src"].trim(), &player_name);
            let mob_id = state.active_mob_id.unwrap_or(0);
            emit(
                &event_tx,
                CombatEvent::Absorb {
                    ts: current_ts,
                    mob: mob_id as u32,
                    tgt,
                    src,
                },
            );

        // ── Absorb: rune shield ("X has shielded itself from N points…") ──────
        } else if let Some(caps) = RE_ABSORB_RUNE.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &player_name);
            let mob_id = state.active_mob_id.unwrap_or(0);
            emit(
                &event_tx,
                CombatEvent::Absorb {
                    ts: current_ts,
                    mob: mob_id as u32,
                    tgt,
                    src: String::new(),
                },
            );

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
            emit(
                &event_tx,
                CombatEvent::Resist {
                    ts: current_ts,
                    src,
                    tgt,
                    sp: spell,
                },
            );

        // ── /who player listing ("[65 Warrior Monk Rogue] Name (Race)") ─────────
        } else if let Some(caps) = RE_WHO.captures(line) {
            let name = caps["name"].to_owned();
            let classes = parse_who_classes(&caps["classes"]);
            let level: u8 = caps["lvl"].parse().unwrap_or(0);
            if !classes.is_empty() {
                state.player_classes.insert(name.clone(), classes.clone());
                state.player_levels.insert(name.clone(), level);
                emit(
                    &event_tx,
                    CombatEvent::Who {
                        ts: current_ts,
                        name,
                        classes,
                        level,
                    },
                );
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

            // Mob self-heals ("an orc thaumaturgist healed itself…"): update
            // the mob's sighting so its encounter window stays open while it
            // heal-turtles, but don't emit a Heal event — the viewer's healer
            // lists are for the player group.
            let src_is_mob = !state.known_players.contains(&src)
                && (state.confirmed_mobs.contains(&src) || (src.contains(' ') && tgt == src));
            if src_is_mob {
                update_mob_list(&mut state, &src, current_ts);
            } else {
                let amt: u64 = caps["amt"].parse().unwrap_or(0);
                let spell = caps
                    .name("spell")
                    .map(|m| {
                        m.as_str()
                            .trim_end_matches('.')
                            .trim_end_matches(" over time")
                            .to_owned()
                    })
                    .unwrap_or_else(|| "Unknown".to_owned());

                // Global aggregate healing
                let stats = entity_stats(&mut state, &src);
                stats.total_heals += amt;
                *stats.heals_by_spell.entry(spell.clone()).or_default() += amt;
                let tgt_stats = entity_stats(&mut state, &tgt);
                tgt_stats.total_healed_received += amt;
                *tgt_stats
                    .healed_received_by_spell
                    .entry(spell.clone())
                    .or_default() += amt;

                // Per-mob-instance healing attribution
                let active_mob = state.active_mob_id;
                if let Some(mob_id) = active_mob {
                    let heal_stats = state
                        .mob_healing
                        .entry(mob_id)
                        .or_default()
                        .entry(src.clone())
                        .or_default();
                    heal_stats.total_heals += amt;
                    *heal_stats.heals_by_spell.entry(spell.clone()).or_default() += amt;

                    let healed_stats = state
                        .mob_healed
                        .entry(mob_id)
                        .or_default()
                        .entry(tgt.clone())
                        .or_default();
                    healed_stats.total_healed_received += amt;
                    *healed_stats
                        .healed_received_by_spell
                        .entry(spell.clone())
                        .or_default() += amt;
                }
                emit(
                    &event_tx,
                    CombatEvent::Heal {
                        ts: current_ts,
                        mob: active_mob.map(|id| id as u32),
                        src,
                        tgt,
                        amt: amt as u32,
                        sp: spell,
                        mods,
                    },
                );
            }
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
        } else if state.lines_parsed.is_multiple_of(200) {
            shared.store(Arc::new(state.clone()));
        }
    }
    // Channel closed (channel disconnected or all senders dropped) — publish final state.
    publish(&shared, &broadcast_tx, &state);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Record a mob death, freeze the fight timer if all confirmed mobs are down,
/// and emit a Slay event.  `killer` is empty string when unknown.
/// A never-before-associated generated-name pet showing up as an attacker
/// shortly after a player's summon cast belongs to that caster. Secondary
/// to the Burnout correlation — this is the net for classes whose pets get
/// no visible buff landing.
fn maybe_associate_summoned_pet(
    state: &mut CombatState,
    event_tx: &mpsc::UnboundedSender<CombatEvent>,
    pending: &mut Option<(String, u32)>,
    src: &str,
    ts: u32,
) {
    let Some((owner, cast_ts)) = pending.clone() else {
        return;
    };
    if ts.saturating_sub(cast_ts) <= 60
        && owner != src
        && !state.known_pets.contains_key(src)
        && crate::patterns::is_generated_pet_name(src)
    {
        state.known_pets.insert(src.to_owned(), owner.clone());
        state.known_players.insert(src.to_owned());
        emit(
            event_tx,
            CombatEvent::Pet {
                ts,
                name: src.to_owned(),
                owner,
            },
        );
        *pending = None;
    }
}

/// Register a possessively-named pet ("X`s warder") as owned, and — the
/// first time this pet is seen — tell downstream viewers who owns it.
fn register_warder(
    state: &mut CombatState,
    event_tx: &mpsc::UnboundedSender<CombatEvent>,
    src: &str,
    owner: &str,
    ts: u32,
) {
    if !state.known_pets.contains_key(src) {
        state.known_pets.insert(src.to_owned(), owner.to_owned());
        emit(
            event_tx,
            CombatEvent::Pet {
                ts,
                name: src.to_owned(),
                owner: owner.to_owned(),
            },
        );
    }
    state.known_players.insert(src.to_owned());
}

fn handle_slay(
    state: &mut CombatState,
    event_tx: &mpsc::UnboundedSender<CombatEvent>,
    tgt: String,
    killer: String,
    ts: u32,
) {
    // dead_mobs is keyed lowercase: slay lines capitalize the name at
    // sentence start, combat lines usually don't.
    state.dead_mobs.insert(tgt.to_ascii_lowercase());
    let all_dead = !state.mob_list.is_empty()
        && state
            .mob_list
            .iter()
            .filter(|m| state.confirmed_mobs.contains(&m.name))
            .all(|m| state.dead_mobs.contains(&m.name.to_ascii_lowercase()));
    if all_dead && state.fight_end.is_none() {
        state.fight_end = Some(Instant::now());
    }
    let mob_id = state
        .mob_list
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(&tgt))
        .map(|m| m.id as u32)
        .unwrap_or(0);
    state.pending_loot_mob = Some(mob_id);
    state.pending_loot_ts = ts;
    emit(
        event_tx,
        CombatEvent::Slay {
            ts,
            mob: mob_id,
            tgt,
            killer,
        },
    );
}

/// Returns `Some(id)` when `tgt` is a real mob, `None` when it's a corpse or a known player.
/// Callers must skip `mob_damage`/`mob_tanking` updates on `None`; they may still emit
/// events using `mob: 0`.
fn update_mob_list(state: &mut CombatState, tgt: &str, log_ts: u32) -> Option<u64> {
    // Corpses ("X's corpse") must never enter the mob list.
    if tgt.ends_with("'s corpse") {
        return None;
    }
    // If this entity is a known player (has dealt damage), never add it to the mob list,
    // and remove it if it somehow got there. Use known_players rather than entities
    // because entities is also populated by mob healers.
    if state.known_players.contains(tgt) {
        state.mob_list.retain(|m| m.name != tgt);
        return None;
    }

    let now = Instant::now();
    const GAP: Duration = Duration::from_secs(15);

    // If the mob was already confirmed dead, always start a fresh sighting for
    // the new instance even if it spawns within the 15-second gap window. This
    // prevents sequential same-named mobs from chaining into a single enormous
    // "encounter".  Clear the dead flag so the new sighting renders as alive.
    let was_dead = state.dead_mobs.remove(&tgt.to_ascii_lowercase());

    // Same-instance window: prefer LOG-time deltas so the same log always
    // produces the same encounters (deterministic across live play, imports,
    // and replays — wall-clock gaps depended on how fast the file was read).
    // Falls back to wall-clock only when a line carried no parseable timestamp.
    let within_gap = |m: &MobSighting| {
        // A crowd-controlled mob is deliberately idle: no gap applies while
        // it is parked, however long the group leaves it mezzed.
        if m.parked {
            return true;
        }
        if log_ts != 0 && m.last_log_ts != 0 {
            log_ts.saturating_sub(m.last_log_ts) < GAP.as_secs() as u32
        } else {
            now.duration_since(m.last_seen) < GAP
        }
    };

    let id = 'find: {
        if !was_dead {
            // Case-insensitive: EQ capitalizes a mob's name when it opens the
            // sentence ("Orc legionnaire hits YOU") but not mid-sentence
            // ("You slash orc legionnaire"). Exact matching split every fight
            // into two instances — one holding the player's damage, one the
            // tanking — which the viewer showed as two mobs.
            if let Some(s) = state
                .mob_list
                .iter_mut()
                .find(|m| m.name.eq_ignore_ascii_case(tgt) && within_gap(m))
            {
                s.last_seen = now;
                if log_ts != 0 {
                    s.last_log_ts = log_ts;
                }
                // Any fresh line involving the mob means it is acting again.
                s.parked = false;
                // Prefer the mid-sentence (lowercase) form as the display name.
                if s.name != tgt && tgt.chars().next().is_some_and(|c| c.is_lowercase()) {
                    s.name = tgt.to_owned();
                }
                break 'find s.id;
            }
        }
        // New instance: either mob was dead, or no recent sighting found.
        let id = state.next_mob_id;
        state.next_mob_id += 1;
        state.mob_list.push(MobSighting {
            id,
            name: tgt.to_owned(),
            first_seen: now,
            last_seen: now,
            first_log_ts: log_ts,
            last_log_ts: log_ts,
            parked: false,
        });
        id
    };

    state.active_mob_id = Some(id);
    Some(id)
}

fn entity_stats<'a>(state: &'a mut CombatState, name: &str) -> &'a mut EntityCombatStats {
    state.entities.entry(name.to_owned()).or_default()
}

/// Resolve the mob-instance ID for a loot event. Prefers a name-matched sighting in
/// mob_list (most recently inserted wins), falls back to pending_loot_mob if the
/// pending attribution is still fresh (< 120 s old).
fn resolve_loot_mob(state: &mut CombatState, mob_name: &str, current_ts: u32) -> u32 {
    if current_ts != 0 && current_ts.saturating_sub(state.pending_loot_ts) > 120 {
        state.pending_loot_mob = None;
    }
    state
        .mob_list
        .iter()
        .rev()
        .find(|m| m.name.eq_ignore_ascii_case(mob_name))
        .map(|m| m.id as u32)
        .or(state.pending_loot_mob)
        .unwrap_or(0)
}

fn track_mob_candidate(candidates: &mut HashMap<String, HashSet<String>>, tgt: String, src: &str) {
    if tgt.ends_with("'s corpse") {
        return;
    }
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
            now.month(),
            now.day(),
            now.year(),
            {
                let h = now.hour12().1;
                if h == 0 {
                    12
                } else {
                    h
                }
            },
            now.minute(),
            now.second(),
            if now.hour() < 12 { "AM" } else { "PM" },
        );
    }
}

fn emit(tx: &mpsc::UnboundedSender<CombatEvent>, ev: CombatEvent) {
    let _ = tx.send(ev);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn re_item_merge_captures_result() {
        let line = "You have successfully merged two items together to create a new item: Boots of the Long Road +2";
        let caps = RE_ITEM_MERGE
            .captures(line)
            .expect("RE_ITEM_MERGE should match the manual merge line");
        assert_eq!(&caps["result"], "Boots of the Long Road +2");
    }

    #[test]
    fn re_loot_enhance_captures_result() {
        let line = "You looted a Ebon Scythe +1 from a gnoll's corpse to create a Ebon Scythe +2";
        let caps = RE_LOOT_ENHANCE
            .captures(line)
            .expect("RE_LOOT_ENHANCE should match");
        assert_eq!(&caps["item"], "a Ebon Scythe +1");
        assert_eq!(
            caps.name("result").map(|m| m.as_str()),
            Some("a Ebon Scythe +2"),
            "the resulting item carries the new tier"
        );
    }

    #[test]
    fn re_melee_comma_name_src() {
        let line = "Innoruuk, the Prince of Hate hits YOU for 170 points of damage.";
        let caps = RE_MELEE
            .captures(line)
            .expect("RE_MELEE should match comma-named mob");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
        assert_eq!(&caps["tgt"], "YOU");
        assert_eq!(&caps["dmg"], "170");
    }

    #[test]
    fn re_melee_comma_name_bash() {
        let line = "Innoruuk, the Prince of Hate bashes YOU for 136 points of damage.";
        let caps = RE_MELEE
            .captures(line)
            .expect("RE_MELEE should match comma-named bash");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
    }

    #[test]
    fn re_hit_by_spell_comma_name() {
        let line = "Innoruuk, the Prince of Hate hit Talodar for 100 points of unresistable damage by Avatar Power.";
        let caps = RE_HIT_BY_SPELL
            .captures(line)
            .expect("RE_HIT_BY_SPELL should match comma-named mob");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
        assert_eq!(&caps["tgt"], "Talodar");
    }

    #[test]
    fn re_miss_comma_name() {
        let line = "Innoruuk, the Prince of Hate tries to bash YOU, but YOU dodge!";
        let caps = RE_MISS
            .captures(line)
            .expect("RE_MISS should match comma-named mob");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
    }

    #[test]
    fn re_cast_comma_name() {
        let line = "Innoruuk, the Prince of Hate begins casting Avatar Power.";
        let caps = RE_CAST
            .captures(line)
            .expect("RE_CAST should match comma-named mob");
        assert_eq!(caps["src"].trim(), "Innoruuk, the Prince of Hate");
    }

    // ── parse_mods ───────────────────────────────────────────────────────────────
    #[test]
    fn parse_mods_empty() {
        assert_eq!(parse_mods("()"), 0);
    }
    #[test]
    fn parse_mods_critical() {
        assert_eq!(parse_mods("(Critical)"), MODS_CRIT);
    }
    #[test]
    fn parse_mods_deadly() {
        assert_eq!(parse_mods("(Deadly Strike)"), MODS_CRIT);
    }
    #[test]
    fn parse_mods_crippling() {
        assert_eq!(parse_mods("(Crippling Blow)"), MODS_CRIT);
    }
    #[test]
    fn parse_mods_finishing() {
        assert_eq!(parse_mods("(Finishing Blow)"), MODS_CRIT);
    }
    #[test]
    fn parse_mods_twincast() {
        assert_eq!(parse_mods("(Twincast)"), MODS_TWINCAST);
    }
    #[test]
    fn parse_mods_lucky() {
        assert_eq!(parse_mods("(Lucky)"), MODS_LUCKY);
    }
    #[test]
    fn parse_mods_rampage() {
        assert!(parse_mods("(Rampage)") & MODS_RAMPAGE != 0);
    }
    #[test]
    fn parse_mods_strike() {
        assert!(parse_mods("(Strikethrough)") & MODS_STRIKETHROUGH != 0);
    }
    #[test]
    fn parse_mods_riposte_mod() {
        assert!(parse_mods("(Riposte)") & MODS_RIPOSTE_MOD != 0);
    }
    #[test]
    fn parse_mods_assassinate() {
        assert!(parse_mods("(Assassinate)") & MODS_ASSASSINATE != 0);
    }
    #[test]
    fn parse_mods_headshot() {
        assert!(parse_mods("(Headshot)") & MODS_HEADSHOT != 0);
    }
    #[test]
    fn parse_mods_slay_undead() {
        assert!(parse_mods("(Slay Undead)") & MODS_SLAY_UNDEAD != 0);
    }
    #[test]
    fn parse_mods_doublebow() {
        assert!(parse_mods("(Double Bow Shot)") & MODS_DOUBLEBOW != 0);
    }
    #[test]
    fn parse_mods_flurry() {
        assert!(parse_mods("(Flurry)") & MODS_FLURRY != 0);
    }
    #[test]
    fn parse_mods_combined() {
        let m = parse_mods("(Lucky Critical Twincast)");
        assert_eq!(m, MODS_LUCKY | MODS_CRIT | MODS_TWINCAST);
    }

    // ── strip_mods ───────────────────────────────────────────────────────────────
    #[test]
    fn strip_mods_no_suffix() {
        let (line, mods) = strip_mods("Rysk slashes a goblin for 150 points of damage.");
        assert_eq!(line, "Rysk slashes a goblin for 150 points of damage.");
        assert_eq!(mods, 0);
    }
    #[test]
    fn strip_mods_critical() {
        let (line, mods) = strip_mods("Rysk slashes a goblin for 150 points of damage. (Critical)");
        assert_eq!(line, "Rysk slashes a goblin for 150 points of damage.");
        assert_eq!(mods, MODS_CRIT);
    }
    #[test]
    fn strip_mods_combined() {
        let (line, mods) = strip_mods(
            "Rysk slashes a goblin for 5000 points of damage. (Lucky Critical Twincast)",
        );
        assert_eq!(line, "Rysk slashes a goblin for 5000 points of damage.");
        assert_eq!(mods, MODS_LUCKY | MODS_CRIT | MODS_TWINCAST);
    }
    #[test]
    fn strip_mods_no_opening_paren() {
        let (_, mods) = strip_mods("Some line ending in word)");
        assert_eq!(mods, 0);
    }
    #[test]
    fn strip_mods_unknown_suffix_not_stripped() {
        // (Race) on a /who line must not be stripped — only recognised combat mods are.
        let (line, mods) = strip_mods("[65 Warrior] Rysk (Human)");
        assert_eq!(line, "[65 Warrior] Rysk (Human)");
        assert_eq!(mods, 0);
    }

    // ── Parser integration ────────────────────────────────────────────────────────

    const TS: &str = "[Tue Feb 27 22:00:07 2026] ";

    fn run_lines(lines: &[&str], player: &str) -> Arc<CombatState> {
        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        let shared = Arc::new(ArcSwap::from_pointee(CombatState::default()));
        let reset_flag = Arc::new(AtomicBool::new(false));
        let (broadcast_tx, _) = tokio::sync::broadcast::channel(16);
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        for &line in lines {
            tx.send(line.to_owned()).unwrap();
        }
        drop(tx);
        run(
            rx,
            Arc::clone(&shared),
            reset_flag,
            broadcast_tx,
            event_tx,
            player.to_owned(),
        );
        shared.load_full()
    }

    /// Collect the events a set of lines produces, so a non-combat emission
    /// can be asserted on directly.
    fn run_events(lines: &[&str], player: &str) -> Vec<CombatEvent> {
        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        let shared = Arc::new(ArcSwap::from_pointee(CombatState::default()));
        let reset_flag = Arc::new(AtomicBool::new(false));
        let (broadcast_tx, _) = tokio::sync::broadcast::channel(16);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        for &line in lines {
            tx.send(line.to_owned()).unwrap();
        }
        drop(tx);
        run(
            rx,
            shared,
            reset_flag,
            broadcast_tx,
            event_tx,
            player.to_owned(),
        );
        let mut out = Vec::new();
        while let Ok(e) = event_rx.try_recv() {
            out.push(e);
        }
        out
    }

    /// The whole path a raid macro takes: a line you could actually type in
    /// game becomes the event the server turns into a marker — and a
    /// guildmate wondering aloud when the raid starts does not.
    #[test]
    fn integration_chat_macro_marks_a_raid() {
        let marks: Vec<(u32, String)> = run_events(
            &[
                "[Sun Aug 02 20:00:00 2026] You say to your guild, 'raid start'",
                "[Sun Aug 02 20:00:05 2026] Zyro tells General:1, 'when does the raid start?'",
                "[Sun Aug 02 22:30:00 2026] Kermitzalot tells the raid, 'raid end'",
            ],
            "Izzin",
        )
        .into_iter()
        .filter_map(|e| match e {
            CombatEvent::RaidMark { ts, kind, .. } => Some((ts, kind)),
            _ => None,
        })
        .collect();
        assert_eq!(marks.len(), 2, "the question is not a marker: {marks:?}");
        assert_eq!(marks[0].1, "raid_start");
        assert_eq!(marks[1].1, "raid_end");
        assert!(marks[1].0 > marks[0].0, "end comes after start");
    }

    /// Instances divide the timeline; corpse-running back into the SAME one
    /// does not. Plain zones never cut.
    #[test]
    fn integration_instances_cut_once_each() {
        let marks: Vec<(String, String)> = run_events(
            &[
                "[Sun Aug 02 20:00:00 2026] You have entered The City of Guk 4 (Refined).",
                "[Sun Aug 02 20:30:00 2026] You have entered East Freeport.",
                "[Sun Aug 02 20:31:00 2026] You have entered The City of Guk 4 (Refined).",
                "[Sun Aug 02 21:00:00 2026] You have entered Befallen 4 (Refined).",
            ],
            "Izzin",
        )
        .into_iter()
        .filter_map(|e| match e {
            CombatEvent::RaidMark { kind, label, .. } => Some((kind, label)),
            _ => None,
        })
        .collect();
        assert_eq!(
            marks,
            vec![
                ("instance".to_string(), "The City of Guk 4".to_string()),
                ("instance".to_string(), "Befallen 4".to_string()),
            ],
            "one marker per DISTINCT instance; the sell-trip re-entry is silent"
        );
    }

    #[test]
    fn integration_pet_owner_from_burnout() {
        // Burnout cast + generated-name pet going berserk within the window
        // → ownership learned. (Real pair observed live: Ruin / Labarer.)
        let state = run_lines(
            &[
                "[Fri Feb 27 20:00:01 2026] Ruin begins casting Burnout.",
                "[Fri Feb 27 20:00:05 2026] Labarer goes berserk.",
            ],
            "Izzin",
        );
        assert_eq!(
            state.known_pets.get("Labarer").map(String::as_str),
            Some("Ruin")
        );
        assert!(state.known_players.contains("Labarer"));
    }

    #[test]
    fn integration_pet_owner_window_and_name_gates() {
        // Too late after the cast → no association.
        let late = run_lines(
            &[
                "[Fri Feb 27 20:00:01 2026] Ruin begins casting Burnout.",
                "[Fri Feb 27 20:00:30 2026] Labarer goes berserk.",
            ],
            "Izzin",
        );
        assert!(!late.known_pets.contains_key("Labarer"));
        // Non-generated name (a player named who-knows-what) → no association.
        let notpet = run_lines(
            &[
                "[Fri Feb 27 20:00:01 2026] Ruin begins casting Burnout.",
                "[Fri Feb 27 20:00:03 2026] Steve goes berserk.",
            ],
            "Izzin",
        );
        assert!(!notpet.known_pets.contains_key("Steve"));
    }

    #[test]
    fn integration_pet_owner_from_summon() {
        // "Lesser Summoning: Water" then a brand-new generated-name pet's
        // first swing → owned by the summoner (no Burnout needed).
        let state = run_lines(
            &[
                "[Fri Feb 27 20:00:01 2026] Marrowbane begins casting Lesser Summoning: Water.",
                "[Fri Feb 27 20:00:20 2026] Gobaner slashes a gnoll for 12 points of damage.",
            ],
            "Izzin",
        );
        assert_eq!(
            state.known_pets.get("Gobaner").map(String::as_str),
            Some("Marrowbane")
        );
        // An already-associated pet is NOT re-owned by someone else's summon.
        let state2 = run_lines(
            &[
                "[Fri Feb 27 20:00:01 2026] Ruin begins casting Burnout.",
                "[Fri Feb 27 20:00:03 2026] Labarer goes berserk.",
                "[Fri Feb 27 20:00:10 2026] Marrowbane begins casting Lesser Summoning: Water.",
                "[Fri Feb 27 20:00:15 2026] Labarer slashes a gnoll for 12 points of damage.",
            ],
            "Izzin",
        );
        assert_eq!(
            state2.known_pets.get("Labarer").map(String::as_str),
            Some("Ruin")
        );
    }

    #[test]
    fn integration_melee_player_to_mob() {
        let state = run_lines(
            &[&format!(
                "{TS}Rysk slashes a goblin for 150 points of damage."
            )],
            "Rysk",
        );
        let stats = state.entities.get("Rysk").unwrap();
        assert_eq!(stats.total_damage, 150);
        assert_eq!(stats.damage_by_type.get("slash"), Some(&150));
        assert!(state.known_players.contains("Rysk"));
        assert!(state.confirmed_mobs.contains("a goblin"));
    }

    #[test]
    fn integration_melee_mob_to_player() {
        let state = run_lines(
            &[&format!("{TS}a goblin hits Rysk for 80 points of damage.")],
            "Rysk",
        );
        assert!(state.confirmed_mobs.contains("a goblin"));
        let mob_id = state
            .mob_list
            .iter()
            .find(|m| m.name == "a goblin")
            .unwrap()
            .id;
        let tanking = state.mob_tanking.get(&mob_id).unwrap();
        assert_eq!(tanking.get("Rysk").unwrap().total_damage, 80);
    }

    #[test]
    fn integration_hit_by_spell_player_to_mob() {
        let state = run_lines(
            &[&format!(
                "{TS}Rysk hit a goblin for 500 points of magic damage by Fireball."
            )],
            "Rysk",
        );
        let stats = state.entities.get("Rysk").unwrap();
        assert_eq!(stats.total_damage, 500);
        assert_eq!(stats.damage_by_spell.get("Fireball"), Some(&500));
    }

    #[test]
    fn integration_hit_by_spell_mob_to_player() {
        let state = run_lines(
            &[&format!(
                "{TS}an orc hit Rysk for 200 points of fire damage by Scorchblast."
            )],
            "Rysk",
        );
        assert!(state.confirmed_mobs.contains("an orc"));
        let mob_id = state
            .mob_list
            .iter()
            .find(|m| m.name == "an orc")
            .unwrap()
            .id;
        let tanking = state.mob_tanking.get(&mob_id).unwrap();
        assert_eq!(tanking.get("Rysk").unwrap().total_damage, 200);
    }

    #[test]
    fn integration_dot_tick() {
        let state = run_lines(
            &[&format!(
                "{TS}a goblin has been damaged by Rysk's Envenomed Bolt for 150 damage."
            )],
            "Rysk",
        );
        let stats = state.entities.get("Rysk").unwrap();
        assert_eq!(stats.total_damage, 150);
        assert_eq!(stats.damage_by_spell.get("Envenomed Bolt"), Some(&150));
        assert_eq!(stats.damage_by_type.get("dot"), Some(&150));
    }

    #[test]
    fn integration_crit_mods_increments_counters() {
        let state = run_lines(
            &[&format!(
                "{TS}Rysk slashes a goblin for 5000 points of damage. (Lucky Critical Twincast)"
            )],
            "Rysk",
        );
        let stats = state.entities.get("Rysk").unwrap();
        assert_eq!(stats.crit_count, 1);
        assert_eq!(stats.twincast_count, 1);
    }

    #[test]
    fn integration_kill_you_have_slain() {
        let state = run_lines(
            &[
                &format!("{TS}Rysk slashes a goblin for 150 points of damage."),
                &format!("{TS}You have slain a goblin!"),
            ],
            "Rysk",
        );
        assert!(state.dead_mobs.contains("a goblin"));
        assert!(state.fight_end.is_some());
    }

    #[test]
    fn integration_kill_x_has_slain() {
        let state = run_lines(
            &[
                &format!("{TS}Rysk slashes a skeleton for 100 points of damage."),
                &format!("{TS}Rysk has slain a skeleton!"),
            ],
            "Rysk",
        );
        assert!(state.dead_mobs.contains("a skeleton"));
    }

    #[test]
    fn integration_kill_slain_by() {
        let state = run_lines(
            &[
                &format!("{TS}Rysk slashes a skeleton for 100 points of damage."),
                &format!("{TS}a skeleton was slain by Rysk!"),
            ],
            "Rysk",
        );
        assert!(state.dead_mobs.contains("a skeleton"));
    }

    #[test]
    fn integration_kill_died() {
        let state = run_lines(
            &[
                &format!("{TS}Rysk slashes a skeleton for 100 points of damage."),
                &format!("{TS}a skeleton died."),
            ],
            "Rysk",
        );
        assert!(state.dead_mobs.contains("a skeleton"));
    }

    #[test]
    fn integration_article_normalized_on_kill() {
        // "A goblin" at sentence start → should still hit dead_mobs["a goblin"]
        let state = run_lines(
            &[
                &format!("{TS}Rysk slashes a goblin for 150 points of damage."),
                &format!("{TS}A goblin was slain by Rysk!"),
            ],
            "Rysk",
        );
        assert!(state.dead_mobs.contains("a goblin"));
    }

    #[test]
    fn integration_sentence_start_case_single_mob_instance() {
        // Article-less mobs (EQ Legends: "orc legionnaire") capitalize the
        // NAME itself at sentence start. Incoming ("Orc legionnaire hits
        // YOU") and outgoing ("You slash orc legionnaire") lines must map to
        // ONE mob instance, not two — this was the "I fight one mob but the
        // viewer shows 2" bug, which also split damage vs tanking across the
        // two phantom instances.
        let state = run_lines(
            &[
                &format!("{TS}Orc legionnaire hits YOU for 34 points of damage."),
                &format!("{TS}You slash Orc legionnaire for 450 points of damage."),
                &format!("{TS}You slash orc legionnaire for 450 points of damage."),
            ],
            "Rysk",
        );
        assert_eq!(
            state.mob_list.len(),
            1,
            "case variants must share one instance"
        );
        // Mid-sentence lowercase form wins as the display name.
        assert_eq!(state.mob_list[0].name, "orc legionnaire");
    }

    #[test]
    fn integration_log_time_gap_new_instance() {
        // Encounter windows are LOG-time based: 20 log-seconds between hits on
        // a same-named mob is a new instance even when the lines are parsed
        // back-to-back (imports/replays), so the same log always yields the
        // same encounters.
        let ts1 = "[Fri Feb 27 22:00:07 2026] ";
        let ts2 = "[Fri Feb 27 22:00:27 2026] ";
        let state = run_lines(
            &[
                &format!("{ts1}Rysk slashes a goblin for 150 points of damage."),
                &format!("{ts2}Rysk slashes a goblin for 150 points of damage."),
            ],
            "Rysk",
        );
        assert_eq!(state.mob_list.len(), 2);
    }

    #[test]
    fn integration_log_time_within_gap_same_instance() {
        let ts1 = "[Fri Feb 27 22:00:07 2026] ";
        let ts2 = "[Fri Feb 27 22:00:12 2026] "; // 5 log-seconds later
        let state = run_lines(
            &[
                &format!("{ts1}Rysk slashes a goblin for 150 points of damage."),
                &format!("{ts2}Rysk slashes a goblin for 150 points of damage."),
            ],
            "Rysk",
        );
        assert_eq!(state.mob_list.len(), 1);
    }

    #[test]
    fn integration_player_miss_parses() {
        // "You try to slash X, but miss!" — the player-side miss form.
        let state = run_lines(
            &[
                &format!("{TS}You try to slash royal guard, but miss!"),
                &format!("{TS}You try to kick an orc thaumaturgist, but miss!"),
            ],
            "Rysk",
        );
        let rysk = state.entities.get("royal guard");
        assert!(rysk.is_some(), "miss must create defender avoidance entry");
    }

    #[test]
    fn integration_inbound_ds_burn_counts_as_tanking() {
        let state = run_lines(
            &[
                &format!("{TS}You slash orc centurion for 100 points of damage."),
                &format!("{TS}YOU are burned by orc centurion's flames for 6 points of non-melee damage!"),
            ],
            "Rysk",
        );
        assert_eq!(
            state.mob_list.len(),
            1,
            "burn attributes to the same instance"
        );
        let tank = state
            .mob_tanking
            .get(&state.mob_list[0].id)
            .and_then(|m| m.get("Rysk"));
        assert_eq!(tank.map(|t| t.total_damage), Some(6));
    }

    #[test]
    fn integration_mez_parks_and_suspends_gap() {
        // Mez lands, then the mob sits idle for 20+ log-seconds while the
        // group kills something else — the parked instance must NOT split
        // into a new one when it finally acts (mez suspends the gap), and a
        // mez on a never-engaged add must create its pull membership.
        let ts1 = "[Fri Feb 27 22:00:07 2026] ";
        let ts2 = "[Fri Feb 27 22:00:37 2026] "; // 30s later — beyond the gap
        let state = run_lines(
            &[
                &format!("{ts1}orc centurion has been mesmerized."),
                &format!("{ts2}Orc centurion has been awakened by Rysk!"),
                &format!("{ts2}You slash orc centurion for 100 points of damage."),
            ],
            "Rysk",
        );
        assert_eq!(state.mob_list.len(), 1, "mez + wake + hit = one instance");
        assert!(
            !state.mob_list[0].parked,
            "awakened mob is no longer parked"
        );
    }

    #[test]
    fn integration_mez_stays_parked_until_woken() {
        let state = run_lines(
            &[&format!("{TS}a Tesch Mas Gnoll has been enthralled.")],
            "Rysk",
        );
        assert_eq!(state.mob_list.len(), 1);
        assert!(state.mob_list[0].parked);
    }

    #[test]
    fn integration_mob_self_heal_tracked_not_emitted() {
        // Mob self-heal keeps the encounter window open but must not appear
        // as a player healer.
        let state = run_lines(
            &[
                &format!("{TS}Rysk slashes an orc thaumaturgist for 150 points of damage."),
                &format!("{TS}an orc thaumaturgist healed itself for 11 hit points by Lifespike."),
            ],
            "Rysk",
        );
        assert_eq!(state.mob_list.len(), 1);
        let healer = state.entities.get("an orc thaumaturgist");
        assert!(
            healer.is_none_or(|e| e.total_heals == 0),
            "mob self-heal must not credit healer stats"
        );
    }

    #[test]
    fn integration_sentence_start_case_slay_matches() {
        // A slay line with sentence-start capitalization must mark the
        // lowercase-tracked mob dead (dead_mobs keys are lowercased).
        let state = run_lines(
            &[
                &format!("{TS}You slash orc legionnaire for 450 points of damage."),
                &format!("{TS}Orc legionnaire was slain by Rysk!"),
            ],
            "Rysk",
        );
        assert!(state.dead_mobs.contains("orc legionnaire"));
    }

    #[test]
    fn integration_heal_tracked() {
        let state = run_lines(
            &[
                &format!("{TS}Rysk slashes a goblin for 150 points of damage."),
                &format!("{TS}Healer healed Rysk for 1500 hit points by Complete Heal."),
            ],
            "Rysk",
        );
        let healer = state.entities.get("Healer").unwrap();
        assert_eq!(healer.total_heals, 1500);
        assert_eq!(healer.heals_by_spell.get("Complete Heal"), Some(&1500));
        let healee = state.entities.get("Rysk").unwrap();
        assert_eq!(healee.total_healed_received, 1500);
    }

    #[test]
    fn integration_who_populates_classes() {
        let state = run_lines(&[&format!("{TS}[65 Warrior] Rysk (Human)")], "Rysk");
        assert_eq!(
            state.player_classes.get("Rysk"),
            Some(&vec!["WAR".to_owned()])
        );
    }

    #[test]
    fn integration_mob_name_set() {
        let state = run_lines(
            &[&format!(
                "{TS}Rysk slashes a goblin for 150 points of damage."
            )],
            "Rysk",
        );
        assert_eq!(state.mob_name, "a goblin");
    }

    #[test]
    fn integration_riposte_player() {
        let state = run_lines(
            &[&format!(
                "{TS}a skeleton was injured by Rysk's riposte for 200 damage."
            )],
            "Rysk",
        );
        let stats = state.entities.get("Rysk").unwrap();
        assert_eq!(stats.total_damage, 200);
        assert_eq!(stats.damage_by_type.get("riposte"), Some(&200));
    }

    #[test]
    fn integration_ds_player() {
        let state = run_lines(
            &[&format!(
                "{TS}a goblin was struck by Rysk's damage shield for 40 damage."
            )],
            "Rysk",
        );
        let stats = state.entities.get("Rysk").unwrap();
        assert_eq!(stats.total_damage, 40);
        assert_eq!(stats.damage_by_type.get("ds"), Some(&40));
    }

    #[test]
    fn integration_ds_proc_your() {
        let state = run_lines(
            &[&format!(
                "{TS}a goblin is burned by YOUR flames for 12 points of non-melee damage."
            )],
            "Rysk",
        );
        let stats = state.entities.get("Rysk").unwrap();
        assert_eq!(stats.total_damage, 12);
        assert_eq!(stats.damage_by_type.get("ds"), Some(&12));
    }

    #[test]
    fn integration_miss_avoidance() {
        let state = run_lines(
            &[&format!(
                "{TS}a skeleton tries to slash Rysk, but Rysk dodges!"
            )],
            "Rysk",
        );
        let stats = state.entities.get("Rysk").unwrap();
        assert_eq!(stats.avoidance_by_type.get("dodge"), Some(&1));
    }

    #[test]
    fn integration_cast_populates_active_casts() {
        // RE_CAST records the player and spell in active_casts, visible in JSON.
        // Note: "SpellName hit Target for N" is caught by RE_MELEE before RE_SPELL_HIT,
        // so unattributed spell attribution via the cast map doesn't fire for "hit" verbs.
        // Attribution for DD spells comes via RE_HIT_BY_SPELL instead.
        let state = run_lines(
            &[&format!("{TS}Rysk begins casting Complete Heal.")],
            "Rysk",
        );
        let json = state.to_api_json();
        let casting = json["casting"].as_object().unwrap();
        assert!(
            casting.contains_key("Rysk"),
            "active_casts should contain Rysk"
        );
        assert_eq!(casting["Rysk"]["spell"].as_str().unwrap(), "Complete Heal");
    }

    #[test]
    fn integration_login_emits_event() {
        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        let shared = Arc::new(ArcSwap::from_pointee(CombatState::default()));
        let reset_flag = Arc::new(AtomicBool::new(false));
        let (broadcast_tx, _) = tokio::sync::broadcast::channel(16);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(format!("{TS}Welcome to EverQuest Legends!"))
            .unwrap();
        drop(tx);
        run(
            rx,
            shared,
            reset_flag,
            broadcast_tx,
            event_tx,
            "Rysk".to_owned(),
        );
        let mut found_login = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, CombatEvent::Login { .. }) {
                found_login = true;
            }
        }
        assert!(
            found_login,
            "Login event should be emitted for the welcome message"
        );
    }

    #[test]
    fn integration_resist() {
        let state = run_lines(
            &[&format!("{TS}a goblin resisted your Shadowbolt!")],
            "Rysk",
        );
        let stats = state.entities.get("Rysk").unwrap();
        assert_eq!(stats.resists_by_spell.get("Shadowbolt"), Some(&1));
    }

    #[test]
    fn integration_damage_accumulates() {
        let state = run_lines(
            &[
                &format!("{TS}Rysk slashes a goblin for 100 points of damage."),
                &format!("{TS}Rysk slashes a goblin for 200 points of damage."),
                &format!("{TS}Rysk slashes a goblin for 300 points of damage."),
            ],
            "Rysk",
        );
        assert_eq!(state.total_damage(), 600);
    }

    #[test]
    fn integration_mob_list_tracks_mob() {
        let state = run_lines(
            &[&format!(
                "{TS}Rysk slashes a goblin for 150 points of damage."
            )],
            "Rysk",
        );
        assert_eq!(state.mob_list.len(), 1);
        assert_eq!(state.mob_list[0].name, "a goblin");
    }

    #[test]
    fn integration_mob_damage_breakdown() {
        let state = run_lines(
            &[&format!(
                "{TS}Rysk slashes a goblin for 150 points of damage."
            )],
            "Rysk",
        );
        let mob_id = state.mob_list[0].id;
        let by_player = state.mob_damage.get(&mob_id).unwrap();
        assert_eq!(by_player.get("Rysk").unwrap().total_damage, 150);
    }

    #[test]
    fn integration_reset_clears_combat() {
        use std::sync::atomic::AtomicBool;
        use std::thread;
        use std::time::Duration;

        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        let shared = Arc::new(ArcSwap::from_pointee(CombatState::default()));
        let reset_flag = Arc::new(AtomicBool::new(false));
        let reset_flag2 = Arc::clone(&reset_flag);
        let shared2 = Arc::clone(&shared);
        let (broadcast_tx, _) = tokio::sync::broadcast::channel(16);
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();

        tx.send(format!(
            "{TS}Rysk slashes a goblin for 150 points of damage."
        ))
        .unwrap();
        let tx2 = tx.clone();
        thread::spawn(move || {
            // The parser processes one line in microseconds; 20ms is a wide margin.
            thread::sleep(Duration::from_millis(20));
            reset_flag2.store(true, Ordering::Relaxed);
            tx2.send(format!(
                "{TS}Rysk slashes a skeleton for 99 points of damage."
            ))
            .unwrap();
            drop(tx2);
        });
        drop(tx);

        run(
            rx,
            Arc::clone(&shared),
            reset_flag,
            broadcast_tx,
            event_tx,
            "Rysk".to_owned(),
        );
        let state = shared2.load_full();
        // After reset: only the skeleton hit is present (99 dmg, not 150+99)
        assert_eq!(state.total_damage(), 99);
    }

    #[test]
    fn integration_player_classes_survive_reset() {
        use std::sync::atomic::AtomicBool;
        use std::thread;
        use std::time::Duration;

        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        let shared = Arc::new(ArcSwap::from_pointee(CombatState::default()));
        let reset_flag = Arc::new(AtomicBool::new(false));
        let reset_flag2 = Arc::clone(&reset_flag);
        let shared2 = Arc::clone(&shared);
        let (broadcast_tx, _) = tokio::sync::broadcast::channel(16);
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();

        tx.send(format!("{TS}[65 Warrior] Rysk (Human)")).unwrap();
        let tx2 = tx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            reset_flag2.store(true, Ordering::Relaxed);
            tx2.send(format!(
                "{TS}Rysk slashes a skeleton for 50 points of damage."
            ))
            .unwrap();
            drop(tx2);
        });
        drop(tx);

        run(
            rx,
            Arc::clone(&shared),
            reset_flag,
            broadcast_tx,
            event_tx,
            "Rysk".to_owned(),
        );
        let state = shared2.load_full();
        // player_classes must survive the reset
        assert_eq!(
            state.player_classes.get("Rysk"),
            Some(&vec!["WAR".to_owned()])
        );
    }
}
