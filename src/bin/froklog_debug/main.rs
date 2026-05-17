/// froklog-debug — Line-by-line parser trace tool.
///
/// Reads an EverQuest log file and processes it exactly as the production
/// parser does, printing a human-readable explanation of every decision:
/// which regex matched, whether a new mob/player was created, what damage
/// or heal was attributed, how state changed.
///
/// Usage:
///   froklog-debug --log logs/eqlog_Icestorm_test.txt --player Icestorm
///   froklog-debug --log logs/eqlog_Icestorm_test.txt --player Icestorm --only-matched
///   froklog-debug --log logs/eqlog_Icestorm_test.txt --player Icestorm --filter mob
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

use chrono::Timelike;
use clap::Parser;
use froklog::patterns::{
    norm, normalize_article_case, normalize_miss, normalize_verb, parse_who_classes,
    RE_ABSORB_RUNE, RE_ABSORB_SKIN, RE_CAST, RE_DIED, RE_DOT, RE_DS, RE_DS_PROC, RE_EXTRA_DMG,
    RE_HAS_TAKEN, RE_HEAL, RE_HIT_BY_SPELL, RE_MELEE, RE_MISS, RE_RESIST, RE_RIPOSTE, RE_SLAIN_BY,
    RE_SLAY_HAS, RE_SLAY_YOU, RE_SPELL_ATTR, RE_SPELL_HIT, RE_WHO, TS_LEN,
};
use froklog::tailer::parse_eq_timestamp;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "froklog-debug",
    about = "Process an EQ log line-by-line and trace every parser decision"
)]
struct Args {
    /// Path to the EverQuest log file
    #[arg(short, long)]
    log: String,

    /// Your character name (replaces \"you\" / \"yourself\" etc.)
    #[arg(short, long, default_value = "")]
    player: String,

    /// Only print lines that matched a combat pattern (skip unrecognised lines)
    #[arg(long)]
    only_matched: bool,

    /// Filter output: comma-separated keywords to require in the trace line
    /// (e.g. \"mob\", \"heal\", \"spell\", \"slay\", \"cast\", \"dot\", \"ds\", \"rip\")
    #[arg(long, default_value = "")]
    filter: String,

    /// Pause between lines (milliseconds). 0 = no pause.
    #[arg(long, default_value = "0")]
    delay_ms: u64,

    /// Stop after this many lines (0 = no limit)
    #[arg(long, default_value = "0")]
    max_lines: u64,
}

// ── In-memory state (mirrors CombatState, simplified) ─────────────────────────

#[derive(Default)]
struct DebugState {
    lines_parsed: u64,

    /// player name → stats (includes mob healers; use known_players for classification)
    entities: HashMap<String, EntityStats>,
    /// Names confirmed as players by dealing player-attributed damage.
    known_players: HashSet<String>,
    /// mob name → mob instance list
    mob_list: Vec<MobInstance>,
    next_mob_id: u64,
    active_mob_id: Option<u64>,

    /// mob name → true
    confirmed_mobs: HashSet<String>,
    dead_mobs: HashSet<String>,

    /// mob_id → player → damage dealt
    mob_damage: HashMap<u64, HashMap<String, EntityStats>>,
    /// mob_id → player → tanking damage received
    mob_tanking: HashMap<u64, HashMap<String, EntityStats>>,
    /// mob_id → healer → heals cast (mirrors production EntityCombatStats)
    mob_healing: HashMap<u64, HashMap<String, EntityStats>>,
    /// mob_id → heal target → heals received
    mob_healed: HashMap<u64, HashMap<String, EntityStats>>,

    fight_started: bool,
    mob_name: String,

    /// spell name → caster name
    spell_caster: HashMap<String, String>,
    /// mob name → set of players who attacked it
    mob_candidates: HashMap<String, HashSet<String>>,
    /// player name → 1–3 class short-codes from /who lines
    player_classes: HashMap<String, Vec<String>>,
}

#[derive(Default)]
struct EntityStats {
    total_damage: u64,
    damage_by_type: HashMap<String, u64>,
    damage_by_spell: HashMap<String, u64>,
    total_heals: u64,
    heals_by_spell: HashMap<String, u64>,
    total_healed_received: u64,
    healed_received_by_spell: HashMap<String, u64>,
    avoidance_by_type: HashMap<String, u64>,
    resists_by_spell: HashMap<String, u64>,
}

struct MobInstance {
    id: u64,
    name: String,
    last_seen: Instant,
}

// ── Display helpers ───────────────────────────────────────────────────────────

fn update_mob_list(state: &mut DebugState, tgt: &str) -> (u64, bool) {
    // Corpses ("X's corpse") are dead mobs and must never enter the mob list.
    if tgt.ends_with("'s corpse") {
        return (state.active_mob_id.unwrap_or(u64::MAX), false);
    }
    // Use known_players (damage-dealers only) to decide if tgt is a player.
    // entities is also populated by mob healers and cannot be used here.
    if state.known_players.contains(tgt) {
        state.mob_list.retain(|m| m.name != tgt);
        return (state.active_mob_id.unwrap_or(u64::MAX), false);
    }

    const GAP: Duration = Duration::from_secs(15);
    let now = Instant::now();

    // Mirror production: a previously-dead mob always starts a fresh instance,
    // even if it re-appears within the 15-second gap window.
    let was_dead = state.dead_mobs.remove(tgt);

    let (id, is_new) = if !was_dead {
        if let Some(s) = state
            .mob_list
            .iter_mut()
            .find(|m| m.name == tgt && now.duration_since(m.last_seen) < GAP)
        {
            s.last_seen = now;
            (s.id, false)
        } else {
            let id = state.next_mob_id;
            state.next_mob_id += 1;
            state.mob_list.push(MobInstance {
                id,
                name: tgt.to_owned(),
                last_seen: now,
            });
            (id, true)
        }
    } else {
        let id = state.next_mob_id;
        state.next_mob_id += 1;
        state.mob_list.push(MobInstance {
            id,
            name: tgt.to_owned(),
            last_seen: now,
        });
        (id, true)
    };

    state
        .mob_list
        .sort_unstable_by_key(|b| std::cmp::Reverse(b.last_seen));
    state.active_mob_id = Some(id);
    (id, is_new)
}

fn track_mob_candidate(state: &mut DebugState, tgt: String, src: &str) -> bool {
    state
        .mob_candidates
        .entry(tgt)
        .or_default()
        .insert(src.to_owned())
}

fn update_mob_name_and_confirmed(state: &mut DebugState) -> Vec<String> {
    let mut newly_confirmed = Vec::new();
    let candidates: Vec<String> = state.mob_candidates.keys().cloned().collect();
    // Use known_players (damage-dealers only) — entities is also populated by mob
    // healers and would incorrectly de-confirm mobs that have healed.
    for tgt in &candidates {
        if !state.known_players.contains(tgt.as_str()) {
            if state.confirmed_mobs.insert(tgt.clone()) {
                newly_confirmed.push(tgt.clone());
            }
        } else {
            state.confirmed_mobs.remove(tgt.as_str());
            state.mob_list.retain(|m| m.name != tgt.as_str());
        }
    }

    let best = state
        .mob_candidates
        .iter()
        .filter(|(tgt, _)| !state.known_players.contains(tgt.as_str()))
        .max_by_key(|(_, srcs)| srcs.len());

    if let Some((name, _)) = best {
        state.mob_name = name.clone();
    }
    newly_confirmed
}

