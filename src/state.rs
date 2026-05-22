use std::collections::{HashMap, HashSet};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

/// A single mob instance sighted during combat.
#[derive(Debug, Clone)]
pub struct MobSighting {
    /// Unique per-instance ID — incremented each time a new encounter begins.
    pub id: u64,
    pub name: String,
    /// When this instance was first engaged.
    pub first_seen: Instant,
    pub last_seen: Instant,
    /// Log-line timestamps (fake-UTC unix seconds) for the first and most
    /// recent combat event involving this mob.  Zero if not yet populated.
    pub first_log_ts: u32,
    pub last_log_ts: u32,
}

/// Per-entity combat breakdown stored in the shared snapshot.
#[derive(Debug, Default, Clone, Serialize)]
pub struct EntityCombatStats {
    pub total_damage: u64,
    /// Melee/skill attack type → total damage  ("slash", "backstab", "hit", "kick", …)
    pub damage_by_type: HashMap<String, u64>,
    /// Spell name → total damage (the "spell breakdowns" section)
    pub damage_by_spell: HashMap<String, u64>,
    /// Number of critical hits landed.
    pub crit_count: u64,
    /// Number of twincast procs.
    pub twincast_count: u64,
    pub total_heals: u64,
    pub heals_by_spell: HashMap<String, u64>,
    pub total_healed_received: u64,
    pub healed_received_by_spell: HashMap<String, u64>,
    /// Avoidance type → count: dodge, parry, miss, block, riposte, invulnerable, absorb.
    /// Populated for the player who avoided (defender perspective).
    pub avoidance_by_type: HashMap<String, u64>,
    /// Spell name → number of times that spell was resisted by the target NPC.
    pub resists_by_spell: HashMap<String, u64>,
    /// Class shortcode if known (ROG, BRD, WIZ, …) — empty string if unknown.
    pub class: String,
}

/// Immutable snapshot shared across threads via `Arc<ArcSwap<CombatState>>`.
/// The parser thread clones this and swaps a new `Arc` in atomically.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CombatState {
    /// Current or most recent mob name, e.g. "Mistress of Scorn".
    pub mob_name: String,
    /// Wall-clock time the fight started, formatted for display ("5/5/2026, 5:01:17 PM").
    pub fight_start_display: String,
    /// Timestamp of the most recently parsed log line, formatted as "Jan 2 12:34".
    pub last_log_time: String,
    /// Per-entity stats for known players, keyed by entity name.
    pub entities: HashMap<String, EntityCombatStats>,
    pub lines_parsed: u64,
    pub player_name: String,

    /// Per-mob-instance per-player damage breakdown: mob_id → (player_name → stats).
    pub mob_damage: HashMap<u64, HashMap<String, EntityCombatStats>>,
    /// Per-mob-instance tanking: mob_id → (player_name → damage_received_stats).
    pub mob_tanking: HashMap<u64, HashMap<String, EntityCombatStats>>,
    /// Per-mob-instance healing cast during the encounter: mob_id → (healer → stats).
    pub mob_healing: HashMap<u64, HashMap<String, EntityCombatStats>>,
    /// Per-mob-instance heals received during the encounter: mob_id → (healee → stats).
    pub mob_healed: HashMap<u64, HashMap<String, EntityCombatStats>>,
    /// Names confirmed to be mobs (not players).
    pub confirmed_mobs: HashSet<String>,
    /// Names confirmed to be players — populated only when a name is the
    /// attributed *source* of player-side damage (melee, spell, dot, ds, rip).
    /// Unlike `entities` (which also stores mob healers), this set is never
    /// polluted by mob heal events and is the authoritative signal for
    /// mob-vs-player classification.
    pub known_players: HashSet<String>,
    /// Mobs confirmed dead via kill messages.
    pub dead_mobs: HashSet<String>,
    /// Classes for known players, populated from /who log lines.
    /// Maps player name → ordered list of 1–3 EQ class short-codes (e.g. ["WAR","MNK","ROG"]).
    /// Intentionally persisted across combat resets — /who data outlives any single fight.
    /// Skipped from direct serialization; class info is embedded per-entity in to_api_json().
    #[serde(skip)]
    pub player_classes: HashMap<String, Vec<String>>,
    /// Level for known players, populated from /who log lines.
    #[serde(skip)]
    pub player_levels: HashMap<String, u8>,

    #[serde(skip)]
    pub fight_start: Option<Instant>,
    /// Wall-clock time the last mob was slain, used to freeze the timer.
    #[serde(skip)]
    pub fight_end: Option<Instant>,
    /// Currently casting players: player_name → (spell_name, cast_start).
    #[serde(skip)]
    pub active_casts: HashMap<String, (String, Instant)>,
    /// Mobs seen during this fight, ordered most-recently-seen first.
    #[serde(skip)]
    pub mob_list: Vec<MobSighting>,
    /// Counter used to assign unique IDs to mob sightings.
    #[serde(skip)]
    pub next_mob_id: u64,
    /// ID of the mob instance most recently seen in a combat event.
    #[serde(skip)]
    pub active_mob_id: Option<u64>,
    /// Mob-instance ID of the most recently slain mob, used to attribute
    /// "from the corpse" currency lines that carry no mob name.
    #[serde(skip)]
    pub pending_loot_mob: Option<u32>,
    /// EQ log timestamp (seconds) of the most recent slay event.
    #[serde(skip)]
    pub pending_loot_ts: u32,
}

impl CombatState {
    pub fn elapsed_secs(&self) -> f64 {
        match (self.fight_start, self.fight_end) {
            (Some(start), Some(end)) => end.duration_since(start).as_secs_f64(),
            (Some(start), None) => start.elapsed().as_secs_f64(),
            _ => 0.0,
        }
    }

    pub fn total_damage(&self) -> u64 {
        self.entities.values().map(|e| e.total_damage).sum()
    }

    pub fn total_heals(&self) -> u64 {
        self.entities.values().map(|e| e.total_heals).sum()
    }

    /// All player entities sorted by total damage descending, excluding confirmed mobs.
    pub fn sorted_by_damage(&self) -> Vec<(&str, &EntityCombatStats)> {
        let mut v: Vec<_> = self
            .entities
            .iter()
            .filter(|(k, _)| !self.confirmed_mobs.contains(k.as_str()))
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.1.total_damage));
        v
    }

    /// Players sorted by damage dealt to a specific mob instance.
    #[allow(dead_code)]
    pub fn mob_players_by_damage(&self, mob_id: u64) -> Vec<(&str, &EntityCombatStats)> {
        let Some(by_player) = self.mob_damage.get(&mob_id) else {
            return vec![];
        };
        let mut v: Vec<_> = by_player.iter().map(|(k, v)| (k.as_str(), v)).collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.1.total_damage));
        v
    }

    /// Players sorted by damage tanked from a specific mob instance.
    #[allow(dead_code)]
    pub fn mob_players_by_tanking(&self, mob_id: u64) -> Vec<(&str, &EntityCombatStats)> {
        let Some(by_player) = self.mob_tanking.get(&mob_id) else {
            return vec![];
        };
        let mut v: Vec<_> = by_player.iter().map(|(k, v)| (k.as_str(), v)).collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.1.total_damage));
        v
    }

    /// Healers sorted by total heals descending.
    pub fn healers_sorted(&self) -> Vec<(&str, &EntityCombatStats)> {
        let mut v: Vec<_> = self
            .entities
            .iter()
            .filter(|(_, s)| s.total_heals > 0)
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.1.total_heals));
        v
    }

    /// Healees (heal recipients) sorted by total heals received descending.
    pub fn healees_sorted(&self) -> Vec<(&str, &EntityCombatStats)> {
        let mut v: Vec<_> = self
            .entities
            .iter()
            .filter(|(_, s)| s.total_healed_received > 0)
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.1.total_healed_received));
        v
    }

    /// Canonical JSON sent by both `/stats` and `/ws`.
    pub fn to_api_json(&self) -> Value {
        let elapsed = self.elapsed_secs().max(0.001);

        // ── Aggregate damage entities ──
        let entities: Vec<Value> = self
            .sorted_by_damage()
            .iter()
            .map(|(name, stats)| {
                let by_type: Vec<Value> = {
                    let mut rows: Vec<_> = stats.damage_by_type.iter().collect();
                    rows.sort_by(|a, b| b.1.cmp(a.1));
                    rows.iter()
                        .map(|(t, &d)| {
                            json!({
                                "type": t,
                                "damage": d,
                                "dps": (d as f64 / elapsed).round() as u64,
                            })
                        })
                        .collect()
                };
                let spells: Vec<Value> = {
                    let mut rows: Vec<_> = stats.damage_by_spell.iter().collect();
                    rows.sort_by(|a, b| b.1.cmp(a.1));
                    rows.iter()
                        .map(|(s, &d)| {
                            json!({
                                "spell": s,
                                "damage": d,
                                "dps": (d as f64 / elapsed).round() as u64,
                            })
                        })
                        .collect()
                };
                let classes = self.player_classes.get(*name).cloned().unwrap_or_default();
                let class = classes.first().cloned().unwrap_or_default();
                let level = self.player_levels.get(*name).copied().unwrap_or(0);
                json!({
                    "name": name,
                    "class": class,
                    "classes": classes,
                    "level": level,
                    "total_damage": stats.total_damage,
                    "dps": (stats.total_damage as f64 / elapsed).round() as u64,
                    "crit_count": stats.crit_count,
                    "twincast_count": stats.twincast_count,
                    "damage_by_type": by_type,
                    "spells": spells,
                })
            })
            .collect();

        // ── Healers ──
        let healers: Vec<Value> = self
            .healers_sorted()
            .iter()
            .map(|(name, stats)| {
                let heals_by_spell: Vec<Value> = {
                    let mut rows: Vec<_> = stats.heals_by_spell.iter().collect();
                    rows.sort_by(|a, b| b.1.cmp(a.1));
                    rows.iter()
                        .map(|(s, &h)| {
                            json!({
                                "spell": s,
                                "healed": h,
                                "hps": (h as f64 / elapsed).round() as u64,
                            })
                        })
                        .collect()
                };
                let classes = self.player_classes.get(*name).cloned().unwrap_or_default();
                let class = classes.first().cloned().unwrap_or_default();
                let level = self.player_levels.get(*name).copied().unwrap_or(0);
                json!({
                    "name": name,
                    "class": class,
                    "classes": classes,
                    "level": level,
                    "total_heals": stats.total_heals,
                    "hps": (stats.total_heals as f64 / elapsed).round() as u64,
                    "heals_by_spell": heals_by_spell,
                })
            })
            .collect();

        // ── Healees (recipients) ──
        let healees: Vec<Value> = self
            .healees_sorted()
            .iter()
            .map(|(name, stats)| {
                let healed_received_by_spell: Vec<Value> = {
                    let mut rows: Vec<_> = stats.healed_received_by_spell.iter().collect();
                    rows.sort_by(|a, b| b.1.cmp(a.1));
                    rows.iter()
                        .map(|(s, &h)| {
                            json!({
                                "spell": s,
                                "healed": h,
                                "hps": (h as f64 / elapsed).round() as u64,
                            })
                        })
                        .collect()
                };
                let classes = self.player_classes.get(*name).cloned().unwrap_or_default();
                let class = classes.first().cloned().unwrap_or_default();
                let level = self.player_levels.get(*name).copied().unwrap_or(0);
                json!({
                    "name": name,
                    "class": class,
                    "classes": classes,
                    "level": level,
                    "total_healed_received": stats.total_healed_received,
                    "hps": (stats.total_healed_received as f64 / elapsed).round() as u64,
                    "healed_received_by_spell": healed_received_by_spell,
                })
            })
            .collect();

        // ── Mob list (confirmed only, most-recently-seen first) ──
        let mob_list: Vec<Value> = self.mob_list.iter()
            .filter(|m| self.confirmed_mobs.contains(&m.name))
            .map(|m| {
                let secs_since_last = m.last_seen.elapsed().as_secs_f64();
                let timed_out = secs_since_last >= 15.0;
                let is_dead = self.dead_mobs.contains(&m.name) || timed_out;
                let is_active = !is_dead && secs_since_last < 5.0;
                // Encounter duration: from first hit to last hit (fixed, doesn't grow after death).
                let encounter_secs = m.last_seen.duration_since(m.first_seen).as_secs_f64();
                // Offsets from fight start so the UI can display "engaged at +Xs, last at +Ys".
                let (first_offset, last_offset) = match self.fight_start {
                    Some(fs) => (
                        m.first_seen.checked_duration_since(fs).map(|d| d.as_secs_f64()),
                        m.last_seen.checked_duration_since(fs).map(|d| d.as_secs_f64()),
                    ),
                    None => (None, None),
                };
                json!({
                    "id": m.id,
                    "name": m.name,
                    "last_seen_secs": if timed_out { 15.0 } else { secs_since_last },
                    "encounter_secs": encounter_secs,
                    "first_seen_offset_secs": first_offset,
                    "last_seen_offset_secs": last_offset,
                    "first_log_ts": if m.first_log_ts != 0 { Value::from(m.first_log_ts) } else { Value::Null },
                    "last_log_ts": if m.last_log_ts != 0 { Value::from(m.last_log_ts) } else { Value::Null },
                    "is_dead": is_dead,
                    "is_active": is_active,
                })
            }).collect();

        // ── Per-mob-instance damage breakdown (keyed by mob ID as string) ──
        let mob_damage: Value = {
            let mut map = serde_json::Map::new();
            for (mob_id, by_player) in &self.mob_damage {
                let mut players: Vec<_> = by_player.iter().collect();
                players.sort_by_key(|b| std::cmp::Reverse(b.1.total_damage));
                let arr: Vec<Value> = players
                    .iter()
                    .map(|(player, stats)| {
                        let by_type: Vec<Value> = {
                            let mut rows: Vec<_> = stats.damage_by_type.iter().collect();
                            rows.sort_by(|a, b| b.1.cmp(a.1));
                            rows.iter()
                                .map(|(t, &d)| {
                                    json!({
                                        "type": t, "damage": d,
                                        "dps": (d as f64 / elapsed).round() as u64,
                                    })
                                })
                                .collect()
                        };
                        let spells: Vec<Value> = {
                            let mut rows: Vec<_> = stats.damage_by_spell.iter().collect();
                            rows.sort_by(|a, b| b.1.cmp(a.1));
                            rows.iter()
                                .map(|(s, &d)| {
                                    json!({
                                        "spell": s, "damage": d,
                                        "dps": (d as f64 / elapsed).round() as u64,
                                    })
                                })
                                .collect()
                        };
                        let classes = self
                            .player_classes
                            .get(*player)
                            .cloned()
                            .unwrap_or_default();
                        let class = classes.first().cloned().unwrap_or_default();
                        let level = self.player_levels.get(*player).copied().unwrap_or(0);
                        json!({
                            "name": *player,
                            "class": class,
                            "classes": classes,
                            "level": level,
                            "total_damage": stats.total_damage,
                            "dps": (stats.total_damage as f64 / elapsed).round() as u64,
                            "damage_by_type": by_type,
                            "spells": spells,
                        })
                    })
                    .collect();
                map.insert(mob_id.to_string(), Value::Array(arr));
            }
            Value::Object(map)
        };

        // ── Per-mob-instance tanking breakdown ──
        let mob_tanking: Value = {
            let mut map = serde_json::Map::new();
            for (mob_id, by_player) in &self.mob_tanking {
                let mut players: Vec<_> = by_player.iter().collect();
                players.sort_by_key(|b| std::cmp::Reverse(b.1.total_damage));
                let arr: Vec<Value> = players
                    .iter()
                    .map(|(player, stats)| {
                        let by_type: Vec<Value> = {
                            let mut rows: Vec<_> = stats.damage_by_type.iter().collect();
                            rows.sort_by(|a, b| b.1.cmp(a.1));
                            rows.iter()
                                .map(|(t, &d)| {
                                    json!({
                                        "type": t, "damage": d,
                                        "dps": (d as f64 / elapsed).round() as u64,
                                    })
                                })
                                .collect()
                        };
                        let avoidance: Vec<Value> = {
                            let mut rows: Vec<_> = stats.avoidance_by_type.iter().collect();
                            rows.sort_by(|a, b| b.1.cmp(a.1));
                            rows.iter()
                                .map(|(t, &c)| json!({ "type": t, "count": c }))
                                .collect()
                        };
                        let classes = self
                            .player_classes
                            .get(*player)
                            .cloned()
                            .unwrap_or_default();
                        let class = classes.first().cloned().unwrap_or_default();
                        let level = self.player_levels.get(*player).copied().unwrap_or(0);
                        json!({
                            "name": *player,
                            "class": class,
                            "classes": classes,
                            "level": level,
                            "total_damage": stats.total_damage,
                            "dps": (stats.total_damage as f64 / elapsed).round() as u64,
                            "damage_by_type": by_type,
                            "avoidance": avoidance,
                        })
                    })
                    .collect();
                map.insert(mob_id.to_string(), Value::Array(arr));
            }
            Value::Object(map)
        };

        // ── Per-mob-instance healing cast during encounter ──
        let mob_healing: Value = {
            let mut map = serde_json::Map::new();
            for (mob_id, by_healer) in &self.mob_healing {
                let mut healers: Vec<_> = by_healer.iter().collect();
                healers.sort_by_key(|b| std::cmp::Reverse(b.1.total_heals));
                let arr: Vec<Value> = healers
                    .iter()
                    .map(|(healer, stats)| {
                        let heals_by_spell: Vec<Value> = {
                            let mut rows: Vec<_> = stats.heals_by_spell.iter().collect();
                            rows.sort_by(|a, b| b.1.cmp(a.1));
                            rows.iter()
                                .map(|(s, &h)| {
                                    json!({
                                        "spell": s, "healed": h,
                                        "hps": (h as f64 / elapsed).round() as u64,
                                    })
                                })
                                .collect()
                        };
                        let classes = self
                            .player_classes
                            .get(*healer)
                            .cloned()
                            .unwrap_or_default();
                        let class = classes.first().cloned().unwrap_or_default();
                        let level = self.player_levels.get(*healer).copied().unwrap_or(0);
                        json!({
                            "name": *healer,
                            "class": class,
                            "classes": classes,
                            "level": level,
                            "total_heals": stats.total_heals,
                            "hps": (stats.total_heals as f64 / elapsed).round() as u64,
                            "heals_by_spell": heals_by_spell,
                        })
                    })
                    .collect();
                map.insert(mob_id.to_string(), Value::Array(arr));
            }
            Value::Object(map)
        };

        // ── Per-mob-instance heals received during encounter ──
        let mob_healed: Value =
            {
                let mut map = serde_json::Map::new();
                for (mob_id, by_healee) in &self.mob_healed {
                    let mut healees: Vec<_> = by_healee.iter().collect();
                    healees.sort_by_key(|b| std::cmp::Reverse(b.1.total_healed_received));
                    let arr: Vec<Value> = healees.iter().map(|(healee, stats)| {
                    let healed_received_by_spell: Vec<Value> = {
                        let mut rows: Vec<_> = stats.healed_received_by_spell.iter().collect();
                        rows.sort_by(|a, b| b.1.cmp(a.1));
                        rows.iter().map(|(s, &h)| json!({
                            "spell": s, "healed": h,
                            "hps": (h as f64 / elapsed).round() as u64,
                        })).collect()
                    };
                    let classes = self.player_classes.get(*healee).cloned().unwrap_or_default();
                    let class  = classes.first().cloned().unwrap_or_default();
                    let level = self.player_levels.get(*healee).copied().unwrap_or(0);
                    json!({
                        "name": *healee,
                        "class": class,
                        "classes": classes,
                        "level": level,
                        "total_healed_received": stats.total_healed_received,
                        "hps": (stats.total_healed_received as f64 / elapsed).round() as u64,
                        "healed_received_by_spell": healed_received_by_spell,
                    })
                }).collect();
                    map.insert(mob_id.to_string(), Value::Array(arr));
                }
                Value::Object(map)
            };

        // ── Global aggregate tanking (across all mob instances) ──
        let tanked: Vec<Value> = {
            let mut agg: HashMap<String, EntityCombatStats> = HashMap::new();
            for by_player in self.mob_tanking.values() {
                for (player, stats) in by_player {
                    let e = agg.entry(player.clone()).or_default();
                    e.total_damage += stats.total_damage;
                    for (k, v) in &stats.damage_by_type {
                        *e.damage_by_type.entry(k.clone()).or_default() += v;
                    }
                    for (k, v) in &stats.avoidance_by_type {
                        *e.avoidance_by_type.entry(k.clone()).or_default() += v;
                    }
                }
            }
            let mut v: Vec<_> = agg.iter().collect();
            v.sort_by_key(|b| std::cmp::Reverse(b.1.total_damage));
            v.iter()
                .map(|(name, stats)| {
                    let by_type: Vec<Value> = {
                        let mut rows: Vec<_> = stats.damage_by_type.iter().collect();
                        rows.sort_by(|a, b| b.1.cmp(a.1));
                        rows.iter()
                            .map(|(t, &d)| {
                                json!({
                                    "type": t, "damage": d,
                                    "dps": (d as f64 / elapsed).round() as u64,
                                })
                            })
                            .collect()
                    };
                    let avoidance: Vec<Value> = {
                        let mut rows: Vec<_> = stats.avoidance_by_type.iter().collect();
                        rows.sort_by(|a, b| b.1.cmp(a.1));
                        rows.iter()
                            .map(|(t, &c)| json!({ "type": t, "count": c }))
                            .collect()
                    };
                    let classes = self
                        .player_classes
                        .get(name.as_str())
                        .cloned()
                        .unwrap_or_default();
                    let class = classes.first().cloned().unwrap_or_default();
                    let level = self.player_levels.get(name.as_str()).copied().unwrap_or(0);
                    json!({
                        "name": name,
                        "class": class,
                        "classes": classes,
                        "level": level,
                        "total_damage": stats.total_damage,
                        "dps": (stats.total_damage as f64 / elapsed).round() as u64,
                        "damage_by_type": by_type,
                        "avoidance": avoidance,
                    })
                })
                .collect()
        };

        // ── Global aggregate resists (per player, across all spells) ──
        let resists: Vec<Value> =
            {
                let mut v: Vec<_> = self
                    .entities
                    .iter()
                    .filter(|(k, s)| {
                        !s.resists_by_spell.is_empty() && !self.confirmed_mobs.contains(k.as_str())
                    })
                    .collect();
                v.sort_by(|a, b| {
                    let sa: u64 = a.1.resists_by_spell.values().sum();
                    let sb: u64 = b.1.resists_by_spell.values().sum();
                    sb.cmp(&sa)
                });
                v.iter().map(|(name, stats)| {
                let by_spell: Vec<Value> = {
                    let mut rows: Vec<_> = stats.resists_by_spell.iter().collect();
                    rows.sort_by(|a, b| b.1.cmp(a.1));
                    rows.iter().map(|(s, &c)| json!({ "spell": s, "count": c })).collect()
                };
                let total: u64 = stats.resists_by_spell.values().sum();
                json!({ "name": name, "total_resists": total, "resists_by_spell": by_spell })
            }).collect()
            };

        // ── Active casts (entries older than 10 s are dropped) ──
        let casting: serde_json::Map<String, Value> = self
            .active_casts
            .iter()
            .filter_map(|(player, (spell, started))| {
                let secs = started.elapsed().as_secs_f64();
                if secs < 10.0 {
                    Some((player.clone(), json!({ "spell": spell })))
                } else {
                    None
                }
            })
            .collect();

        json!({
            "mob_name": self.mob_name,
            "fight_start_display": self.fight_start_display,
            "last_log_time": self.last_log_time,
            "fight_elapsed_secs": self.elapsed_secs(),
            "total_damage": self.total_damage(),
            "total_heals": self.total_heals(),
            "lines_parsed": self.lines_parsed,
            "entities": entities,
            "healers": healers,
            "healees": healees,
            "tanked": tanked,
            "resists": resists,
            "mob_list": mob_list,
            "mob_damage": mob_damage,
            "mob_tanking": mob_tanking,
            "mob_healing": mob_healing,
            "mob_healed": mob_healed,
            "casting": casting,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn two_player_state() -> CombatState {
        let mut state = CombatState::default();
        let mut rysk = EntityCombatStats {
            total_damage: 5000,
            ..Default::default()
        };
        *rysk.damage_by_type.entry("slash".to_owned()).or_default() = 3000;
        *rysk
            .damage_by_type
            .entry("backstab".to_owned())
            .or_default() = 2000;
        state.entities.insert("Rysk".to_owned(), rysk);
        let talodar = EntityCombatStats {
            total_damage: 2000,
            ..Default::default()
        };
        state.entities.insert("Talodar".to_owned(), talodar);
        state.known_players.insert("Rysk".to_owned());
        state.known_players.insert("Talodar".to_owned());
        state
    }

    // ── elapsed_secs ─────────────────────────────────────────────────────────────
    #[test]
    fn elapsed_no_fight() {
        let state = CombatState::default();
        assert_eq!(state.elapsed_secs(), 0.0);
    }
    #[test]
    fn elapsed_active_fight() {
        let state = CombatState {
            fight_start: Some(Instant::now()),
            ..Default::default()
        };
        let e = state.elapsed_secs();
        assert!((0.0..1.0).contains(&e));
    }
    #[test]
    fn elapsed_ended_fight() {
        let mut state = CombatState::default();
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        state.fight_start = Some(start);
        state.fight_end = Some(Instant::now());
        let e = state.elapsed_secs();
        assert!((0.01..1.0).contains(&e), "elapsed {e}");
    }

    // ── total_damage / total_heals ────────────────────────────────────────────────
    #[test]
    fn total_damage_empty() {
        assert_eq!(CombatState::default().total_damage(), 0);
    }
    #[test]
    fn total_damage_sum() {
        assert_eq!(two_player_state().total_damage(), 7000);
    }
    #[test]
    fn total_heals_sum() {
        let mut state = CombatState::default();
        let e = EntityCombatStats {
            total_heals: 3000,
            ..Default::default()
        };
        state.entities.insert("Healer".to_owned(), e);
        assert_eq!(state.total_heals(), 3000);
    }

    // ── sorted_by_damage ─────────────────────────────────────────────────────────
    #[test]
    fn sorted_by_damage_order() {
        let state = two_player_state();
        let sorted = state.sorted_by_damage();
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted[0].0, "Rysk");
        assert_eq!(sorted[0].1.total_damage, 5000);
        assert_eq!(sorted[1].0, "Talodar");
    }
    #[test]
    fn sorted_by_damage_excludes_confirmed_mobs() {
        let mut state = two_player_state();
        let mob = EntityCombatStats {
            total_damage: 9999,
            ..Default::default()
        };
        state.entities.insert("a goblin".to_owned(), mob);
        state.confirmed_mobs.insert("a goblin".to_owned());
        assert!(!state
            .sorted_by_damage()
            .iter()
            .any(|(n, _)| *n == "a goblin"));
    }

    // ── healers_sorted / healees_sorted ───────────────────────────────────────────
    #[test]
    fn healers_sorted_order() {
        let mut state = CombatState::default();
        let h1 = EntityCombatStats {
            total_heals: 5000,
            ..Default::default()
        };
        let h2 = EntityCombatStats {
            total_heals: 10000,
            ..Default::default()
        };
        let d = EntityCombatStats {
            total_heals: 0,
            ..Default::default()
        };
        state.entities.insert("Healer1".to_owned(), h1);
        state.entities.insert("Healer2".to_owned(), h2);
        state.entities.insert("Damager".to_owned(), d);
        let sorted = state.healers_sorted();
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted[0].1.total_heals, 10000);
    }
    #[test]
    fn healees_sorted_order() {
        let mut state = CombatState::default();
        let e1 = EntityCombatStats {
            total_healed_received: 3000,
            ..Default::default()
        };
        let e2 = EntityCombatStats {
            total_healed_received: 8000,
            ..Default::default()
        };
        state.entities.insert("Tank1".to_owned(), e1);
        state.entities.insert("Tank2".to_owned(), e2);
        let sorted = state.healees_sorted();
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted[0].1.total_healed_received, 8000);
    }

    // ── mob_players_by_damage / mob_players_by_tanking ────────────────────────────
    #[test]
    fn mob_players_by_damage_empty() {
        assert!(CombatState::default().mob_players_by_damage(0).is_empty());
    }
    #[test]
    fn mob_players_by_damage_sorted() {
        let mut state = CombatState::default();
        let p1 = EntityCombatStats {
            total_damage: 1000,
            ..Default::default()
        };
        let p2 = EntityCombatStats {
            total_damage: 3000,
            ..Default::default()
        };
        let mut by_player = HashMap::new();
        by_player.insert("Rysk".to_owned(), p1);
        by_player.insert("Talodar".to_owned(), p2);
        state.mob_damage.insert(42, by_player);
        let sorted = state.mob_players_by_damage(42);
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted[0].0, "Talodar");
        assert_eq!(sorted[0].1.total_damage, 3000);
    }
    #[test]
    fn mob_players_by_tanking_empty() {
        assert!(CombatState::default().mob_players_by_tanking(99).is_empty());
    }

    // ── to_api_json ───────────────────────────────────────────────────────────────
    #[test]
    fn to_api_json_required_keys() {
        let json = CombatState::default().to_api_json();
        for key in &[
            "mob_name",
            "entities",
            "healers",
            "healees",
            "tanked",
            "resists",
            "total_damage",
            "total_heals",
            "mob_list",
            "mob_damage",
            "mob_tanking",
            "mob_healing",
            "mob_healed",
            "casting",
        ] {
            assert!(json.get(key).is_some(), "missing key: {key}");
        }
    }
    #[test]
    fn to_api_json_entity_shape() {
        let mut state = CombatState::default();
        let stats = EntityCombatStats {
            total_damage: 5000,
            crit_count: 3,
            ..Default::default()
        };
        state.entities.insert("Rysk".to_owned(), stats);
        state.known_players.insert("Rysk".to_owned());
        state.fight_start = Some(Instant::now());
        let json = state.to_api_json();
        let entities = json["entities"].as_array().unwrap();
        assert_eq!(entities.len(), 1);
        let e = &entities[0];
        assert_eq!(e["name"].as_str().unwrap(), "Rysk");
        assert_eq!(e["total_damage"].as_u64().unwrap(), 5000);
        assert_eq!(e["crit_count"].as_u64().unwrap(), 3);
    }
}