// ── Trace output helpers ──────────────────────────────────────────────────────

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    let filter_words: Vec<String> = if args.filter.is_empty() {
        vec![]
    } else {
        args.filter
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    };

    let mut state = DebugState::default();

    let file = File::open(&args.log).unwrap_or_else(|e| {
        eprintln!("ERROR: cannot open {:?}: {e}", args.log);
        std::process::exit(1);
    });

    let mut line_no: u64 = 0;
    let mut ts_display = String::from("(no timestamp)");

    for raw_line in BufReader::new(file).lines() {
        let raw_line = match raw_line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ERROR reading line {line_no}: {e}");
                continue;
            }
        };

        line_no += 1;
        state.lines_parsed += 1;

        if args.max_lines > 0 && line_no > args.max_lines {
            println!("\n[stopped after {} lines]", args.max_lines);
            break;
        }

        // Strip the EQ timestamp prefix.
        let line = if raw_line.len() > TS_LEN && raw_line.starts_with('[') {
            // Parse timestamp for display.
            if let Some(dt) = parse_eq_timestamp(&raw_line) {
                ts_display = format!(
                    "{} {:02}:{:02}:{:02}",
                    dt.format("%a %b %-d %Y"),
                    dt.hour(),
                    dt.minute(),
                    dt.second()
                );
            }
            &raw_line[TS_LEN..]
        } else {
            raw_line.as_str()
        };

        // We'll buffer trace lines so we can apply --filter and --only-matched.
        let mut trace_lines: Vec<String> = Vec::new();
        let mut matched = false;
        let mut event_desc = String::new();

        // A closure to buffer trace output.
        macro_rules! t {
            ($label:expr, $msg:expr) => {
                trace_lines.push(format!("║  {:<8}: {}", $label, $msg));
            };
        }

        // ── RE_HIT_BY_SPELL ────────────────────────────────────────────────────
        if let Some(caps) = RE_HIT_BY_SPELL.captures(line) {
            let src = norm(caps["src"].trim(), &args.player);
            let tgt = norm(caps["tgt"].trim(), &args.player);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);
            let spell = caps["spell"].to_owned();

            t!(
                "MATCH",
                format!("RE_HIT_BY_SPELL — src={src:?} tgt={tgt:?} dmg={dmg} spell={spell:?}")
            );

            let src_is_mob = src.contains(' ')
                || state.confirmed_mobs.contains(&src)
                || state.known_players.contains(tgt.as_str())
                || tgt == args.player
                || state.mob_candidates.contains_key(src.as_str());

            if src_is_mob {
                t!("REASON", format!("{src:?} identified as MOB (multi-word={}, confirmed={}, tgt_is_player={}, tgt_is_self={}, src_is_candidate={})",
                    src.contains(' '), state.confirmed_mobs.contains(&src),
                    state.known_players.contains(tgt.as_str()), tgt == args.player,
                    state.mob_candidates.contains_key(src.as_str())));
                if !state.known_players.contains(&src) {
                    let was_new_mob = state.confirmed_mobs.insert(src.clone());
                    if was_new_mob {
                        t!("NEW", format!("mob confirmed: {src:?}"));
                    }
                } else {
                    t!(
                        "INFO",
                        format!("{src:?} is in known_players — skipping confirmed_mobs.insert")
                    );
                }
                let (mob_id, is_new_instance) = update_mob_list(&mut state, &src);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {src:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({src:?}) last_seen refreshed")
                    );
                }
                let tank_stats = state
                    .mob_tanking
                    .entry(mob_id)
                    .or_default()
                    .entry(tgt.clone())
                    .or_default();
                tank_stats.total_damage += dmg;
                *tank_stats
                    .damage_by_type
                    .entry("hit".to_owned())
                    .or_default() += dmg;
                t!(
                    "STORE",
                    format!("mob_tanking[#{mob_id}][{tgt:?}].total_damage += {dmg} (hit)")
                );
                event_desc = format!("Spell(tank) mob={src:?} → player={tgt:?} dmg={dmg} spell={spell:?} mob_id=#{mob_id}");
            } else {
                t!("REASON", format!("{src:?} identified as PLAYER"));
                let is_new_player = !state.entities.contains_key(&src);
                let stats = state.entities.entry(src.clone()).or_default();
                if is_new_player {
                    t!("NEW", format!("player entity created: {src:?}"));
                }
                state.known_players.insert(src.clone());
                stats.total_damage += dmg;
                *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                t!(
                    "STORE",
                    format!("entities[{src:?}].total_damage += {dmg} (spell={spell:?})")
                );

                let is_new_candidate = track_mob_candidate(&mut state, tgt.clone(), &src);
                if is_new_candidate {
                    t!(
                        "TRACK",
                        format!("{tgt:?} tracked as mob candidate (attacked by {src:?})")
                    );
                }
                let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                    );
                }
                let mob_p = state
                    .mob_damage
                    .entry(mob_id)
                    .or_default()
                    .entry(src.clone())
                    .or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                t!(
                    "STORE",
                    format!(
                        "mob_damage[#{mob_id}][{src:?}].total_damage += {dmg} (spell={spell:?})"
                    )
                );
                event_desc = format!("Spell(dmg) player={src:?} → mob={tgt:?} dmg={dmg} spell={spell:?} mob_id=#{mob_id}");
            }
            if !state.fight_started {
                state.fight_started = true;
                t!("STATE", "fight_start set");
            }
            let newly = update_mob_name_and_confirmed(&mut state);
            for m in &newly {
                t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
            }
            matched = true;

        // -- RE_MELEE
        } else if let Some(caps) = RE_MELEE.captures(line) {
            let src = norm(caps["src"].trim(), &args.player);
            let verb = caps["verb"].to_owned();
            let tgt = norm(&caps["tgt"], &args.player);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);
            let typ = normalize_verb(&verb).to_owned();

            t!(
                "MATCH",
                format!("RE_MELEE — src={src:?} verb={verb:?} tgt={tgt:?} dmg={dmg} type={typ:?}")
            );

            let src_is_mob = !state.known_players.contains(&src)
                && (verb == "hit"
                    || verb == "hits"
                    || src.contains(' ')
                    || state.confirmed_mobs.contains(&src)
                    || state.mob_candidates.contains_key(src.as_str())
                    || state.known_players.contains(tgt.as_str())
                    || tgt == args.player);

            if src_is_mob {
                t!("REASON", format!("{src:?} identified as MOB (verb={verb:?}, multi-word={}, confirmed={}, is_candidate={}, tgt_is_player={}, player_override=false)",
                    src.contains(' '), state.confirmed_mobs.contains(&src),
                    state.mob_candidates.contains_key(src.as_str()),
                    state.known_players.contains(tgt.as_str())));
                let (mob_id, is_new_instance) = update_mob_list(&mut state, &src);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {src:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({src:?}) last_seen refreshed")
                    );
                }
                let tank_stats = state
                    .mob_tanking
                    .entry(mob_id)
                    .or_default()
                    .entry(tgt.clone())
                    .or_default();
                tank_stats.total_damage += dmg;
                *tank_stats.damage_by_type.entry(typ.clone()).or_default() += dmg;
                t!(
                    "STORE",
                    format!("mob_tanking[#{mob_id}][{tgt:?}].total_damage += {dmg} ({typ})")
                );
                if !state.known_players.contains(&src) {
                    let was_new_confirmed = state.confirmed_mobs.insert(src.clone());
                    if was_new_confirmed {
                        t!("CONFIRM", format!("mob confirmed: {src:?}"));
                    }
                } else {
                    t!(
                        "INFO",
                        format!("{src:?} is in known_players — skipping confirmed_mobs.insert")
                    );
                }
                event_desc = format!("Melee(tank) mob={src:?} → player={tgt:?} dmg={dmg} type={typ:?} mob_id=#{mob_id}");
            } else {
                t!(
                    "REASON",
                    format!("{src:?} identified as PLAYER (verb={verb:?})")
                );
                let is_new_player = !state.entities.contains_key(&src);
                let stats = state.entities.entry(src.clone()).or_default();
                if is_new_player {
                    t!("NEW", format!("player entity created: {src:?}"));
                }
                state.known_players.insert(src.clone());
                stats.total_damage += dmg;
                *stats.damage_by_type.entry(typ.clone()).or_default() += dmg;
                t!(
                    "STORE",
                    format!("entities[{src:?}].total_damage += {dmg} ({typ})")
                );

                let is_new_candidate = track_mob_candidate(&mut state, tgt.clone(), &src);
                if is_new_candidate {
                    t!(
                        "TRACK",
                        format!("{tgt:?} tracked as mob candidate (attacked by {src:?})")
                    );
                }
                let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                    );
                }
                let mob_p = state
                    .mob_damage
                    .entry(mob_id)
                    .or_default()
                    .entry(src.clone())
                    .or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_type.entry(typ.clone()).or_default() += dmg;
                t!(
                    "STORE",
                    format!("mob_damage[#{mob_id}][{src:?}].total_damage += {dmg} ({typ})")
                );
                event_desc = format!("Melee(dmg) player={src:?} → mob={tgt:?} dmg={dmg} type={typ:?} mob_id=#{mob_id}");
            }
            if !state.fight_started {
                state.fight_started = true;
                t!("STATE", "fight_start set");
            }
            let newly = update_mob_name_and_confirmed(&mut state);
            for m in &newly {
                t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
            }
            matched = true;

        // ── RE_SPELL_ATTR ─────────────────────────────────────────────────────
        } else if let Some(caps) = RE_SPELL_ATTR.captures(line) {
            let raw_src = norm(&caps["src"], &args.player);
            let raw_spell = caps["spell"].to_owned();
            let tgt = norm(&caps["tgt"], &args.player);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            t!("MATCH", format!("RE_SPELL_ATTR — raw_src={raw_src:?} raw_spell={raw_spell:?} tgt={tgt:?} dmg={dmg}"));

            // Guard against phantom players from spell-name prefixes.
            // "Denon's Disruptive Discord hit Mob for X" → raw_src="Denon", raw_spell="Disruptive Discord"
            // Only attribute if the combined possessive is in spell_caster, or raw_src is a known player.
            let combined = format!("{}'s {}", raw_src, raw_spell);
            let real_caster_opt = state.spell_caster.get(&combined).cloned();
            let is_attributed = real_caster_opt.is_some() || state.known_players.contains(&raw_src);

            if is_attributed {
                let (src, spell) = if let Some(real_caster) = real_caster_opt {
                    t!(
                        "LOOKUP",
                        format!("spell_caster[{combined:?}] = {real_caster:?} — using real caster")
                    );
                    (real_caster, combined)
                } else {
                    t!(
                        "REASON",
                        format!("{raw_src:?} is a known player — direct attribution")
                    );
                    (raw_src, raw_spell)
                };

                let is_new_player = !state.entities.contains_key(&src);
                let stats = state.entities.entry(src.clone()).or_default();
                if is_new_player {
                    t!("NEW", format!("player entity created: {src:?}"));
                }
                state.known_players.insert(src.clone());
                stats.total_damage += dmg;
                *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                t!(
                    "STORE",
                    format!("entities[{src:?}].total_damage += {dmg} (spell={spell:?})")
                );

                let is_new_candidate = track_mob_candidate(&mut state, tgt.clone(), &src);
                if is_new_candidate {
                    t!(
                        "TRACK",
                        format!("{tgt:?} tracked as mob candidate (proc from {src:?})")
                    );
                }
                let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                    );
                }
                let mob_p = state
                    .mob_damage
                    .entry(mob_id)
                    .or_default()
                    .entry(src.clone())
                    .or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                t!(
                    "STORE",
                    format!(
                        "mob_damage[#{mob_id}][{src:?}].total_damage += {dmg} (spell={spell:?})"
                    )
                );
                event_desc = format!("Spell(attr/proc) player={src:?} → mob={tgt:?} dmg={dmg} spell={spell:?} mob_id=#{mob_id}");
            } else {
                t!("SKIP", format!("not attributed — {raw_src:?} not a known player and {combined:?} not in spell_caster"));
                event_desc = format!("Spell(attr/unresolved) raw_src={raw_src:?} raw_spell={raw_spell:?} tgt={tgt:?}");
            }

            if !state.fight_started {
                state.fight_started = true;
                t!("STATE", "fight_start set");
            }
            let newly = update_mob_name_and_confirmed(&mut state);
            for m in &newly {
                t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
            }
            matched = true;

        // ── RE_SPELL_HIT ──────────────────────────────────────────────────────
        } else if let Some(caps) = RE_SPELL_HIT.captures(line) {
            let spell = caps["spell"].to_owned();
            let tgt = norm(&caps["tgt"], &args.player);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            t!(
                "MATCH",
                format!("RE_SPELL_HIT — spell={spell:?} tgt={tgt:?} dmg={dmg}")
            );

            if let Some(caster) = state.spell_caster.get(&spell).cloned() {
                t!("LOOKUP", format!("spell_caster[{spell:?}] = {caster:?}"));

                let caster_is_mob = caster.contains(' ') || state.confirmed_mobs.contains(&caster);

                if caster_is_mob {
                    t!(
                        "REASON",
                        format!(
                            "caster {caster:?} is a MOB — ignoring player target for mob tracking"
                        )
                    );
                    if !state.known_players.contains(&caster) {
                        let was_new = state.confirmed_mobs.insert(caster.clone());
                        if was_new {
                            t!("CONFIRM", format!("mob confirmed: {caster:?}"));
                        }
                    } else {
                        t!(
                            "INFO",
                            format!(
                                "{caster:?} is in known_players — skipping confirmed_mobs.insert"
                            )
                        );
                    }
                    event_desc =
                        format!("Spell(mob-cast/no-track) caster={caster:?} spell={spell:?}");
                } else {
                    t!(
                        "REASON",
                        format!("caster {caster:?} is a PLAYER — attributing damage")
                    );
                    let is_new_player = !state.entities.contains_key(&caster);
                    let stats = state.entities.entry(caster.clone()).or_default();
                    if is_new_player {
                        t!("NEW", format!("player entity created: {caster:?}"));
                    }
                    state.known_players.insert(caster.clone());
                    stats.total_damage += dmg;
                    *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    t!(
                        "STORE",
                        format!("entities[{caster:?}].total_damage += {dmg} (spell={spell:?})")
                    );

                    let is_new_candidate = track_mob_candidate(&mut state, tgt.clone(), &caster);
                    if is_new_candidate {
                        t!(
                            "TRACK",
                            format!(
                                "{tgt:?} tracked as mob candidate (hit by {caster:?}'s {spell:?})"
                            )
                        );
                    }
                    let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
                    if is_new_instance {
                        t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
                    } else {
                        t!(
                            "UPDATE",
                            format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                        );
                    }
                    let mob_p = state
                        .mob_damage
                        .entry(mob_id)
                        .or_default()
                        .entry(caster.clone())
                        .or_default();
                    mob_p.total_damage += dmg;
                    *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    t!("STORE", format!("mob_damage[#{mob_id}][{caster:?}].total_damage += {dmg} (spell={spell:?})"));
                    event_desc = format!("Spell(unattr) caster={caster:?} → mob={tgt:?} dmg={dmg} spell={spell:?} mob_id=#{mob_id}");
                }
                if !state.fight_started {
                    state.fight_started = true;
                    t!("STATE", "fight_start set");
                }
                let newly = update_mob_name_and_confirmed(&mut state);
                for m in &newly {
                    t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
                }
                matched = true;
            } else {
                t!(
                    "LOOKUP",
                    format!("spell_caster[{spell:?}] = (unknown — cannot attribute, skipping)")
                );
                // matched stays false
            }

        // ── RE_DOT ────────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_DOT.captures(line) {
            let src = norm(&caps["src"], &args.player);
            let spell = caps["spell"].to_owned();
            let tgt = norm(&caps["tgt"], &args.player);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            t!(
                "MATCH",
                format!("RE_DOT — src={src:?} spell={spell:?} tgt={tgt:?} dmg={dmg}")
            );

            let is_new_player = !state.entities.contains_key(&src);
            let stats = state.entities.entry(src.clone()).or_default();
            if is_new_player {
                t!("NEW", format!("player entity created: {src:?}"));
            }
            state.known_players.insert(src.clone());
            stats.total_damage += dmg;
            *stats.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
            *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            t!(
                "STORE",
                format!("entities[{src:?}].total_damage += {dmg} (dot / spell={spell:?})")
            );

            let is_new_candidate = track_mob_candidate(&mut state, tgt.clone(), &src);
            if is_new_candidate {
                t!(
                    "TRACK",
                    format!("{tgt:?} tracked as mob candidate (DoT'd by {src:?})")
                );
            }
            let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
            if is_new_instance {
                t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
            } else {
                t!(
                    "UPDATE",
                    format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                );
            }
            let mob_p = state
                .mob_damage
                .entry(mob_id)
                .or_default()
                .entry(src.clone())
                .or_default();
            mob_p.total_damage += dmg;
            *mob_p.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
            *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            t!(
                "STORE",
                format!(
                    "mob_damage[#{mob_id}][{src:?}].total_damage += {dmg} (dot / spell={spell:?})"
                )
            );

            if !state.fight_started {
                state.fight_started = true;
                t!("STATE", "fight_start set");
            }
            let newly = update_mob_name_and_confirmed(&mut state);
            for m in &newly {
                t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
            }
            event_desc =
                format!("Dot src={src:?} → mob={tgt:?} dmg={dmg} spell={spell:?} mob_id=#{mob_id}");
            matched = true;

        // ── RE_RIPOSTE ────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_RIPOSTE.captures(line) {
            let src = norm(&caps["src"], &args.player);
            let tgt = norm(&caps["tgt"], &args.player);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            t!(
                "MATCH",
                format!("RE_RIPOSTE — src={src:?} tgt={tgt:?} dmg={dmg}")
            );

            // A mob can riposte a player: "Player was injured by Mob's riposte for N"
            let src_is_mob = src.contains(' ')
                || state.confirmed_mobs.contains(&src)
                || state.known_players.contains(tgt.as_str())
                || tgt == args.player;

            if src_is_mob {
                t!(
                    "REASON",
                    format!("{src:?} identified as MOB riposting player")
                );
                if !state.known_players.contains(&src) {
                    let was_new = state.confirmed_mobs.insert(src.clone());
                    if was_new {
                        t!("CONFIRM", format!("mob confirmed: {src:?}"));
                    }
                }
                let (mob_id, is_new_instance) = update_mob_list(&mut state, &src);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {src:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({src:?}) last_seen refreshed")
                    );
                }
                let tank = state
                    .mob_tanking
                    .entry(mob_id)
                    .or_default()
                    .entry(tgt.clone())
                    .or_default();
                tank.total_damage += dmg;
                *tank.damage_by_type.entry("riposte".to_owned()).or_default() += dmg;
                t!(
                    "STORE",
                    format!("mob_tanking[#{mob_id}][{tgt:?}].total_damage += {dmg} (riposte)")
                );
                event_desc = format!(
                    "Riposte(tank) mob={src:?} → player={tgt:?} dmg={dmg} mob_id=#{mob_id}"
                );
            } else {
                t!(
                    "REASON",
                    format!("{src:?} identified as PLAYER riposting mob")
                );
                let is_new_player = !state.entities.contains_key(&src);
                let stats = state.entities.entry(src.clone()).or_default();
                if is_new_player {
                    t!("NEW", format!("player entity created: {src:?}"));
                }
                state.known_players.insert(src.clone());
                stats.total_damage += dmg;
                *stats
                    .damage_by_type
                    .entry("riposte".to_owned())
                    .or_default() += dmg;
                t!(
                    "STORE",
                    format!("entities[{src:?}].total_damage += {dmg} (riposte)")
                );

                let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                    );
                }
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
                t!(
                    "STORE",
                    format!("mob_damage[#{mob_id}][{src:?}].total_damage += {dmg} (riposte)")
                );
                event_desc =
                    format!("Riposte(dmg) player={src:?} → mob={tgt:?} dmg={dmg} mob_id=#{mob_id}");
            }

            if !state.fight_started {
                state.fight_started = true;
                t!("STATE", "fight_start set");
            }
            let newly = update_mob_name_and_confirmed(&mut state);
            for m in &newly {
                t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
            }
            matched = true;

        // ── RE_DS ─────────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_DS.captures(line) {
            let src = norm(&caps["src"], &args.player);
            let tgt = norm(&caps["tgt"], &args.player);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            t!(
                "MATCH",
                format!("RE_DS — src={src:?} tgt={tgt:?} dmg={dmg}")
            );

            // A mob can have a DS: "Player was struck by Mob's damage shield for N"
            let src_is_mob = src.contains(' ')
                || state.confirmed_mobs.contains(&src)
                || state.known_players.contains(tgt.as_str())
                || tgt == args.player;

            if src_is_mob {
                t!(
                    "REASON",
                    format!("{src:?} identified as MOB with damage shield")
                );
                if !state.known_players.contains(&src) {
                    let was_new = state.confirmed_mobs.insert(src.clone());
                    if was_new {
                        t!("CONFIRM", format!("mob confirmed: {src:?}"));
                    }
                }
                let (mob_id, is_new_instance) = update_mob_list(&mut state, &src);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {src:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({src:?}) last_seen refreshed")
                    );
                }
                let tank = state
                    .mob_tanking
                    .entry(mob_id)
                    .or_default()
                    .entry(tgt.clone())
                    .or_default();
                tank.total_damage += dmg;
                *tank.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
                t!(
                    "STORE",
                    format!("mob_tanking[#{mob_id}][{tgt:?}].total_damage += {dmg} (ds)")
                );
                event_desc =
                    format!("DS(tank) mob={src:?} → player={tgt:?} dmg={dmg} mob_id=#{mob_id}");
            } else {
                t!(
                    "REASON",
                    format!("{src:?} identified as PLAYER with damage shield")
                );
                let is_new_player = !state.entities.contains_key(&src);
                let stats = state.entities.entry(src.clone()).or_default();
                if is_new_player {
                    t!("NEW", format!("player entity created: {src:?}"));
                }
                state.known_players.insert(src.clone());
                stats.total_damage += dmg;
                *stats.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
                t!(
                    "STORE",
                    format!("entities[{src:?}].total_damage += {dmg} (damage shield)")
                );

                let is_new_candidate = track_mob_candidate(&mut state, tgt.clone(), &src);
                if is_new_candidate {
                    t!(
                        "TRACK",
                        format!("{tgt:?} tracked as mob candidate (DS'd by {src:?})")
                    );
                }
                let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                    );
                }
                let mob_p = state
                    .mob_damage
                    .entry(mob_id)
                    .or_default()
                    .entry(src.clone())
                    .or_default();
                mob_p.total_damage += dmg;
                *mob_p.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
                t!(
                    "STORE",
                    format!("mob_damage[#{mob_id}][{src:?}].total_damage += {dmg} (ds)")
                );
                event_desc =
                    format!("DS(dmg) player={src:?} → mob={tgt:?} dmg={dmg} mob_id=#{mob_id}");
            }

            if !state.fight_started {
                state.fight_started = true;
                t!("STATE", "fight_start set");
            }
            let newly = update_mob_name_and_confirmed(&mut state);
            for m in &newly {
                t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
            }
            matched = true;

        // ── RE_DS_PROC ────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_DS_PROC.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &args.player);
            let src = if let Some(m) = caps.name("src") {
                norm(m.as_str(), &args.player)
            } else {
                args.player.clone() // YOUR
            };
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);

            t!(
                "MATCH",
                format!("RE_DS_PROC — src={src:?} tgt={tgt:?} dmg={dmg}")
            );
            t!(
                "REASON",
                format!("{src:?} identified as PLAYER with outbound damage shield proc")
            );

            let is_new_player = !state.entities.contains_key(&src);
            let stats = state.entities.entry(src.clone()).or_default();
            if is_new_player {
                t!("NEW", format!("player entity created: {src:?}"));
            }
            state.known_players.insert(src.clone());
            stats.total_damage += dmg;
            *stats.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
            t!(
                "STORE",
                format!("entities[{src:?}].total_damage += {dmg} (ds proc)")
            );

            let is_new_candidate = track_mob_candidate(&mut state, tgt.clone(), &src);
            if is_new_candidate {
                t!(
                    "TRACK",
                    format!("{tgt:?} tracked as mob candidate (DS proc from {src:?})")
                );
            }
            let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
            if is_new_instance {
                t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
            } else {
                t!(
                    "UPDATE",
                    format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                );
            }
            let mob_p = state
                .mob_damage
                .entry(mob_id)
                .or_default()
                .entry(src.clone())
                .or_default();
            mob_p.total_damage += dmg;
            *mob_p.damage_by_type.entry("ds".to_owned()).or_default() += dmg;
            t!(
                "STORE",
                format!("mob_damage[#{mob_id}][{src:?}].total_damage += {dmg} (ds)")
            );

            if !state.fight_started {
                state.fight_started = true;
                t!("STATE", "fight_start set");
            }
            let newly = update_mob_name_and_confirmed(&mut state);
            for m in &newly {
                t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
            }
            event_desc =
                format!("DS(proc) player={src:?} → mob={tgt:?} dmg={dmg} mob_id=#{mob_id}");
            matched = true;

        // ── RE_MISS ───────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_MISS.captures(line) {
            let src = norm(caps["src"].trim(), &args.player);
            let tgt = norm(caps["tgt"].trim(), &args.player);
            let miss_type = normalize_miss(&caps["miss"]).to_owned();

            t!(
                "MATCH",
                format!("RE_MISS — src={src:?} tgt={tgt:?} type={miss_type:?}")
            );

            // Track avoidance on the defender.
            let def_stats = state.entities.entry(tgt.clone()).or_default();
            *def_stats
                .avoidance_by_type
                .entry(miss_type.clone())
                .or_default() += 1;
            t!(
                "STORE",
                format!("entities[{tgt:?}].avoidance_by_type[{miss_type:?}] += 1")
            );

            // If src is a mob, update mob tracking and tanking avoidance.
            let src_is_mob = src.contains(' ')
                || state.confirmed_mobs.contains(&src)
                || state.known_players.contains(tgt.as_str())
                || tgt == args.player;
            if src_is_mob {
                t!("REASON", format!("{src:?} identified as MOB"));
                if !state.known_players.contains(&src) {
                    let was_new = state.confirmed_mobs.insert(src.clone());
                    if was_new {
                        t!("CONFIRM", format!("mob confirmed: {src:?}"));
                    }
                }
                let (mob_id, is_new_instance) = update_mob_list(&mut state, &src);
                if is_new_instance {
                    t!("NEW", format!("mob instance #{mob_id} created for {src:?}"));
                } else {
                    t!(
                        "UPDATE",
                        format!("mob instance #{mob_id} ({src:?}) last_seen refreshed")
                    );
                }
                let tank = state
                    .mob_tanking
                    .entry(mob_id)
                    .or_default()
                    .entry(tgt.clone())
                    .or_default();
                *tank.avoidance_by_type.entry(miss_type.clone()).or_default() += 1;
                t!(
                    "STORE",
                    format!(
                        "mob_tanking[#{mob_id}][{tgt:?}].avoidance_by_type[{miss_type:?}] += 1"
                    )
                );
                event_desc = format!(
                    "Miss mob={src:?} → player={tgt:?} type={miss_type:?} mob_id=#{mob_id}"
                );
            } else {
                t!(
                    "REASON",
                    format!("{src:?} identified as PLAYER — avoidance on target only")
                );
                event_desc = format!("Miss src={src:?} → tgt={tgt:?} type={miss_type:?}");
            }
            matched = true;

        // ── RE_ABSORB_SKIN ────────────────────────────────────────────────────
        } else if let Some(caps) = RE_ABSORB_SKIN.captures(line) {
            let tgt = if let Some(m) = caps.name("tgt") {
                norm(m.as_str(), &args.player)
            } else {
                args.player.clone()
            };
            let src = norm(caps["src"].trim(), &args.player);

            t!("MATCH", format!("RE_ABSORB_SKIN — tgt={tgt:?} src={src:?}"));
            event_desc = format!("AbsorbSkin tgt={tgt:?} src(thorns owner)={src:?}");
            matched = true;

        // ── RE_ABSORB_RUNE ────────────────────────────────────────────────────
        } else if let Some(caps) = RE_ABSORB_RUNE.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &args.player);

            t!("MATCH", format!("RE_ABSORB_RUNE — tgt={tgt:?}"));
            event_desc = format!("AbsorbRune tgt={tgt:?}");
            matched = true;

        // ── RE_RESIST ─────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_RESIST.captures(line) {
            let tgt = normalize_article_case(&caps["tgt"]);
            let spell = caps["spell"].trim().to_owned();
            let src = if let Some(m) = caps.name("src") {
                norm(m.as_str(), &args.player)
            } else {
                args.player.clone()
            };

            t!(
                "MATCH",
                format!("RE_RESIST — src={src:?} spell={spell:?} tgt={tgt:?}")
            );

            let stats = state.entities.entry(src.clone()).or_default();
            *stats.resists_by_spell.entry(spell.clone()).or_default() += 1;
            t!(
                "STORE",
                format!("entities[{src:?}].resists_by_spell[{spell:?}] += 1")
            );

            event_desc = format!("Resist src={src:?} spell={spell:?} tgt={tgt:?}");
            matched = true;

        // ── RE_CAST ───────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_CAST.captures(line) {
            let src = norm(&caps["src"], &args.player);
            let spell = caps["spell"].to_owned();

            t!("MATCH", format!("RE_CAST — src={src:?} spell={spell:?}"));

            let prev = state.spell_caster.insert(spell.clone(), src.clone());
            if let Some(old) = &prev {
                if old != &src {
                    t!(
                        "UPDATE",
                        format!("spell_caster[{spell:?}]: {old:?} → {src:?}")
                    );
                } else {
                    t!(
                        "UPDATE",
                        format!("spell_caster[{spell:?}] = {src:?} (unchanged)")
                    );
                }
            } else {
                t!(
                    "STORE",
                    format!("spell_caster[{spell:?}] = {src:?} (new attribution)")
                );
            }
            // No fight_start touch for cast-only lines.
            event_desc = format!("Cast src={src:?} spell={spell:?}");
            matched = true;

        // ── SLAY / DEATH ─────────────────────────────────────────────────────
        } else if let Some(tgt) = slay_tgt(line) {
            t!("MATCH", format!("SLAY — tgt={tgt:?}"));

            let was_new = state.dead_mobs.insert(tgt.clone());
            if was_new {
                t!("STORE", format!("dead_mobs ← {tgt:?}"));
            }

            let all_dead = !state.mob_list.is_empty()
                && state
                    .mob_list
                    .iter()
                    .filter(|m| state.confirmed_mobs.contains(&m.name))
                    .all(|m| state.dead_mobs.contains(&m.name));
            if all_dead {
                t!("STATE", "all confirmed mobs dead → fight_end frozen");
            }

            let mob_id = state.mob_list.iter().find(|m| m.name == tgt).map(|m| m.id);
            if let Some(id) = mob_id {
                t!("LOOKUP", format!("mob instance #{id} for {tgt:?}"));
                event_desc = format!("Slay tgt={tgt:?} mob_id=#{id}");
            } else {
                t!(
                    "LOOKUP",
                    format!("{tgt:?} not in mob_list (killed without a prior hit?)")
                );
                event_desc = format!("Slay tgt={tgt:?} mob_id=unknown");
            }
            matched = true;

        // ── RE_HAS_TAKEN ──────────────────────────────────────────────────────
        } else if let Some(caps) = RE_HAS_TAKEN.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &args.player);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);
            let spell = caps["spell"].trim().trim_end_matches('.').to_owned();

            // Resolve attacker: "your" → player, "by X" with optional possessive prefix → X,
            // plain "Src's Spell" → src.
            let (attacker, spell) = if caps.name("your").is_some() {
                (Some(args.player.clone()), spell)
            } else if let Some(m) = caps.name("by_src") {
                let s = m.as_str().trim().trim_end_matches('.');
                let real_src = if s.is_empty() {
                    None
                } else {
                    Some(norm(s, &args.player))
                };
                let full_spell = if let Some(pfx) = caps.name("src") {
                    format!("{}'s {}", pfx.as_str(), spell)
                } else {
                    spell
                };
                (real_src, full_spell)
            } else if let Some(m) = caps.name("src") {
                (Some(norm(m.as_str(), &args.player)), spell)
            } else {
                (None, spell)
            };

            t!(
                "MATCH",
                format!(
                    "RE_HAS_TAKEN — tgt={tgt:?} dmg={dmg} spell={spell:?} attacker={attacker:?}"
                )
            );

            if let Some(src) = attacker {
                let src_is_mob = src.contains(' ')
                    || state.confirmed_mobs.contains(&src)
                    || state.known_players.contains(tgt.as_str())
                    || tgt == args.player;

                if src_is_mob && src != args.player {
                    t!(
                        "REASON",
                        format!("{src:?} identified as MOB — mob DoT/spell hitting player")
                    );
                    if !state.known_players.contains(&src) {
                        let was_new = state.confirmed_mobs.insert(src.clone());
                        if was_new {
                            t!("CONFIRM", format!("mob confirmed: {src:?}"));
                        }
                    }
                    let (mob_id, is_new_instance) = update_mob_list(&mut state, &src);
                    if is_new_instance {
                        t!("NEW", format!("mob instance #{mob_id} created for {src:?}"));
                    } else {
                        t!(
                            "UPDATE",
                            format!("mob instance #{mob_id} ({src:?}) last_seen refreshed")
                        );
                    }
                    let tank = state
                        .mob_tanking
                        .entry(mob_id)
                        .or_default()
                        .entry(tgt.clone())
                        .or_default();
                    tank.total_damage += dmg;
                    *tank.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                    t!(
                        "STORE",
                        format!("mob_tanking[#{mob_id}][{tgt:?}].total_damage += {dmg} (dot)")
                    );
                    event_desc = format!("HasTaken(tank) mob={src:?} → player={tgt:?} dmg={dmg} spell={spell:?} mob_id=#{mob_id}");
                } else {
                    t!(
                        "REASON",
                        format!("{src:?} identified as PLAYER — player DoT hitting mob")
                    );
                    let is_new = !state.entities.contains_key(&src);
                    let stats = state.entities.entry(src.clone()).or_default();
                    if is_new {
                        t!("NEW", format!("player entity created: {src:?}"));
                    }
                    state.known_players.insert(src.clone());
                    stats.total_damage += dmg;
                    *stats.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                    *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    t!(
                        "STORE",
                        format!("entities[{src:?}].total_damage += {dmg} (dot / spell={spell:?})")
                    );

                    let is_new_candidate = track_mob_candidate(&mut state, tgt.clone(), &src);
                    if is_new_candidate {
                        t!("TRACK", format!("{tgt:?} tracked as mob candidate"));
                    }
                    let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
                    if is_new_instance {
                        t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
                    } else {
                        t!(
                            "UPDATE",
                            format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                        );
                    }
                    let mob_p = state
                        .mob_damage
                        .entry(mob_id)
                        .or_default()
                        .entry(src.clone())
                        .or_default();
                    mob_p.total_damage += dmg;
                    *mob_p.damage_by_type.entry("dot".to_owned()).or_default() += dmg;
                    *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
                    t!("STORE", format!("mob_damage[#{mob_id}][{src:?}].total_damage += {dmg} (dot / spell={spell:?})"));
                    event_desc = format!("HasTaken(dmg) player={src:?} → mob={tgt:?} dmg={dmg} spell={spell:?} mob_id=#{mob_id}");
                }
                if !state.fight_started {
                    state.fight_started = true;
                    t!("STATE", "fight_start set");
                }
                let newly = update_mob_name_and_confirmed(&mut state);
                for m in &newly {
                    t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
                }
                matched = true;
            } else {
                t!("SKIP", "attacker unresolvable — skipping attribution");
            }

        // ── RE_EXTRA_DMG ──────────────────────────────────────────────────────
        } else if let Some(caps) = RE_EXTRA_DMG.captures(line) {
            let tgt = norm(caps["tgt"].trim(), &args.player);
            let dmg: u64 = caps["dmg"].parse().unwrap_or(0);
            let spell = caps["spell"].trim().to_owned();
            let src = if caps.name("your").is_some() {
                args.player.clone()
            } else if let Some(m) = caps.name("src") {
                norm(m.as_str(), &args.player)
            } else {
                args.player.clone()
            };

            t!(
                "MATCH",
                format!("RE_EXTRA_DMG — src={src:?} spell={spell:?} tgt={tgt:?} dmg={dmg}")
            );

            let is_new = !state.entities.contains_key(&src);
            let stats = state.entities.entry(src.clone()).or_default();
            if is_new {
                t!("NEW", format!("player entity created: {src:?}"));
            }
            state.known_players.insert(src.clone());
            stats.total_damage += dmg;
            *stats.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            t!(
                "STORE",
                format!("entities[{src:?}].total_damage += {dmg} (extra/bane spell={spell:?})")
            );

            let is_new_candidate = track_mob_candidate(&mut state, tgt.clone(), &src);
            if is_new_candidate {
                t!(
                    "TRACK",
                    format!("{tgt:?} tracked as mob candidate (extra dmg from {src:?})")
                );
            }
            let (mob_id, is_new_instance) = update_mob_list(&mut state, &tgt);
            if is_new_instance {
                t!("NEW", format!("mob instance #{mob_id} created for {tgt:?}"));
            } else {
                t!(
                    "UPDATE",
                    format!("mob instance #{mob_id} ({tgt:?}) last_seen refreshed")
                );
            }
            let mob_p = state
                .mob_damage
                .entry(mob_id)
                .or_default()
                .entry(src.clone())
                .or_default();
            mob_p.total_damage += dmg;
            *mob_p.damage_by_spell.entry(spell.clone()).or_default() += dmg;
            t!(
                "STORE",
                format!(
                    "mob_damage[#{mob_id}][{src:?}].total_damage += {dmg} (extra spell={spell:?})"
                )
            );

            if !state.fight_started {
                state.fight_started = true;
                t!("STATE", "fight_start set");
            }
            let newly = update_mob_name_and_confirmed(&mut state);
            for m in &newly {
                t!("CONFIRM", format!("mob confirmed from candidates: {m:?}"));
            }
            event_desc = format!(
                "ExtraDmg player={src:?} → mob={tgt:?} dmg={dmg} spell={spell:?} mob_id=#{mob_id}"
            );
            matched = true;

        // ── RE_WHO ────────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_WHO.captures(line) {
            let name = caps["name"].to_owned();
            let classes = parse_who_classes(&caps["classes"]);
            t!(
                "MATCH",
                format!("RE_WHO — name={name:?} classes={classes:?}")
            );
            if !classes.is_empty() {
                let prev = state.player_classes.insert(name.clone(), classes.clone());
                if prev.as_deref() != Some(&classes) {
                    t!("STORE", format!("player_classes[{name:?}] = {classes:?}"));
                }
            } else {
                t!(
                    "SKIP",
                    format!("no recognised class names in {:?}", &caps["classes"])
                );
            }
            event_desc = format!("Who name={name:?} classes={classes:?}");
            matched = true;

        // ── RE_HEAL ───────────────────────────────────────────────────────────
        } else if let Some(caps) = RE_HEAL.captures(line) {
            let src = norm(&caps["src"], &args.player);
            let raw_tgt = caps["tgt"].trim().trim_end_matches(" over time");
            let tgt = match raw_tgt {
                "himself" | "herself" | "itself" | "yourself" => src.clone(),
                other => norm(other, &args.player),
            };
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

            t!(
                "MATCH",
                format!("RE_HEAL — src={src:?} tgt={tgt:?} amt={amt} spell={spell:?}")
            );

            let is_new_src = !state.entities.contains_key(&src);
            let stats = state.entities.entry(src.clone()).or_default();
            if is_new_src {
                t!("NEW", format!("entity created: {src:?}"));
            }
            stats.total_heals += amt;
            *stats.heals_by_spell.entry(spell.clone()).or_default() += amt;
            t!(
                "STORE",
                format!("entities[{src:?}].total_heals += {amt} (spell={spell:?})")
            );

            let tgt_stats = state.entities.entry(tgt.clone()).or_default();
            tgt_stats.total_healed_received += amt;
            *tgt_stats
                .healed_received_by_spell
                .entry(spell.clone())
                .or_default() += amt;
            t!(
                "STORE",
                format!("entities[{tgt:?}].total_healed_received += {amt} (spell={spell:?})")
            );

            if let Some(mob_id) = state.active_mob_id {
                let heal_stats = state
                    .mob_healing
                    .entry(mob_id)
                    .or_default()
                    .entry(src.clone())
                    .or_default();
                heal_stats.total_heals += amt;
                *heal_stats.heals_by_spell.entry(spell.clone()).or_default() += amt;
                t!(
                    "STORE",
                    format!(
                        "mob_healing[#{mob_id}][{src:?}].total_heals += {amt} (spell={spell:?})"
                    )
                );

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
                t!(
                    "STORE",
                    format!("mob_healed[#{mob_id}][{tgt:?}].total_healed_received += {amt}")
                );
            } else {
                t!(
                    "INFO",
                    "no active mob — heal stored globally only (not per-mob)"
                );
            }
            event_desc = format!("Heal src={src:?} → tgt={tgt:?} amt={amt} spell={spell:?}");
            matched = true;
        }

        // ── Filter / print ────────────────────────────────────────────────────

        let should_print = if !matched && args.only_matched {
            false
        } else {
            if filter_words.is_empty() {
                matched || !args.only_matched
            } else {
                let combined = trace_lines.join(" ").to_lowercase();
                filter_words.iter().all(|w| combined.contains(w.as_str()))
            }
        };

        if should_print {
            println!(
                "\n╔══ Line {:>6} ══════════════════════════════════════════════════",
                line_no
            );
            println!("║  TS      : {ts_display}");
            println!("║  RAW     : {}", raw_line);
            for tl in &trace_lines {
                println!("{tl}");
            }
            if matched {
                println!("║  EVENT   : {event_desc}");
                // Print current snapshot summary.
                let players: Vec<String> = state
                    .entities
                    .iter()
                    .filter(|(k, _)| state.known_players.contains(k.as_str()))
                    .map(|(k, v)| format!("{k}={}", v.total_damage))
                    .collect();
                let mobs: Vec<String> = state
                    .mob_list
                    .iter()
                    .map(|m| {
                        let dead = if state.dead_mobs.contains(&m.name) {
                            " [DEAD]"
                        } else {
                            ""
                        };
                        let conf = if state.confirmed_mobs.contains(&m.name) {
                            "✓"
                        } else {
                            "?"
                        };
                        format!("#{} {}{}{}", m.id, m.name, conf, dead)
                    })
                    .collect();
                println!("║  PLAYERS : [{}]", players.join(", "));
                println!("║  MOBS    : [{}]", mobs.join(", "));
                println!("║  SPELL↦  : {} attributions", state.spell_caster.len());
            } else {
                println!("║  RESULT  : [no combat pattern matched — skipped]");
            }
            println!("╚══════════════════════════════════════════════════════════════════");
        }

        if args.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(args.delay_ms));
        }
    }

    // ── Final summary ─────────────────────────────────────────────────────────
    println!("\n╔══ FINAL SUMMARY ══════════════════════════════════════════════════");
    println!("║  Lines parsed   : {}", state.lines_parsed);
    println!("║  Fight started  : {}", state.fight_started);
    println!("║  Players seen   :");
    let mut players: Vec<_> = state
        .entities
        .iter()
        .filter(|(k, _)| state.known_players.contains(k.as_str()))
        .collect();
    players.sort_by_key(|b| std::cmp::Reverse(b.1.total_damage));
    for (name, stats) in &players {
        let cls = state
            .player_classes
            .get(*name)
            .map(|v| v.join("/"))
            .unwrap_or_else(|| "?".to_owned());
        println!(
            "║    {name:<20} [{cls:<11}] dmg={:<8} heals={}",
            stats.total_damage, stats.total_heals
        );
        let mut types: Vec<_> = stats.damage_by_type.iter().collect();
        types.sort_by(|a, b| b.1.cmp(a.1));
        for (t, d) in &types {
            println!("║      melee/{t:<12} {d}");
        }
        let mut spells: Vec<_> = stats.damage_by_spell.iter().collect();
        spells.sort_by(|a, b| b.1.cmp(a.1));
        for (s, d) in &spells {
            println!("║      spell/{s:<12} {d}");
        }
        let mut avoids: Vec<_> = stats.avoidance_by_type.iter().collect();
        avoids.sort_by(|a, b| b.1.cmp(a.1));
        for (t, c) in &avoids {
            println!("║      avoid/{t:<12} {c}");
        }
        let mut resists: Vec<_> = stats.resists_by_spell.iter().collect();
        resists.sort_by(|a, b| b.1.cmp(a.1));
        for (s, c) in &resists {
            println!("║      resist/{s:<11} {c}");
        }
    }
    println!("║  Mob instances  :");
    for mob in &state.mob_list {
        let dead = if state.dead_mobs.contains(&mob.name) {
            " [DEAD]"
        } else {
            ""
        };
        let conf = if state.confirmed_mobs.contains(&mob.name) {
            "confirmed"
        } else {
            "candidate"
        };
        println!("║    #{:<4} {:<30} {conf}{dead}", mob.id, mob.name);
        if let Some(by_player) = state.mob_damage.get(&mob.id) {
            let mut entries: Vec<_> = by_player.iter().collect();
            entries.sort_by_key(|b| std::cmp::Reverse(b.1.total_damage));
            for (player, stats) in entries {
                println!("║      damage from {player:<18} = {}", stats.total_damage);
            }
        }
        if let Some(by_player) = state.mob_tanking.get(&mob.id) {
            let mut entries: Vec<_> = by_player.iter().collect();
            entries.sort_by_key(|b| std::cmp::Reverse(b.1.total_damage));
            for (player, stats) in entries {
                println!("║      tanking by  {player:<18} = {}", stats.total_damage);
            }
        }
    }
    println!(
        "║  Confirmed mobs : {:?}",
        state.confirmed_mobs.iter().collect::<Vec<_>>()
    );
    println!(
        "║  Dead mobs      : {:?}",
        state.dead_mobs.iter().collect::<Vec<_>>()
    );
    println!("║  Spell attribs  : {} entries", state.spell_caster.len());
    println!("╚══════════════════════════════════════════════════════════════════");
}

// ── Slay helper ───────────────────────────────────────────────────────────────

/// Try all kill-message patterns and return the normalised target name if any match.
fn slay_tgt(line: &str) -> Option<String> {
    if let Some(caps) = RE_SLAY_YOU.captures(line) {
        return Some(normalize_article_case(&caps["tgt"]));
    }
    if let Some(caps) = RE_SLAY_HAS.captures(line) {
        return Some(normalize_article_case(&caps["tgt"]));
    }
    if let Some(caps) = RE_SLAIN_BY.captures(line) {
        return Some(normalize_article_case(&caps["tgt"]));
    }
    if let Some(caps) = RE_DIED.captures(line) {
        return Some(normalize_article_case(&caps["tgt"]));
    }
    None
}
