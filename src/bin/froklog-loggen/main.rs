//! froklog-loggen — pseudo-random EverQuest log generator.
//!
//! Simulates a group of 2–8 players fighting mobs across random encounters,
//! producing a log file whose lines match the patterns the froklog parser
//! expects.  All mob, spell, gear, and chat data is loaded from TOML files
//! (embedded as defaults; override with --config-dir).
//!
//! Usage:
//!   froklog-loggen --player-name Talodar --players 5 --encounters 30
//!   froklog-loggen --seed 42 --zone hate --encounters 50
//!   froklog-loggen --config-dir /path/to/my/data --zone mystic

mod chatmod;
mod config;
mod namegen;

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::Instant;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use clap::Parser;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

#[cfg(feature = "neural")]
use chatmod::{try_load_neural_backend, NeuralCtx};
use chatmod::{ChatCtx, ChatDispatch, ChatTrigger, Personality, SimChatState};
use config::{GameConfig, LootEntry, MobDef, SpellDef, SpellKind, ZoneDef};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "froklog-loggen",
    about = "Generate a pseudo-random EverQuest log"
)]
struct Args {
    /// Number of players in the group (2–8; default random)
    #[arg(long)]
    players: Option<usize>,

    /// Name of the "you" character (player 0)
    #[arg(long)]
    player_name: Option<String>,

    /// Output file path (default: stdout)
    #[arg(long)]
    output: Option<String>,

    /// Number of mob encounters to simulate (omit for infinite)
    #[arg(long)]
    encounters: Option<u32>,

    /// RNG seed (omit for random)
    #[arg(long)]
    seed: Option<u64>,

    /// Zone override (key or partial name match)
    #[arg(long)]
    zone: Option<String>,

    /// Directory containing zones.toml / spells.toml / gear.toml / chat.toml
    #[arg(long)]
    config_dir: Option<String>,

    /// Stream output paced to simulated timestamps
    #[arg(long, default_value_t = false)]
    realtime: bool,

    /// Difficulty level: 0=default, 1=+25% mob HP, 2=+50%, 3=+75%, 4=+100%
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=4))]
    difficulty: u8,

    /// Simulated seconds to advance between encounters
    #[arg(long, default_value_t = 2)]
    gap: u32,

    /// Puller intensity: 0=always single, 1=occasional double, 2=default, 3=frequent adds, 4=always adds
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(0..=4))]
    intensity: u8,

    /// Chat volume: 0=silent, 1=quiet, 2=default, 3=chatty, 4=very chatty
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(0..=4))]
    chat: u8,

    /// Wall-clock duration limit (e.g. 4h, 5m, 60s); stops after this much real time
    #[arg(long, value_parser = parse_duration)]
    duration: Option<std::time::Duration>,

    /// Path to a directory containing model.onnx, vocab.model, and
    /// model_meta.json for the neural chat backend.  When omitted, the binary's
    /// own directory is searched.  Ignored if the `neural` feature is absent.
    #[arg(long)]
    model_dir: Option<String>,
}

// ── Duration parser ─────────────────────────────────────────────────────────

fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let (num_part, unit) = s
        .find(|c: char| c.is_alphabetic())
        .map(|i| s.split_at(i))
        .ok_or_else(|| format!("missing unit in '{}' (use s, m, or h)", s))?;
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("invalid number '{}'", num_part))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        other => return Err(format!("unknown unit '{}' (use s, m, or h)", other)),
    };
    Ok(std::time::Duration::from_secs(secs))
}

// ── Output context ────────────────────────────────────────────────────────────

struct Ctx {
    out: Box<dyn Write>,
    realtime: bool,
    wall_start: Instant,
    sim_base: NaiveDateTime,
}

impl Ctx {
    fn new(out: Box<dyn Write>, realtime: bool, sim_base: NaiveDateTime) -> Self {
        Self {
            out,
            realtime,
            wall_start: Instant::now(),
            sim_base,
        }
    }

    fn emit(&mut self, dt: NaiveDateTime, msg: &str) {
        if self.realtime {
            let sim_ms = (dt - self.sim_base).num_milliseconds();
            if sim_ms > 0 {
                let target = std::time::Duration::from_millis(sim_ms as u64);
                let elapsed = self.wall_start.elapsed();
                if target > elapsed {
                    std::thread::sleep(target - elapsed);
                }
            }
        }
        writeln!(self.out, "{} {}", dt.format("[%a %b %d %H:%M:%S %Y]"), msg).ok();
        if self.realtime {
            self.out.flush().ok();
        }
    }
}

fn adv(dt: &mut NaiveDateTime, secs: i64) {
    *dt += Duration::seconds(secs);
}

// ── Class / role enums ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Warrior,
    Paladin,
    Shadowknight,
    Ranger,
    Rogue,
    Monk,
    Bard,
    Druid,
    Cleric,
    Shaman,
    Enchanter,
    Wizard,
    Magician,
    Necromancer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Tank,
    Healer,
    Debuffer,
    Dps,
}

const ALL_CLASSES: &[Class] = &[
    Class::Warrior,
    Class::Paladin,
    Class::Shadowknight,
    Class::Ranger,
    Class::Rogue,
    Class::Monk,
    Class::Bard,
    Class::Druid,
    Class::Cleric,
    Class::Shaman,
    Class::Enchanter,
    Class::Wizard,
    Class::Magician,
    Class::Necromancer,
];

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Warrior => "Warrior",
            Class::Paladin => "Paladin",
            Class::Shadowknight => "Shadowknight",
            Class::Ranger => "Ranger",
            Class::Rogue => "Rogue",
            Class::Monk => "Monk",
            Class::Bard => "Bard",
            Class::Druid => "Druid",
            Class::Cleric => "Cleric",
            Class::Shaman => "Shaman",
            Class::Enchanter => "Enchanter",
            Class::Wizard => "Wizard",
            Class::Magician => "Magician",
            Class::Necromancer => "Necromancer",
        }
    }

    fn abbrev(self) -> &'static str {
        match self {
            Class::Warrior => "WAR",
            Class::Paladin => "PAL",
            Class::Shadowknight => "SHD",
            Class::Ranger => "RNG",
            Class::Rogue => "ROG",
            Class::Monk => "MNK",
            Class::Bard => "BRD",
            Class::Druid => "DRU",
            Class::Cleric => "CLR",
            Class::Shaman => "SHM",
            Class::Enchanter => "ENC",
            Class::Wizard => "WIZ",
            Class::Magician => "MAG",
            Class::Necromancer => "NEC",
        }
    }

    fn role(self) -> Role {
        match self {
            Class::Warrior | Class::Paladin | Class::Shadowknight => Role::Tank,
            Class::Cleric | Class::Druid | Class::Shaman => Role::Healer,
            Class::Enchanter => Role::Debuffer,
            _ => Role::Dps,
        }
    }

    fn hp_max(self) -> i32 {
        match self {
            Class::Warrior => 11000,
            Class::Paladin | Class::Shadowknight => 8500,
            Class::Monk => 7200,
            Class::Ranger | Class::Rogue => 6000,
            Class::Bard | Class::Cleric | Class::Shaman => 5500,
            _ => 4200,
        }
    }

    fn mana_max(self) -> i32 {
        match self {
            Class::Warrior | Class::Rogue | Class::Monk => 0,
            Class::Bard => 2500,
            Class::Ranger | Class::Shadowknight => 3200,
            Class::Paladin => 4200,
            Class::Shaman | Class::Druid => 6800,
            Class::Cleric | Class::Magician | Class::Enchanter => 8200,
            Class::Necromancer | Class::Wizard => 9800,
        }
    }

    fn melee_attacks(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Class::Warrior => &[
                ("slash", "slashes"),
                ("hit", "hits"),
                ("bash", "bashes"),
                ("strike", "strikes"),
            ],
            Class::Paladin => &[
                ("slash", "slashes"),
                ("crush", "crushes"),
                ("strike", "strikes"),
            ],
            Class::Shadowknight => &[("slash", "slashes"), ("hit", "hits"), ("strike", "strikes")],
            Class::Ranger => &[
                ("slash", "slashes"),
                ("pierce", "pierces"),
                ("shoot", "shoots"),
            ],
            Class::Rogue => &[
                ("pierce", "pierces"),
                ("backstab", "backstabs"),
                ("cleave", "cleaves"),
                ("slash", "slashes"),
            ],
            Class::Monk => &[
                ("punch", "punches"),
                ("kick", "kicks"),
                ("strike", "strikes"),
                ("frenzy", "frenzies"),
            ],
            Class::Bard => &[("slash", "slashes"), ("pierce", "pierces")],
            _ => &[("crush", "crushes")],
        }
    }

    fn attacks_per_round(self) -> u32 {
        match self {
            Class::Warrior | Class::Paladin | Class::Shadowknight => 3,
            Class::Ranger => 3,
            Class::Rogue | Class::Monk => 4,
            Class::Bard => 2,
            _ => 1,
        }
    }

    fn attack_delay(self) -> u32 {
        match self {
            Class::Warrior | Class::Paladin | Class::Shadowknight => 3,
            Class::Ranger | Class::Rogue | Class::Monk => 2,
            Class::Bard => 3,
            _ => 5,
        }
    }

    fn dmg_range(self) -> (u32, u32) {
        match self {
            Class::Warrior => (85, 200),
            Class::Paladin => (70, 180),
            Class::Shadowknight => (75, 190),
            Class::Ranger => (50, 155),
            Class::Rogue => (35, 135),
            Class::Monk => (45, 160),
            Class::Bard => (30, 105),
            Class::Druid | Class::Shaman => (20, 65),
            Class::Cleric => (20, 58),
            _ => (15, 48),
        }
    }

    fn backstab_range(self) -> Option<(u32, u32)> {
        if self == Class::Rogue {
            Some((85, 390))
        } else {
            None
        }
    }

    fn has_ds(self) -> bool {
        matches!(
            self,
            Class::Warrior | Class::Paladin | Class::Shadowknight | Class::Ranger
        )
    }
}

// ── Triple-class combo ────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct PlayerClasses {
    primary: Class,
    secondary: Option<Class>,
    tertiary: Option<Class>,
}

impl PlayerClasses {
    fn who_display(&self) -> String {
        match (&self.secondary, &self.tertiary) {
            (Some(s), Some(t)) => {
                format!("{}/{}/{}", self.primary.abbrev(), s.abbrev(), t.abbrev())
            }
            (Some(s), None) => format!("{}/{}", self.primary.abbrev(), s.abbrev()),
            _ => self.primary.abbrev().to_string(),
        }
    }

    fn all_class_names(&self) -> Vec<&'static str> {
        let mut names = vec![self.primary.name()];
        if let Some(s) = self.secondary {
            names.push(s.name());
        }
        if let Some(t) = self.tertiary {
            names.push(t.name());
        }
        names
    }

    fn role(&self) -> Role {
        self.primary.role()
    }
    fn hp_max(&self) -> i32 {
        self.primary.hp_max()
    }

    fn mana_max(&self) -> i32 {
        let p = self.primary.mana_max();
        let s = self.secondary.map(|c| c.mana_max()).unwrap_or(0);
        let t = self.tertiary.map(|c| c.mana_max()).unwrap_or(0);
        p.max(s).max(t)
    }

    fn melee_attacks(&self) -> &'static [(&'static str, &'static str)] {
        self.primary.melee_attacks()
    }

    fn attacks_per_round(&self) -> u32 {
        self.primary.attacks_per_round()
    }
    fn attack_delay(&self) -> u32 {
        self.primary.attack_delay()
    }
    fn dmg_range(&self) -> (u32, u32) {
        self.primary.dmg_range()
    }

    fn backstab_range(&self) -> Option<(u32, u32)> {
        for c in [Some(self.primary), self.secondary, self.tertiary]
            .iter()
            .flatten()
        {
            if let Some(r) = c.backstab_range() {
                return Some(r);
            }
        }
        None
    }

    fn has_ds(&self) -> bool {
        [Some(self.primary), self.secondary, self.tertiary]
            .iter()
            .flatten()
            .any(|c| c.has_ds())
    }
}

// ── Player and Mob structs ────────────────────────────────────────────────────

#[derive(Clone)]
struct Player {
    name: String,
    race: String,
    guild: String,
    level: u32,
    classes: PlayerClasses,
    personality: Personality,
    sim_state: SimChatState,
    hp: i32,
    hp_max: i32,
    mana: i32,
    mana_max: i32,
    has_ds: bool,
    ds_dmg: u32,
    next_attack_sec: u32,
    next_spell_sec: u32,
    debuff_applied: bool,
    dots_active: u32,
    consecutive_misses: u32,
    mana_alerted: u8,
    inventory: Vec<String>,
}

#[derive(Clone)]
struct Mob {
    article: String,
    name: String,
    hp: i32,
    dmg: (u32, u32),
    attack_delay: u32,
    next_attack_sec: u32,
    next_spell_sec: u32,
    spells: Vec<String>,
    loot_table: Vec<LootEntry>,
    has_thorns: bool,
    thorns_dmg: u32,
    target_player: usize,
    slowed: bool,
    snared: bool,
    mezzed: bool,
}

impl Mob {
    fn full_name(&self) -> String {
        format!("{} {}", self.article, self.name)
    }

    fn full_name_cap(&self) -> String {
        let s = self.full_name();
        let mut c = s.chars();
        match c.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        }
    }
}

struct ActiveDot {
    caster: usize,
    spell: String,
    dmg_lo: u32,
    dmg_hi: u32,
    remaining: u32,
    next_tick: u32,
    tick_secs: u32,
    mob_idx: usize,
}

struct ActiveHot {
    caster: usize,
    spell: String,
    heal_lo: u32,
    heal_hi: u32,
    remaining: u32,
    next_tick: u32,
    tick_secs: u32,
    target: usize,
}

struct PendingCast {
    caster: usize,
    spell: SpellDef,
    complete_sec: u32,
    target_mob: Option<usize>,
    target_player: Option<usize>,
    // HP of the heal target at cast time (for near-death-save detection)
    target_hp_at_cast: Option<i32>,
}

// ── Simulation state ──────────────────────────────────────────────────────────

struct Sim {
    players: Vec<Player>,
    mobs: Vec<Mob>,
    dots: Vec<ActiveDot>,
    hots: Vec<ActiveHot>,
    casts: Vec<PendingCast>,
    sec: u32,
    base: NaiveDateTime,
    rng: StdRng,
    zone: ZoneDef,
    loot_bag: Vec<(String, u32)>,
    encounters_done: u32,
    difficulty: u8,
    intensity: u8,
    chat_level: u8,
    #[cfg(feature = "neural")]
    neural: Option<froklog::chat::NeuralBackend>,
}

impl Sim {
    fn dt(&self) -> NaiveDateTime {
        self.base + Duration::seconds(self.sec as i64)
    }

    fn rand_range(&mut self, lo: u32, hi: u32) -> u32 {
        if lo >= hi {
            return lo;
        }
        self.rng.gen_range(lo..=hi)
    }

    fn roll(&mut self, pct: u32) -> bool {
        self.rng.gen_range(0u32..100) < pct
    }
}

// ── Formatting helpers ────────────────────────────────────────────────────────

#[allow(dead_code)]
fn pname(players: &[Player], idx: usize) -> &str {
    if idx == 0 {
        "You"
    } else {
        &players[idx].name
    }
}

fn ptgt_heal(players: &[Player], idx: usize) -> String {
    if idx == 0 {
        "you".to_string()
    } else {
        players[idx].name.clone()
    }
}

fn copper_to_str(c: u32) -> String {
    let pp = c / 1000;
    let rem = c % 1000;
    let gp = rem / 100;
    let rem = rem % 100;
    let sp = rem / 10;
    let cp = rem % 10;
    let mut parts: Vec<String> = Vec::new();
    if pp > 0 {
        parts.push(format!("{} platinum", pp));
    }
    if gp > 0 {
        parts.push(format!("{} gold", gp));
    }
    if sp > 0 {
        parts.push(format!("{} silver", sp));
    }
    if cp > 0 {
        parts.push(format!("{} copper", cp));
    }
    if parts.is_empty() {
        return "nothing".to_string();
    }
    if parts.len() == 1 {
        return parts.remove(0);
    }
    let last = parts.pop().unwrap();
    format!("{} and {}", parts.join(", "), last)
}

fn coin_from_corpse(c: u32) -> String {
    copper_to_str(c)
}

fn vendor_price_str(c: u32) -> String {
    let pp = c / 1000;
    let rem = c % 1000;
    let gp = rem / 100;
    let rem = rem % 100;
    let sp = rem / 10;
    let cp = rem % 10;
    let mut parts: Vec<String> = Vec::new();
    if pp > 0 {
        parts.push(format!("{} platinum", pp));
    }
    if gp > 0 {
        parts.push(format!("{} gold", gp));
    }
    if sp > 0 {
        parts.push(format!("{} silver", sp));
    }
    if cp > 0 {
        parts.push(format!("{} copper", cp));
    }
    if parts.is_empty() {
        return "nothing".to_string();
    }
    parts.join(" ")
}

// ── Spell helpers (operate on loaded config data) ─────────────────────────────

fn player_spells<'a>(player: &Player, cfg: &'a GameConfig) -> Vec<&'a SpellDef> {
    player
        .classes
        .all_class_names()
        .iter()
        .flat_map(|&name| cfg.spells_for(name))
        .collect()
}

/// Return the highest-level spell of `kind` that the player's level allows.
fn pick_best<'a>(player: &Player, cfg: &'a GameConfig, kind: SpellKind) -> Option<&'a SpellDef> {
    player_spells(player, cfg)
        .into_iter()
        .filter(|s| s.kind == kind && s.level <= player.level && s.mana_cost >= 0)
        .max_by_key(|s| s.level)
}

/// Best direct/HoT/promised heal available to this player at their level.
fn pick_heal_spell<'a>(player: &Player, cfg: &'a GameConfig) -> Option<&'a SpellDef> {
    player_spells(player, cfg)
        .into_iter()
        .filter(|s| {
            matches!(
                s.kind,
                SpellKind::DirectHeal | SpellKind::HoT | SpellKind::PromisedHeal
            ) && s.level <= player.level
        })
        .max_by_key(|s| s.level)
}

// ── Combat helpers ────────────────────────────────────────────────────────────

fn tank_idx(players: &[Player]) -> usize {
    players
        .iter()
        .enumerate()
        .find(|(_, p)| p.classes.role() == Role::Tank)
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn lowest_hp_player(players: &[Player]) -> usize {
    players
        .iter()
        .enumerate()
        .min_by_key(|(_, p)| p.hp)
        .map(|(i, _)| i)
        .unwrap_or(0)
}

// ── /who simulation ───────────────────────────────────────────────────────────

fn emit_who(sim: &Sim, ctx: &mut Ctx) {
    let dt = sim.dt();
    ctx.emit(dt, "[PLAYERS IN EVERQUEST]");
    ctx.emit(dt, "---------------------------");
    for p in &sim.players {
        ctx.emit(
            dt,
            &format!(
                "[{} {}] {} ({}) <{}> ZONE: {}",
                p.level,
                p.classes.who_display(),
                p.name,
                p.race,
                p.guild,
                sim.zone.name,
            ),
        );
    }
    ctx.emit(
        dt,
        &format!("There are {} players in EverQuest.", sim.players.len()),
    );
}

// ── Group builder ─────────────────────────────────────────────────────────────

fn build_group(
    you_name: &str,
    count: usize,
    rng: &mut StdRng,
    guild: &str,
    min_level: u32,
) -> Vec<Player> {
    let you_primary = {
        let candidates = [
            Class::Ranger,
            Class::Rogue,
            Class::Monk,
            Class::Wizard,
            Class::Magician,
            Class::Necromancer,
            Class::Druid,
            Class::Cleric,
        ];
        *candidates.choose(rng).unwrap()
    };

    // Build primary class list for the rest of the group
    let need_tank = you_primary.role() != Role::Tank;
    let mut need_healer = you_primary.role() != Role::Healer;

    let mut primaries: Vec<Class> = Vec::new();

    if count >= 2 && need_tank {
        let tanks = [Class::Warrior, Class::Paladin, Class::Shadowknight];
        primaries.push(*tanks.choose(rng).unwrap());
    }
    if count >= 3 && need_healer {
        let healers = [Class::Cleric, Class::Druid, Class::Shaman];
        primaries.push(*healers.choose(rng).unwrap());
        need_healer = false;
    }
    if count >= 5
        && !need_healer
        && primaries
            .iter()
            .filter(|c| c.role() == Role::Healer)
            .count()
            == 0
    {
        let healers = [Class::Cleric, Class::Druid, Class::Shaman];
        primaries.push(*healers.choose(rng).unwrap());
    }

    let dps = [
        Class::Ranger,
        Class::Rogue,
        Class::Monk,
        Class::Wizard,
        Class::Magician,
        Class::Necromancer,
        Class::Enchanter,
        Class::Bard,
    ];
    while primaries.len() < count - 1 {
        primaries.push(*dps.choose(rng).unwrap());
    }
    primaries.shuffle(rng);

    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    used_names.insert(you_name.to_string());

    let mut players: Vec<Player> = Vec::with_capacity(count);

    // Player 0 = "you"
    let you_classes = make_triple_class(you_primary, rng);
    let you_hp = you_classes.hp_max();
    let you_mana = you_classes.mana_max();
    let level_lo = min_level.clamp(10, 50);
    // Pick a group-wide max level, then constrain the minimum so no player is
    // more than 1.5x lower than the highest (i.e. max <= min * 1.5).
    let group_max_level = rng.gen_range(level_lo..=50);
    let group_min_level = ((group_max_level as f32 / 1.5).ceil() as u32).max(level_lo);
    let you_race = namegen::RACES[rng.gen_range(0..namegen::RACES.len())].to_string();
    players.push(Player {
        name: you_name.to_string(),
        race: you_race,
        guild: guild.to_string(),
        level: rng.gen_range(group_min_level..=group_max_level),
        personality: Personality::random(rng),
        sim_state: SimChatState::new(),
        classes: you_classes.clone(),
        hp: you_hp,
        hp_max: you_hp,
        mana: you_mana,
        mana_max: you_mana,
        has_ds: you_classes.has_ds(),
        ds_dmg: if you_classes.has_ds() {
            20 + rng.gen_range(0u32..15)
        } else {
            0
        },
        next_attack_sec: rng.gen_range(0u32..3),
        next_spell_sec: rng.gen_range(5u32..15),
        debuff_applied: false,
        dots_active: 0,
        consecutive_misses: 0,
        mana_alerted: 0,
        inventory: Vec::new(),
    });

    for primary in primaries.iter().take(count - 1) {
        let nm = loop {
            let candidate = namegen::generate_name(rng);
            if used_names.insert(candidate.clone()) {
                break candidate;
            }
        };
        let classes = make_triple_class(*primary, rng);
        let hp = classes.hp_max();
        let mana = classes.mana_max();
        let race = namegen::RACES[rng.gen_range(0..namegen::RACES.len())].to_string();
        players.push(Player {
            name: nm,
            race,
            guild: guild.to_string(),
            level: rng.gen_range(group_min_level..=group_max_level),
            personality: Personality::random(rng),
            sim_state: SimChatState::new(),
            classes: classes.clone(),
            hp,
            hp_max: hp,
            mana,
            mana_max: mana,
            has_ds: classes.has_ds(),
            ds_dmg: if classes.has_ds() {
                18 + rng.gen_range(0u32..15)
            } else {
                0
            },
            next_attack_sec: rng.gen_range(0u32..3),
            next_spell_sec: rng.gen_range(5u32..15),
            debuff_applied: false,
            dots_active: 0,
            consecutive_misses: 0,
            mana_alerted: 0,
            inventory: Vec::new(),
        });
    }
    players
}

fn make_triple_class(primary: Class, rng: &mut StdRng) -> PlayerClasses {
    // Pick 2 additional classes (any class, not same as primary)
    let mut pool: Vec<Class> = ALL_CLASSES
        .iter()
        .filter(|&&c| c != primary)
        .copied()
        .collect();
    pool.shuffle(rng);

    PlayerClasses {
        primary,
        secondary: Some(pool[0]),
        tertiary: Some(pool[1]),
    }
}

// ── Mob spawner ───────────────────────────────────────────────────────────────

fn spawn_mob(
    tmpl: &MobDef,
    cfg: &GameConfig,
    rng: &mut StdRng,
    tank_idx: usize,
    difficulty: u8,
) -> Mob {
    let base_hp = rng.gen_range(tmpl.hp_lo..=tmpl.hp_hi) as i32;
    let hp = (base_hp as f64 * (1.0 + difficulty as f64 * 0.25)).round() as i32;
    let has_thorns = rng.gen_range(0u32..100) < 30;
    let thorns_dmg = if has_thorns {
        rng.gen_range(15u32..40)
    } else {
        0
    };
    let loot_table = cfg.loot_for(&tmpl.loot_table).to_vec();
    Mob {
        article: tmpl.article.clone(),
        name: tmpl.name.clone(),
        hp,
        dmg: (tmpl.dmg_lo, tmpl.dmg_hi),
        attack_delay: tmpl.attack_delay,
        next_attack_sec: rng.gen_range(0u32..tmpl.attack_delay.max(1)),
        next_spell_sec: rng.gen_range(8u32..20),
        spells: tmpl.spells.clone(),
        loot_table,
        has_thorns,
        thorns_dmg,
        target_player: tank_idx,
        slowed: false,
        snared: false,
        mezzed: false,
    }
}

// ── Encounter ─────────────────────────────────────────────────────────────────

fn run_encounter(sim: &mut Sim, ctx: &mut Ctx, cfg: &GameConfig) {
    let num_mobs = match sim.intensity {
        0 => 1,
        1 => {
            if sim.roll(12) {
                2
            } else {
                1
            }
        }
        2 => {
            if sim.roll(25) {
                2
            } else if sim.roll(10) {
                3
            } else {
                1
            }
        }
        3 => {
            if sim.roll(50) {
                if sim.roll(25) {
                    3
                } else {
                    2
                }
            } else {
                1
            }
        }
        _ => {
            // intensity 4: almost always adds
            if sim.roll(75) {
                if sim.roll(40) {
                    3
                } else {
                    2
                }
            } else {
                1
            }
        }
    };

    let tank = tank_idx(&sim.players);
    let tmpl_count = sim.zone.mobs.len();

    for _ in 0..num_mobs {
        let tidx = sim.rng.gen_range(0..tmpl_count);
        let tmpl = sim.zone.mobs[tidx].clone();
        let tgt = if sim.mobs.is_empty() {
            tank
        } else {
            sim.rng.gen_range(0..sim.players.len())
        };
        let mob = spawn_mob(&tmpl, cfg, &mut sim.rng, tgt, sim.difficulty);
        sim.mobs.push(mob);
    }

    for p in sim.players.iter_mut() {
        p.debuff_applied = false;
        p.dots_active = 0;
        p.consecutive_misses = 0;
        p.mana_alerted = 0;
    }

    // Create unified chat dispatcher — uses neural backend when available
    let group_size = sim.players.len() as u8;
    let zone_name: String = sim.zone.name.clone();
    #[cfg(feature = "neural")]
    let mut chat: ChatDispatch<'_> = {
        let backend = sim.neural.take();
        if let Some(b) = backend {
            ChatDispatch::Neural(NeuralCtx::new(b, &cfg.phrases, sim.chat_level))
        } else {
            ChatDispatch::Phrasebook(ChatCtx::new(&cfg.phrases, sim.chat_level))
        }
    };
    #[cfg(not(feature = "neural"))]
    let mut chat: ChatDispatch<'_> =
        ChatDispatch::Phrasebook(ChatCtx::new(&cfg.phrases, sim.chat_level));

    // ── Pull announcement ─────────────────────────────────────────────────────
    if let Some(puller) = pick_puller(&sim.players, &mut sim.rng) {
        let mob_first = sim.mobs[0].full_name();
        let trigger = ChatTrigger::Incoming {
            count: num_mobs as u32,
            mob: &mob_first,
        };
        let cur_sec = sim.sec;
        let personality = sim.players[puller].personality.clone();
        let topic = trigger.topic_hint();
        if let Some(msg) = chat.respond(
            &trigger,
            &personality,
            &sim.players[puller].sim_state,
            group_size,
            Some(zone_name.as_str()),
            cur_sec,
            &mut sim.rng,
        ) {
            sim.players[puller].sim_state.mark_spoke(cur_sec);
            if let Some(t) = topic {
                sim.players[puller].sim_state.mark_topic_spoken(t, cur_sec);
            }
            emit_group_chat(sim, ctx, puller, &msg);
        }
    }

    // ── CC announcement for multi-mob encounters ──────────────────────────────
    if num_mobs > 1 {
        if let Some(cc) = pick_cc_player(&sim.players, &mut sim.rng) {
            let mob_extra = sim.mobs[1].full_name();
            if let Some(msg) = chat.pick_template("ExtraMobCC", &mut sim.rng) {
                let msg = msg.replace("{mob}", &mob_extra);
                emit_group_chat(sim, ctx, cc, &msg);
            }
        }
    }

    // Track which mobs have already triggered a being-hit alert
    let mut hit_alerted: Vec<bool> = vec![false; sim.mobs.len()];

    let mut enc_sec = 0u32;

    while !sim.mobs.is_empty() && enc_sec < 300 {
        sim.sec += 1;
        enc_sec += 1;
        let cur = sim.sec;
        let dt = sim.dt();

        // ── Mob attacks ──────────────────────────────────────────────────────
        let mob_count = sim.mobs.len();
        for mi in 0..mob_count {
            if sim.mobs[mi].hp <= 0 {
                continue;
            }
            // Mezzed mobs can't act
            if sim.mobs[mi].mezzed {
                continue;
            }

            // Mob melee
            if sim.mobs[mi].next_attack_sec <= cur {
                let delay = sim.mobs[mi].attack_delay;
                sim.mobs[mi].next_attack_sec = cur + delay + sim.rng.gen_range(0u32..2);

                let tgt = sim.mobs[mi].target_player;
                let slow_factor = if sim.mobs[mi].slowed { 2 } else { 1 };
                let dmg_lo = sim.mobs[mi].dmg.0 / slow_factor;
                let dmg_hi = sim.mobs[mi].dmg.1 / slow_factor;
                let miss_roll = sim.rng.gen_range(0u32..100);
                let mob_cap = sim.mobs[mi].full_name_cap();
                let mob_name = sim.mobs[mi].full_name();

                if miss_roll < 22 {
                    let miss_type = if sim.roll(50) {
                        let pn = if tgt == 0 {
                            "YOU".to_string()
                        } else {
                            sim.players[tgt].name.clone()
                        };
                        format!("{} dodges!", pn)
                    } else {
                        "misses!".to_string()
                    };
                    if tgt == 0 {
                        ctx.emit(
                            dt,
                            &format!("{} tries to hit YOU, but {}", mob_cap, miss_type),
                        );
                    } else {
                        ctx.emit(
                            dt,
                            &format!(
                                "{} tries to hit {}, but {}",
                                mob_cap, sim.players[tgt].name, miss_type
                            ),
                        );
                    }
                } else {
                    let dmg = sim.rand_range(dmg_lo, dmg_hi);
                    sim.players[tgt].hp -= dmg as i32;

                    if tgt == 0 {
                        ctx.emit(
                            dt,
                            &format!("{} hits YOU for {} points of damage.", mob_cap, dmg),
                        );
                    } else {
                        ctx.emit(
                            dt,
                            &format!(
                                "{} hits {} for {} points of damage.",
                                mob_cap, sim.players[tgt].name, dmg
                            ),
                        );
                    }

                    // Being-hit alert: non-tank player calls out an extra mob on them
                    if mi < hit_alerted.len() && !hit_alerted[mi] && tgt != tank && sim.roll(75) {
                        if let Some(msg) = chat.pick_template("BeingHit", &mut sim.rng) {
                            let msg = msg.replace("{mob}", &mob_name);
                            emit_group_chat(sim, ctx, tgt, &msg);
                        }
                        hit_alerted[mi] = true;
                    }

                    // Thorns
                    if sim.mobs[mi].has_thorns {
                        let td =
                            sim.rand_range(sim.mobs[mi].thorns_dmg, sim.mobs[mi].thorns_dmg + 20);
                        if tgt == 0 {
                            ctx.emit(dt, &format!("YOU are pierced by {}'s thorns for {} points of non-melee damage!", mob_name, td));
                        } else {
                            ctx.emit(dt, &format!("{} is pierced by {}'s thorns for {} points of non-melee damage.", sim.players[tgt].name, mob_name, td));
                        }
                    }

                    // Damage shield
                    if sim.players[tgt].has_ds {
                        let ds =
                            sim.rand_range(sim.players[tgt].ds_dmg, sim.players[tgt].ds_dmg + 10);
                        if tgt == 0 {
                            ctx.emit(dt, &format!("{} is burned by YOUR flames for {} points of non-melee damage.", mob_cap, ds));
                        } else {
                            ctx.emit(dt, &format!("{} is burned by {}'s flames for {} points of non-melee damage.", mob_cap, sim.players[tgt].name, ds));
                        }
                    }

                    // Context chat: player near death / tank dying
                    let tgt_pct =
                        (sim.players[tgt].hp.saturating_mul(100)) / sim.players[tgt].hp_max;
                    if tgt_pct < 15 {
                        sim.players[tgt].sim_state.on_near_death();
                        if sim.roll(60) {
                            let tgt_name = sim.players[tgt].name.clone();
                            let responder = pick_responder(&sim.players, Some(tgt), &mut sim.rng);
                            if let Some(ri) = responder {
                                let trigger = if tgt == tank && tgt_pct < 10 {
                                    ChatTrigger::TankDying { player: &tgt_name }
                                } else {
                                    ChatTrigger::PlayerNearDeath { player: &tgt_name }
                                };
                                let cur_sec = sim.sec;
                                let personality = sim.players[ri].personality.clone();
                                let topic = trigger.topic_hint();
                                if let Some(msg) = chat.respond(
                                    &trigger,
                                    &personality,
                                    &sim.players[ri].sim_state,
                                    group_size,
                                    Some(zone_name.as_str()),
                                    cur_sec,
                                    &mut sim.rng,
                                ) {
                                    sim.players[ri].sim_state.mark_spoke(cur_sec);
                                    if let Some(t) = topic {
                                        sim.players[ri].sim_state.mark_topic_spoken(t, cur_sec);
                                    }
                                    emit_group_chat(sim, ctx, ri, &msg);
                                }
                            }
                        }
                    }
                }
            }

            // Mob spell cast
            if sim.mobs[mi].hp > 0
                && sim.mobs[mi].next_spell_sec <= cur
                && !sim.mobs[mi].spells.is_empty()
            {
                let sidx = sim.rng.gen_range(0..sim.mobs[mi].spells.len());
                let spell_name = sim.mobs[mi].spells[sidx].clone();
                let mob_cap = sim.mobs[mi].full_name_cap();
                ctx.emit(dt, &format!("{} begins casting {}.", mob_cap, spell_name));
                sim.mobs[mi].next_spell_sec = cur + sim.rand_range(12, 25);
            }
        }

        // ── Healer mana announcements ─────────────────────────────────────────
        for pi in 0..sim.players.len() {
            if sim.players[pi].hp <= 0 || sim.players[pi].mana_max == 0 {
                continue;
            }
            if sim.players[pi].classes.role() != Role::Healer {
                continue;
            }
            let mana_pct = (sim.players[pi].mana * 100 / sim.players[pi].mana_max) as u32;
            let threshold = if mana_pct <= 10 && (sim.players[pi].mana_alerted & 4) == 0 {
                sim.players[pi].mana_alerted |= 7; // mark all thresholds done
                Some(mana_pct)
            } else if mana_pct <= 25 && (sim.players[pi].mana_alerted & 2) == 0 {
                sim.players[pi].mana_alerted |= 3;
                Some(mana_pct)
            } else if mana_pct <= 50 && (sim.players[pi].mana_alerted & 1) == 0 {
                sim.players[pi].mana_alerted |= 1;
                Some(mana_pct)
            } else {
                None
            };
            if let Some(pct) = threshold {
                let trigger = ChatTrigger::HealerMana { pct };
                let cur_sec = sim.sec;
                let personality = sim.players[pi].personality.clone();
                let topic = trigger.topic_hint();
                if let Some(msg) = chat.respond(
                    &trigger,
                    &personality,
                    &sim.players[pi].sim_state,
                    group_size,
                    Some(zone_name.as_str()),
                    cur_sec,
                    &mut sim.rng,
                ) {
                    sim.players[pi].sim_state.mark_spoke(cur_sec);
                    if let Some(t) = topic {
                        sim.players[pi].sim_state.mark_topic_spoken(t, cur_sec);
                    }
                    emit_group_chat(sim, ctx, pi, &msg);
                }
            }
        }

        // ── Player actions ────────────────────────────────────────────────────
        for pi in 0..sim.players.len() {
            if sim.players[pi].hp <= 0 {
                continue;
            }
            if sim.mobs.is_empty() {
                break;
            }

            let cur_mob_alive: Vec<usize> = (0..sim.mobs.len())
                .filter(|&i| sim.mobs[i].hp > 0)
                .collect();
            if cur_mob_alive.is_empty() {
                break;
            }
            let target_mob = *cur_mob_alive.first().unwrap();
            // Unmezzed secondary mob for ENC mez targeting
            let extra_mob = cur_mob_alive
                .iter()
                .skip(1)
                .find(|&&i| !sim.mobs[i].mezzed)
                .copied();

            let primary = sim.players[pi].classes.primary;
            let already_casting = sim.casts.iter().any(|c| c.caster == pi);
            let can_cast = sim.players[pi].next_spell_sec <= cur && !already_casting;

            // ── Warrior: instant taunt (no mana, never skips melee) ────────
            if primary == Class::Warrior && sim.players[pi].next_spell_sec <= cur {
                let mob_name = sim.mobs[target_mob].full_name();
                let mob_cap = sim.mobs[target_mob].full_name_cap();
                if sim.roll(70) {
                    if pi == 0 {
                        ctx.emit(
                            dt,
                            &format!("You have stolen the attention of {}!", mob_name),
                        );
                    } else {
                        ctx.emit(
                            dt,
                            &format!(
                                "{} has stolen the attention of {}!",
                                sim.players[pi].name, mob_cap
                            ),
                        );
                    }
                } else if pi == 0 {
                    ctx.emit(
                        dt,
                        &format!("You have failed to steal the attention of {}.", mob_name),
                    );
                } else {
                    ctx.emit(
                        dt,
                        &format!(
                            "{} has failed to steal the attention of {}.",
                            sim.players[pi].name, mob_cap
                        ),
                    );
                }
                sim.players[pi].next_spell_sec = cur + 8;
                // fall through to melee
            }

            // ── Cleric: heal always; nuke only when no heal needed ─────────
            if primary == Class::Cleric && can_cast {
                let lp = lowest_hp_player(&sim.players);
                let lp_pct = (sim.players[lp].hp.saturating_mul(100)) / sim.players[lp].hp_max;
                if lp_pct < 80 {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_heal_spell(&pc, cfg) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            let csec = cur + spell.cast_secs;
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            let hp_at = sim.players[lp].hp;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: csec,
                                target_mob: None,
                                target_player: Some(lp),
                                target_hp_at_cast: Some(hp_at),
                            });
                            sim.players[pi].next_spell_sec = cur + 6;
                            continue;
                        }
                    }
                } else if sim.players[pi].mana > 0 {
                    // Nuke secondary when everyone is healthy
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Nuke) {
                        if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
            }

            // ── Druid: heal if needed → snare → DoT/nuke ──────────────────
            if primary == Class::Druid && can_cast && sim.players[pi].mana > 0 {
                let lp = lowest_hp_player(&sim.players);
                let lp_pct = (sim.players[lp].hp.saturating_mul(100)) / sim.players[lp].hp_max;
                if lp_pct < 60 {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_heal_spell(&pc, cfg) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            let csec = cur + spell.cast_secs;
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            let hp_at = sim.players[lp].hp;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: csec,
                                target_mob: None,
                                target_player: Some(lp),
                                target_hp_at_cast: Some(hp_at),
                            });
                            sim.players[pi].next_spell_sec = cur + 6;
                            continue;
                        }
                    }
                }
                if !sim.mobs[target_mob].snared {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Snare)
                        .or_else(|| pick_best(&pc, cfg, SpellKind::Root))
                    {
                        if sim.players[pi].mana >= spell.mana_cost {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
                let pc = sim.players[pi].clone();
                let spell = if sim.players[pi].dots_active == 0 {
                    pick_best(&pc, cfg, SpellKind::Dot)
                        .or_else(|| pick_best(&pc, cfg, SpellKind::Nuke))
                } else {
                    pick_best(&pc, cfg, SpellKind::Nuke)
                        .or_else(|| pick_best(&pc, cfg, SpellKind::Dot))
                };
                if let Some(spell) = spell {
                    if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                        if pi == 0 {
                            ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                        } else {
                            ctx.emit(
                                dt,
                                &format!("{} begins casting {}.", sim.players[pi].name, spell.name),
                            );
                        }
                        sim.players[pi].mana -= spell.mana_cost;
                        let will_dot = spell.kind == SpellKind::Dot;
                        sim.casts.push(PendingCast {
                            caster: pi,
                            spell: spell.clone(),
                            complete_sec: cur + spell.cast_secs,
                            target_mob: Some(target_mob),
                            target_player: None,
                            target_hp_at_cast: None,
                        });
                        sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                        if will_dot {
                            sim.players[pi].dots_active += 1;
                        }
                        continue;
                    }
                }
            }

            // ── Shaman: malo → slow → heal if needed → DoT ────────────────
            if primary == Class::Shaman && can_cast && sim.players[pi].mana > 0 {
                if !sim.players[pi].debuff_applied {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Debuff) {
                        if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            sim.players[pi].debuff_applied = true;
                            continue;
                        }
                    }
                }
                if !sim.mobs[target_mob].slowed {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Slow) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
                let lp = lowest_hp_player(&sim.players);
                let lp_pct = (sim.players[lp].hp.saturating_mul(100)) / sim.players[lp].hp_max;
                if lp_pct < 65 {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_heal_spell(&pc, cfg) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            let csec = cur + spell.cast_secs;
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            let hp_at = sim.players[lp].hp;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: csec,
                                target_mob: None,
                                target_player: Some(lp),
                                target_hp_at_cast: Some(hp_at),
                            });
                            sim.players[pi].next_spell_sec = cur + 6;
                            continue;
                        }
                    }
                }
                if sim.players[pi].dots_active == 0 && sim.mobs[target_mob].hp > 0 {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Dot) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            sim.players[pi].dots_active += 1;
                            continue;
                        }
                    }
                }
            }

            // ── Enchanter: tash → slow → mez adds → nuke ──────────────────
            if primary == Class::Enchanter && can_cast && sim.players[pi].mana > 0 {
                if !sim.players[pi].debuff_applied {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Debuff) {
                        if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            sim.players[pi].debuff_applied = true;
                            continue;
                        }
                    }
                }
                if !sim.mobs[target_mob].slowed {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Slow) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
                if let Some(em) = extra_mob {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Mez) {
                        if sim.players[pi].mana >= spell.mana_cost && sim.mobs[em].hp > 0 {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(em),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
                let pc = sim.players[pi].clone();
                if let Some(spell) = pick_best(&pc, cfg, SpellKind::Nuke) {
                    if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                        if pi == 0 {
                            ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                        } else {
                            ctx.emit(
                                dt,
                                &format!("{} begins casting {}.", sim.players[pi].name, spell.name),
                            );
                        }
                        sim.players[pi].mana -= spell.mana_cost;
                        sim.casts.push(PendingCast {
                            caster: pi,
                            spell: spell.clone(),
                            complete_sec: cur + spell.cast_secs,
                            target_mob: Some(target_mob),
                            target_player: None,
                            target_hp_at_cast: None,
                        });
                        sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                        continue;
                    }
                }
            }

            // ── Wizard / Magician: pure nuke ───────────────────────────────
            if matches!(primary, Class::Wizard | Class::Magician)
                && can_cast
                && sim.players[pi].mana > 0
            {
                let pc = sim.players[pi].clone();
                if let Some(spell) = pick_best(&pc, cfg, SpellKind::Nuke) {
                    if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                        if pi == 0 {
                            ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                        } else {
                            ctx.emit(
                                dt,
                                &format!("{} begins casting {}.", sim.players[pi].name, spell.name),
                            );
                        }
                        sim.players[pi].mana -= spell.mana_cost;
                        sim.casts.push(PendingCast {
                            caster: pi,
                            spell: spell.clone(),
                            complete_sec: cur + spell.cast_secs,
                            target_mob: Some(target_mob),
                            target_player: None,
                            target_hp_at_cast: None,
                        });
                        sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                        // OOM chat for pure DPS nukers
                        if sim.players[pi].mana < sim.players[pi].mana_max / 6 && sim.roll(50) {
                            let pname = sim.players[pi].name.clone();
                            if let Some(ri) = pick_responder(&sim.players, Some(pi), &mut sim.rng) {
                                let trigger = ChatTrigger::CasterOom { player: &pname };
                                let cur_sec = sim.sec;
                                let personality = sim.players[ri].personality.clone();
                                let topic = trigger.topic_hint();
                                if let Some(msg) = chat.respond(
                                    &trigger,
                                    &personality,
                                    &sim.players[ri].sim_state,
                                    group_size,
                                    Some(zone_name.as_str()),
                                    cur_sec,
                                    &mut sim.rng,
                                ) {
                                    sim.players[ri].sim_state.mark_spoke(cur_sec);
                                    if let Some(t) = topic {
                                        sim.players[ri].sim_state.mark_topic_spoken(t, cur_sec);
                                    }
                                    emit_group_chat(sim, ctx, ri, &msg);
                                }
                            }
                        }
                        continue;
                    }
                }
            }

            // ── Necromancer: DoT → lifetap → snare ────────────────────────
            if primary == Class::Necromancer && can_cast && sim.players[pi].mana > 0 {
                if sim.players[pi].dots_active < 2 && sim.mobs[target_mob].hp > 0 {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Dot) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            sim.players[pi].dots_active += 1;
                            continue;
                        }
                    }
                }
                if sim.mobs[target_mob].hp > 0 {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Lifetap) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
                if !sim.mobs[target_mob].snared {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Snare) {
                        if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
            }

            // ── Shadowknight: snare → lifetap → DoT ───────────────────────
            if primary == Class::Shadowknight && can_cast && sim.players[pi].mana > 0 {
                if !sim.mobs[target_mob].snared {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Snare) {
                        if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
                if sim.mobs[target_mob].hp > 0 {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Lifetap) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
                if sim.players[pi].dots_active == 0 && sim.mobs[target_mob].hp > 0 {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Dot) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            sim.players[pi].dots_active += 1;
                            continue;
                        }
                    }
                }
            }

            // ── Ranger: snare → DoT/nuke ───────────────────────────────────
            if primary == Class::Ranger && can_cast && sim.players[pi].mana > 0 {
                if !sim.mobs[target_mob].snared {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Snare)
                        .or_else(|| pick_best(&pc, cfg, SpellKind::Root))
                    {
                        if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            continue;
                        }
                    }
                }
                let pc = sim.players[pi].clone();
                let spell = if sim.players[pi].dots_active == 0 {
                    pick_best(&pc, cfg, SpellKind::Dot)
                        .or_else(|| pick_best(&pc, cfg, SpellKind::Nuke))
                } else {
                    pick_best(&pc, cfg, SpellKind::Nuke)
                };
                if let Some(spell) = spell {
                    if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                        if pi == 0 {
                            ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                        } else {
                            ctx.emit(
                                dt,
                                &format!("{} begins casting {}.", sim.players[pi].name, spell.name),
                            );
                        }
                        sim.players[pi].mana -= spell.mana_cost;
                        let will_dot = spell.kind == SpellKind::Dot;
                        sim.casts.push(PendingCast {
                            caster: pi,
                            spell: spell.clone(),
                            complete_sec: cur + spell.cast_secs,
                            target_mob: Some(target_mob),
                            target_player: None,
                            target_hp_at_cast: None,
                        });
                        sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                        if will_dot {
                            sim.players[pi].dots_active += 1;
                        }
                        continue;
                    }
                }
            }

            // ── Paladin: heal → stun/debuff → nuke ────────────────────────
            if primary == Class::Paladin && can_cast && sim.players[pi].mana > 0 {
                let lp = lowest_hp_player(&sim.players);
                let lp_pct = (sim.players[lp].hp.saturating_mul(100)) / sim.players[lp].hp_max;
                if lp_pct < 70 {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_heal_spell(&pc, cfg) {
                        if sim.players[pi].mana >= spell.mana_cost {
                            let csec = cur + spell.cast_secs;
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            let hp_at = sim.players[lp].hp;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: csec,
                                target_mob: None,
                                target_player: Some(lp),
                                target_hp_at_cast: Some(hp_at),
                            });
                            sim.players[pi].next_spell_sec = cur + 6;
                            continue;
                        }
                    }
                }
                if !sim.players[pi].debuff_applied {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Debuff) {
                        if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                            if pi == 0 {
                                ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} begins casting {}.",
                                        sim.players[pi].name, spell.name
                                    ),
                                );
                            }
                            sim.players[pi].mana -= spell.mana_cost;
                            sim.casts.push(PendingCast {
                                caster: pi,
                                spell: spell.clone(),
                                complete_sec: cur + spell.cast_secs,
                                target_mob: Some(target_mob),
                                target_player: None,
                                target_hp_at_cast: None,
                            });
                            sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                            sim.players[pi].debuff_applied = true;
                            continue;
                        }
                    }
                }
                let pc = sim.players[pi].clone();
                if let Some(spell) = pick_best(&pc, cfg, SpellKind::Nuke) {
                    if sim.players[pi].mana >= spell.mana_cost && sim.mobs[target_mob].hp > 0 {
                        if pi == 0 {
                            ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                        } else {
                            ctx.emit(
                                dt,
                                &format!("{} begins casting {}.", sim.players[pi].name, spell.name),
                            );
                        }
                        sim.players[pi].mana -= spell.mana_cost;
                        sim.casts.push(PendingCast {
                            caster: pi,
                            spell: spell.clone(),
                            complete_sec: cur + spell.cast_secs,
                            target_mob: Some(target_mob),
                            target_player: None,
                            target_hp_at_cast: None,
                        });
                        sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                        continue;
                    }
                }
            }

            // ── Bard: slow if not slowed, then nuke (mana_cost=0 songs) ───
            if primary == Class::Bard && sim.players[pi].next_spell_sec <= cur && !already_casting {
                if !sim.mobs[target_mob].slowed {
                    let pc = sim.players[pi].clone();
                    if let Some(spell) = pick_best(&pc, cfg, SpellKind::Slow) {
                        if pi == 0 {
                            ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                        } else {
                            ctx.emit(
                                dt,
                                &format!("{} begins casting {}.", sim.players[pi].name, spell.name),
                            );
                        }
                        sim.casts.push(PendingCast {
                            caster: pi,
                            spell: spell.clone(),
                            complete_sec: cur + spell.cast_secs,
                            target_mob: Some(target_mob),
                            target_player: None,
                            target_hp_at_cast: None,
                        });
                        sim.players[pi].next_spell_sec = cur + spell.cast_secs + 2;
                        continue;
                    }
                }
                let pc = sim.players[pi].clone();
                if let Some(spell) = pick_best(&pc, cfg, SpellKind::Nuke) {
                    if sim.mobs[target_mob].hp > 0 {
                        if pi == 0 {
                            ctx.emit(dt, &format!("You begin casting {}.", spell.name));
                        } else {
                            ctx.emit(
                                dt,
                                &format!("{} begins casting {}.", sim.players[pi].name, spell.name),
                            );
                        }
                        sim.casts.push(PendingCast {
                            caster: pi,
                            spell: spell.clone(),
                            complete_sec: cur + spell.cast_secs,
                            target_mob: Some(target_mob),
                            target_player: None,
                            target_hp_at_cast: None,
                        });
                        sim.players[pi].next_spell_sec = cur + spell.cast_secs + 4;
                        continue;
                    }
                }
            }

            // Melee attack
            if sim.players[pi].next_attack_sec <= cur && !sim.mobs.is_empty() {
                let delay = sim.players[pi].classes.attack_delay();
                sim.players[pi].next_attack_sec = cur + delay + sim.rng.gen_range(0u32..2);

                let (dmg_lo, dmg_hi) = sim.players[pi].classes.dmg_range();
                let attacks = sim.players[pi].classes.attacks_per_round();
                let verbs = sim.players[pi].classes.melee_attacks();
                let mob_name = sim.mobs[target_mob].full_name();
                let mob_name_cap = sim.mobs[target_mob].full_name_cap();

                for _ in 0..attacks {
                    if sim.mobs[target_mob].hp <= 0 {
                        break;
                    }
                    let vidx = sim.rng.gen_range(0..verbs.len());
                    let (verb_you, verb_3p) = verbs[vidx];
                    let is_backstab = verb_you == "backstab";
                    let miss_roll = sim.rng.gen_range(0u32..100);

                    if miss_roll < 25 {
                        let mob_response = if sim.roll(30) {
                            format!("{} parries!", mob_name)
                        } else if sim.roll(30) {
                            format!("{} dodges!", mob_name)
                        } else {
                            String::new()
                        };
                        if pi == 0 {
                            let suffix = if mob_response.is_empty() {
                                "miss!".to_string()
                            } else {
                                mob_response
                            };
                            ctx.emit(
                                dt,
                                &format!("You try to {} {}, but {}", verb_you, mob_name, suffix),
                            );
                        } else {
                            let suffix = if mob_response.is_empty() {
                                "misses!".to_string()
                            } else {
                                mob_response
                            };
                            ctx.emit(
                                dt,
                                &format!(
                                    "{} tries to {} {}, but {}",
                                    sim.players[pi].name, verb_you, mob_name, suffix
                                ),
                            );
                        }
                        sim.players[pi].consecutive_misses += 1;

                        // Tease after 4+ consecutive misses
                        if sim.players[pi].consecutive_misses >= 4 && sim.roll(40) {
                            let pname = sim.players[pi].name.clone();
                            let mob_n = mob_name.clone();
                            if let Some(ri) = pick_responder(&sim.players, Some(pi), &mut sim.rng) {
                                let trigger = ChatTrigger::RepeatMiss {
                                    player: &pname,
                                    mob: &mob_n,
                                };
                                let cur_sec = sim.sec;
                                let personality = sim.players[ri].personality.clone();
                                let topic = trigger.topic_hint();
                                if let Some(msg) = chat.respond(
                                    &trigger,
                                    &personality,
                                    &sim.players[ri].sim_state,
                                    group_size,
                                    Some(zone_name.as_str()),
                                    cur_sec,
                                    &mut sim.rng,
                                ) {
                                    sim.players[ri].sim_state.mark_spoke(cur_sec);
                                    if let Some(t) = topic {
                                        sim.players[ri].sim_state.mark_topic_spoken(t, cur_sec);
                                    }
                                    emit_group_chat(sim, ctx, ri, &msg);
                                }
                                sim.players[pi].consecutive_misses = 0;
                            }
                        }
                    } else {
                        sim.players[pi].consecutive_misses = 0;
                        let dmg = if is_backstab {
                            if let Some((blo, bhi)) = sim.players[pi].classes.backstab_range() {
                                sim.rand_range(blo, bhi)
                            } else {
                                sim.rand_range(dmg_lo, dmg_hi)
                            }
                        } else {
                            sim.rand_range(dmg_lo, dmg_hi)
                        };
                        sim.mobs[target_mob].hp -= dmg as i32;
                        sim.mobs[target_mob].mezzed = false;
                        if pi == 0 {
                            ctx.emit(
                                dt,
                                &format!(
                                    "You {} {} for {} points of damage.",
                                    verb_you, mob_name, dmg
                                ),
                            );
                        } else {
                            ctx.emit(
                                dt,
                                &format!(
                                    "{} {} {} for {} points of damage.",
                                    sim.players[pi].name, verb_3p, mob_name, dmg
                                ),
                            );
                        }

                        // Riposte
                        if sim.roll(5) && !is_backstab {
                            let rip = sim.rand_range(15, 80);
                            ctx.emit(
                                dt,
                                &format!(
                                    "{} was injured by {}'s riposte for {} points of damage.",
                                    mob_name_cap, sim.players[pi].name, rip
                                ),
                            );
                        }
                    }
                }
            }
        }

        // ── Resolve completed casts ───────────────────────────────────────────
        let completed: Vec<usize> = sim
            .casts
            .iter()
            .enumerate()
            .filter(|(_, c)| c.complete_sec <= cur)
            .map(|(i, _)| i)
            .collect();

        for ci in completed.into_iter().rev() {
            let cast = sim.casts.remove(ci);
            let pi = cast.caster;
            let dt = sim.dt();

            match cast.spell.kind {
                SpellKind::DirectHeal | SpellKind::HoT | SpellKind::PromisedHeal => {
                    if let Some(tgt) = cast.target_player {
                        let hp_before = cast.target_hp_at_cast.unwrap_or(sim.players[tgt].hp);
                        let hp_before_pct = (hp_before * 100) / sim.players[tgt].hp_max;
                        let amt = sim.rand_range(cast.spell.min_val, cast.spell.max_val);
                        let tgt_hp_max = sim.players[tgt].hp_max;
                        let overheal = (sim.players[tgt].hp + amt as i32)
                            .saturating_sub(tgt_hp_max)
                            .max(0) as u32;
                        sim.players[tgt].hp = (sim.players[tgt].hp + amt as i32).min(tgt_hp_max);

                        let healer_name = sim.players[pi].name.clone();
                        let tgt_name = sim.players[tgt].name.clone();

                        if cast.spell.kind == SpellKind::PromisedHeal {
                            if !cast.spell.land_msg.is_empty() {
                                ctx.emit(dt, &cast.spell.land_msg);
                            }
                            let heal_str = if overheal > 0 {
                                format!(
                                    "{} ({}) hit points by {}.",
                                    amt, cast.spell.ticks, cast.spell.name
                                )
                            } else {
                                format!("{} hit points by {}.", amt, cast.spell.name)
                            };
                            if pi == 0 {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "You healed {} for {}",
                                        ptgt_heal(&sim.players, tgt),
                                        heal_str
                                    ),
                                );
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} healed {} for {}",
                                        healer_name,
                                        ptgt_heal(&sim.players, tgt),
                                        heal_str
                                    ),
                                );
                            }
                        } else if cast.spell.kind == SpellKind::HoT {
                            if pi == 0 {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "You healed {} for {} hit points by {}.",
                                        ptgt_heal(&sim.players, tgt),
                                        amt,
                                        cast.spell.name
                                    ),
                                );
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} healed {} for {} hit points by {}.",
                                        healer_name,
                                        ptgt_heal(&sim.players, tgt),
                                        amt,
                                        cast.spell.name
                                    ),
                                );
                            }
                            sim.hots.push(ActiveHot {
                                caster: pi,
                                spell: cast.spell.name.clone(),
                                heal_lo: cast.spell.min_val,
                                heal_hi: cast.spell.max_val,
                                remaining: cast.spell.ticks,
                                next_tick: cur + cast.spell.tick_secs,
                                tick_secs: cast.spell.tick_secs,
                                target: tgt,
                            });
                        } else {
                            let heal_str = if overheal > 0 {
                                format!("{} ({}) hit points by {}.", amt, overheal, cast.spell.name)
                            } else {
                                format!("{} hit points by {}.", amt, cast.spell.name)
                            };
                            if pi == 0 {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "You healed {} for {}",
                                        ptgt_heal(&sim.players, tgt),
                                        heal_str
                                    ),
                                );
                            } else if tgt == pi {
                                let reflex = if tgt == 0 {
                                    "yourself".to_string()
                                } else {
                                    "himself".to_string()
                                };
                                ctx.emit(
                                    dt,
                                    &format!("{} healed {} for {}", healer_name, reflex, heal_str),
                                );
                            } else {
                                ctx.emit(
                                    dt,
                                    &format!(
                                        "{} healed {} for {}",
                                        healer_name,
                                        ptgt_heal(&sim.players, tgt),
                                        heal_str
                                    ),
                                );
                            }
                        }

                        // Near-death save chat
                        if hp_before_pct < 15 && amt >= 500 && sim.roll(70) {
                            let hn = healer_name.clone();
                            let tn = tgt_name.clone();
                            if let Some(ri) = pick_responder(&sim.players, Some(pi), &mut sim.rng) {
                                let trigger = ChatTrigger::NearDeathSave {
                                    healer: &hn,
                                    target: &tn,
                                };
                                let cur_sec = sim.sec;
                                let personality = sim.players[ri].personality.clone();
                                let topic = trigger.topic_hint();
                                if let Some(msg) = chat.respond(
                                    &trigger,
                                    &personality,
                                    &sim.players[ri].sim_state,
                                    group_size,
                                    Some(zone_name.as_str()),
                                    cur_sec,
                                    &mut sim.rng,
                                ) {
                                    sim.players[ri].sim_state.mark_spoke(cur_sec);
                                    if let Some(t) = topic {
                                        sim.players[ri].sim_state.mark_topic_spoken(t, cur_sec);
                                    }
                                    emit_group_chat(sim, ctx, ri, &msg);
                                }
                            }
                        }
                    }
                }

                SpellKind::Nuke => {
                    if let Some(mi) = cast.target_mob {
                        if mi < sim.mobs.len() && sim.mobs[mi].hp > 0 {
                            let resist = sim.rng.gen_range(0u32..100) < cast.spell.resist_pct;
                            let mob_name = sim.mobs[mi].full_name();
                            let mob_cap = sim.mobs[mi].full_name_cap();
                            if resist {
                                if pi == 0 {
                                    ctx.emit(
                                        dt,
                                        &format!("{} resisted your {}!", mob_cap, cast.spell.name),
                                    );
                                } else {
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "{} resisted {}'s {}!",
                                            mob_cap, sim.players[pi].name, cast.spell.name
                                        ),
                                    );
                                }
                                if pi < sim.players.len() {
                                    sim.players[pi].sim_state.on_resist();
                                }
                            } else {
                                let dmg = sim.rand_range(cast.spell.min_val, cast.spell.max_val);
                                sim.mobs[mi].hp -= dmg as i32;
                                sim.mobs[mi].mezzed = false;
                                if pi == 0 {
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "You hit {} for {} points of fire damage by {}.",
                                            mob_name, dmg, cast.spell.name
                                        ),
                                    );
                                } else {
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "{} hit {} for {} points of fire damage by {}.",
                                            sim.players[pi].name, mob_name, dmg, cast.spell.name
                                        ),
                                    );
                                }

                                // Big hit chat
                                if dmg >= 1400 && sim.roll(70) {
                                    let cname = sim.players[pi].name.clone();
                                    let sname = cast.spell.name.clone();
                                    let mname = mob_name.clone();
                                    if let Some(ri) =
                                        pick_responder(&sim.players, Some(pi), &mut sim.rng)
                                    {
                                        let trigger = ChatTrigger::BigSpellHit {
                                            caster: &cname,
                                            spell: &sname,
                                            mob: &mname,
                                            damage: dmg,
                                        };
                                        let cur_sec = sim.sec;
                                        let personality = sim.players[ri].personality.clone();
                                        let topic = trigger.topic_hint();
                                        if let Some(msg) = chat.respond(
                                            &trigger,
                                            &personality,
                                            &sim.players[ri].sim_state,
                                            group_size,
                                            Some(zone_name.as_str()),
                                            cur_sec,
                                            &mut sim.rng,
                                        ) {
                                            sim.players[ri].sim_state.mark_spoke(cur_sec);
                                            if let Some(t) = topic {
                                                sim.players[ri]
                                                    .sim_state
                                                    .mark_topic_spoken(t, cur_sec);
                                            }
                                            emit_group_chat(sim, ctx, ri, &msg);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                SpellKind::Dot => {
                    if let Some(mi) = cast.target_mob {
                        if mi < sim.mobs.len() && sim.mobs[mi].hp > 0 {
                            let resist = sim.rng.gen_range(0u32..100) < cast.spell.resist_pct;
                            let mob_cap = sim.mobs[mi].full_name_cap();
                            if resist {
                                if pi == 0 {
                                    ctx.emit(
                                        dt,
                                        &format!("{} resisted your {}!", mob_cap, cast.spell.name),
                                    );
                                } else {
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "{} resisted {}'s {}!",
                                            mob_cap, sim.players[pi].name, cast.spell.name
                                        ),
                                    );
                                }
                                if pi < sim.players.len() {
                                    sim.players[pi].dots_active =
                                        sim.players[pi].dots_active.saturating_sub(1);
                                    sim.players[pi].sim_state.on_resist();
                                }
                            } else {
                                if !cast.spell.land_msg.is_empty() {
                                    ctx.emit(dt, &format!("{} {}.", mob_cap, cast.spell.land_msg));
                                }
                                sim.dots.push(ActiveDot {
                                    caster: pi,
                                    spell: cast.spell.name.clone(),
                                    dmg_lo: cast.spell.min_val,
                                    dmg_hi: cast.spell.max_val,
                                    remaining: cast.spell.ticks,
                                    next_tick: cur + cast.spell.tick_secs,
                                    tick_secs: cast.spell.tick_secs,
                                    mob_idx: mi,
                                });
                            }
                        }
                    }
                }

                SpellKind::Slow | SpellKind::Debuff => {
                    if let Some(mi) = cast.target_mob {
                        if mi < sim.mobs.len() && sim.mobs[mi].hp > 0 {
                            let resist = sim.rng.gen_range(0u32..100) < cast.spell.resist_pct;
                            let mob_cap = sim.mobs[mi].full_name_cap();
                            if resist {
                                if pi == 0 {
                                    ctx.emit(
                                        dt,
                                        &format!("{} resisted your {}!", mob_cap, cast.spell.name),
                                    );
                                } else {
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "{} resisted {}'s {}!",
                                            mob_cap, sim.players[pi].name, cast.spell.name
                                        ),
                                    );
                                }
                                sim.players[pi].debuff_applied = false;
                                if pi < sim.players.len() {
                                    sim.players[pi].sim_state.on_resist();
                                }

                                // Slow-resist chat
                                if cast.spell.kind == SpellKind::Slow && sim.roll(45) {
                                    if let Some(ri) =
                                        pick_responder(&sim.players, Some(pi), &mut sim.rng)
                                    {
                                        let trigger = ChatTrigger::SlowResisted;
                                        let cur_sec = sim.sec;
                                        let personality = sim.players[ri].personality.clone();
                                        let topic = trigger.topic_hint();
                                        if let Some(msg) = chat.respond(
                                            &trigger,
                                            &personality,
                                            &sim.players[ri].sim_state,
                                            group_size,
                                            Some(zone_name.as_str()),
                                            cur_sec,
                                            &mut sim.rng,
                                        ) {
                                            sim.players[ri].sim_state.mark_spoke(cur_sec);
                                            if let Some(t) = topic {
                                                sim.players[ri]
                                                    .sim_state
                                                    .mark_topic_spoken(t, cur_sec);
                                            }
                                            emit_group_chat(sim, ctx, ri, &msg);
                                        }
                                    }
                                }
                            } else {
                                sim.mobs[mi].slowed = true;
                                ctx.emit(dt, &format!("{} has been slowed.", mob_cap));
                            }
                        }
                    }
                }

                SpellKind::Snare | SpellKind::Root => {
                    if let Some(mi) = cast.target_mob {
                        if mi < sim.mobs.len() && sim.mobs[mi].hp > 0 {
                            let resist = sim.rng.gen_range(0u32..100) < cast.spell.resist_pct;
                            let mob_cap = sim.mobs[mi].full_name_cap();
                            if resist {
                                if pi == 0 {
                                    ctx.emit(
                                        dt,
                                        &format!("{} resisted your {}!", mob_cap, cast.spell.name),
                                    );
                                } else {
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "{} resisted {}'s {}!",
                                            mob_cap, sim.players[pi].name, cast.spell.name
                                        ),
                                    );
                                }
                                sim.players[pi].sim_state.on_resist();
                            } else {
                                sim.mobs[mi].snared = true;
                                let land = if cast.spell.land_msg.is_empty() {
                                    format!("{} has been snared.", mob_cap)
                                } else {
                                    cast.spell.land_msg.replace("{mob}", &mob_cap)
                                };
                                ctx.emit(dt, &land);
                            }
                        }
                    }
                }

                SpellKind::Mez => {
                    if let Some(mi) = cast.target_mob {
                        if mi < sim.mobs.len() && sim.mobs[mi].hp > 0 {
                            let resist = sim.rng.gen_range(0u32..100) < cast.spell.resist_pct;
                            let mob_cap = sim.mobs[mi].full_name_cap();
                            if resist {
                                if pi == 0 {
                                    ctx.emit(
                                        dt,
                                        &format!("{} resisted your {}!", mob_cap, cast.spell.name),
                                    );
                                } else {
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "{} resisted {}'s {}!",
                                            mob_cap, sim.players[pi].name, cast.spell.name
                                        ),
                                    );
                                }
                                sim.players[pi].sim_state.on_resist();
                            } else {
                                sim.mobs[mi].mezzed = true;
                                let land = if cast.spell.land_msg.is_empty() {
                                    format!("{} has been mesmerized.", mob_cap)
                                } else {
                                    cast.spell.land_msg.replace("{mob}", &mob_cap)
                                };
                                ctx.emit(dt, &land);
                            }
                        }
                    }
                }

                SpellKind::Lifetap => {
                    if let Some(mi) = cast.target_mob {
                        if mi < sim.mobs.len() && sim.mobs[mi].hp > 0 {
                            let resist = sim.rng.gen_range(0u32..100) < cast.spell.resist_pct;
                            let mob_cap = sim.mobs[mi].full_name_cap();
                            if resist {
                                if pi == 0 {
                                    ctx.emit(
                                        dt,
                                        &format!("{} resisted your {}!", mob_cap, cast.spell.name),
                                    );
                                } else {
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "{} resisted {}'s {}!",
                                            mob_cap, sim.players[pi].name, cast.spell.name
                                        ),
                                    );
                                }
                                sim.players[pi].sim_state.on_resist();
                            } else {
                                let lo = cast.spell.min_val;
                                let hi = cast.spell.max_val.max(lo);
                                let dmg = sim.rng.gen_range(lo..=hi);
                                sim.mobs[mi].hp -= dmg as i32;
                                sim.mobs[mi].mezzed = false;
                                let healed = dmg;
                                let caster_max = sim.players[pi].hp_max;
                                sim.players[pi].hp =
                                    (sim.players[pi].hp + healed as i32).min(caster_max);
                                if pi == 0 {
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "{} has been drained of {} hit points by your {}.",
                                            mob_cap, dmg, cast.spell.name
                                        ),
                                    );
                                    ctx.emit(
                                        dt,
                                        &format!("You have been healed for {} hit points.", healed),
                                    );
                                } else {
                                    let cname = sim.players[pi].name.clone();
                                    ctx.emit(
                                        dt,
                                        &format!(
                                            "{} has been drained of {} hit points by {}'s {}.",
                                            mob_cap, dmg, cname, cast.spell.name
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }

                SpellKind::Buff => {}
            }
        }

        // ── DoT ticks ────────────────────────────────────────────────────────
        let cur_dt = sim.base + Duration::seconds(sim.sec as i64);
        let mut expired_dots: Vec<usize> = Vec::new();
        for di in 0..sim.dots.len() {
            if sim.dots[di].next_tick > cur {
                continue;
            }
            let mi = sim.dots[di].mob_idx;
            if mi >= sim.mobs.len() || sim.mobs[mi].hp <= 0 {
                expired_dots.push(di);
                continue;
            }
            let lo = sim.dots[di].dmg_lo;
            let hi = sim.dots[di].dmg_hi;
            let dmg = sim.rng.gen_range(lo..=hi);
            sim.mobs[mi].hp -= dmg as i32;
            sim.mobs[mi].mezzed = false;
            let mob_cap = sim.mobs[mi].full_name_cap();
            let caster = sim.dots[di].caster;
            let spell = sim.dots[di].spell.clone();
            if caster == 0 {
                ctx.emit(
                    cur_dt,
                    &format!("{} has taken {} damage from your {}.", mob_cap, dmg, spell),
                );
            } else if caster < sim.players.len() {
                let cname = sim.players[caster].name.clone();
                ctx.emit(
                    cur_dt,
                    &format!(
                        "{} has been damaged by {}'s {} for {}",
                        mob_cap, cname, spell, dmg
                    ),
                );
            }
            let tick_s = sim.dots[di].tick_secs.max(6);
            sim.dots[di].next_tick = cur + tick_s;
            sim.dots[di].remaining = sim.dots[di].remaining.saturating_sub(1);
            if sim.dots[di].remaining == 0 {
                expired_dots.push(di);
            }
        }
        for di in expired_dots.into_iter().rev() {
            if di < sim.dots.len() {
                let d = sim.dots.remove(di);
                if d.caster < sim.players.len() {
                    sim.players[d.caster].dots_active =
                        sim.players[d.caster].dots_active.saturating_sub(1);
                }
            }
        }

        // ── HoT ticks ────────────────────────────────────────────────────────
        let mut expired_hots: Vec<usize> = Vec::new();
        for hi in 0..sim.hots.len() {
            if sim.hots[hi].next_tick > cur {
                continue;
            }
            let tgt = sim.hots[hi].target;
            let lo = sim.hots[hi].heal_lo;
            let hhi = sim.hots[hi].heal_hi;
            let amt = sim.rng.gen_range(lo..=hhi);
            let tgt_max = sim.players[tgt].hp_max;
            sim.players[tgt].hp = (sim.players[tgt].hp + amt as i32).min(tgt_max);
            let caster = sim.hots[hi].caster;
            let spell = sim.hots[hi].spell.clone();
            if caster == 0 {
                let tname = ptgt_heal(&sim.players, tgt);
                ctx.emit(
                    cur_dt,
                    &format!(
                        "You healed {} over time for {} hit points by {}.",
                        tname, amt, spell
                    ),
                );
            } else if caster < sim.players.len() {
                let cname = sim.players[caster].name.clone();
                if tgt == 0 {
                    ctx.emit(
                        cur_dt,
                        &format!(
                            "{} healed you over time for {} hit points by {}.",
                            cname, amt, spell
                        ),
                    );
                } else {
                    let tname = sim.players[tgt].name.clone();
                    ctx.emit(
                        cur_dt,
                        &format!(
                            "{} healed {} over time for {} hit points by {}.",
                            cname, tname, amt, spell
                        ),
                    );
                }
            }
            let tick_s = sim.hots[hi].tick_secs.max(4);
            sim.hots[hi].next_tick = cur + tick_s;
            sim.hots[hi].remaining = sim.hots[hi].remaining.saturating_sub(1);
            if sim.hots[hi].remaining == 0 {
                expired_hots.push(hi);
            }
        }
        for hi in expired_hots.into_iter().rev() {
            if hi < sim.hots.len() {
                sim.hots.remove(hi);
            }
        }

        // ── Mob deaths ───────────────────────────────────────────────────────
        let mut killed: Vec<usize> = sim
            .mobs
            .iter()
            .enumerate()
            .filter(|(_, m)| m.hp <= 0)
            .map(|(i, _)| i)
            .collect();

        for &mi in &killed {
            sim.dots.retain(|d| d.mob_idx != mi);
        }

        if !killed.is_empty() {
            let killer = if sim.roll(40) {
                0usize
            } else {
                sim.rng.gen_range(0..sim.players.len())
            };
            let dt = sim.dt();

            for &mi in &killed {
                let mob_cap = sim.mobs[mi].full_name_cap();
                let mob_lc = sim.mobs[mi].full_name();

                ctx.emit(dt, "You gain party experience!");
                let coin = sim.rand_range(2000, 18000);
                ctx.emit(
                    dt,
                    &format!("You receive {} from the corpse.", coin_from_corpse(coin)),
                );

                if killer == 0 {
                    ctx.emit(dt, &format!("You have slain {}!", mob_lc));
                } else {
                    ctx.emit(
                        dt,
                        &format!(
                            "{} has been slain by {}!",
                            mob_cap, sim.players[killer].name
                        ),
                    );
                }

                // All living players get the morale bump from a kill
                for p in sim.players.iter_mut() {
                    if p.hp > 0 {
                        p.sim_state.on_slay();
                    }
                }

                // Mob-killed chat
                if sim.roll(55) {
                    let mn = mob_lc.clone();
                    if let Some(ri) = pick_responder(&sim.players, None, &mut sim.rng) {
                        let trigger = ChatTrigger::MobKilled { mob: &mn };
                        let cur_sec = sim.sec;
                        let personality = sim.players[ri].personality.clone();
                        let topic = trigger.topic_hint();
                        if let Some(msg) = chat.respond(
                            &trigger,
                            &personality,
                            &sim.players[ri].sim_state,
                            group_size,
                            Some(zone_name.as_str()),
                            cur_sec,
                            &mut sim.rng,
                        ) {
                            sim.players[ri].sim_state.mark_spoke(cur_sec);
                            if let Some(t) = topic {
                                sim.players[ri].sim_state.mark_topic_spoken(t, cur_sec);
                            }
                            emit_group_chat(sim, ctx, ri, &msg);
                        }
                    }
                }

                // Loot
                let loot_count = sim.rng.gen_range(0u32..=3);
                let dropped: Vec<LootEntry> = {
                    let mut pool = sim.mobs[mi].loot_table.clone();
                    pool.shuffle(&mut sim.rng);
                    pool.into_iter().take(loot_count as usize).collect()
                };

                for entry in &dropped {
                    let keep = sim.rng.gen_range(0u32..100) < entry.keep_chance;
                    sim.sec += sim.rng.gen_range(3u32..14);
                    let dt_loot = sim.base + Duration::seconds(sim.sec as i64);

                    if keep || entry.keep_chance > 50 {
                        ctx.emit(
                            dt_loot,
                            &format!(
                                "--You have looted {} from {}'s corpse.--",
                                entry.item, mob_lc
                            ),
                        );

                        // Enhancement
                        if entry.enhanceable
                            && sim.players[0].inventory.iter().any(|i| {
                                i.starts_with(entry.item.trim_end_matches(|c: char| {
                                    c.is_ascii_digit() || c == '+' || c == ' '
                                }))
                            })
                        {
                            let base = entry
                                .item
                                .trim_end_matches(|c: char| c.is_ascii_digit())
                                .trim_end_matches('+')
                                .trim_end()
                                .to_string();
                            let current_level: u32 = entry
                                .item
                                .chars()
                                .rev()
                                .take_while(|c| c.is_ascii_digit())
                                .collect::<String>()
                                .chars()
                                .rev()
                                .collect::<String>()
                                .parse()
                                .unwrap_or(1);
                            let new_level = current_level + 1;
                            sim.sec += 5;
                            let dt1 = sim.base + Duration::seconds(sim.sec as i64);
                            ctx.emit(
                                dt1,
                                &format!(
                                    "You successfully destroyed 1 {} (Exaltation).",
                                    entry.item
                                ),
                            );
                            sim.sec += 5;
                            let dt2 = sim.base + Duration::seconds(sim.sec as i64);
                            ctx.emit(dt2, &format!("You have successfully merged two items together to create a new item: {} +{}", base, new_level));
                            if let Some(p) = sim.players[0]
                                .inventory
                                .iter()
                                .position(|i| i.starts_with(&base))
                            {
                                sim.players[0].inventory[p] = format!("{} +{}", base, new_level);
                            }
                        } else {
                            sim.players[0].inventory.push(entry.item.clone());
                        }
                        sim.players[0].sim_state.on_good_loot();
                    } else if entry.sell_value >= 6000 {
                        ctx.emit(
                            dt_loot,
                            &format!(
                                "--You have looted {} from {}'s corpse.--",
                                entry.item, mob_lc
                            ),
                        );
                        sim.loot_bag.push((entry.item.clone(), entry.sell_value));
                        sim.players[0].sim_state.on_good_loot();
                    } else {
                        let price_str = copper_to_str(entry.sell_value);
                        ctx.emit(
                            dt_loot,
                            &format!(
                                "You looted {} from {}'s corpse and sold it for {}.",
                                entry.item, mob_lc, price_str
                            ),
                        );
                        sim.players[0].sim_state.on_bad_loot();
                    }
                }
            }

            killed.sort_unstable();
            for &mi in killed.iter().rev() {
                sim.mobs.remove(mi);
                for d in sim.dots.iter_mut() {
                    if d.mob_idx > mi {
                        d.mob_idx -= 1;
                    }
                }
                for cast in sim.casts.iter_mut() {
                    if let Some(ref mut tm) = cast.target_mob {
                        if *tm > mi {
                            *tm -= 1;
                        }
                    }
                }
            }

            for p in sim.players.iter_mut() {
                p.dots_active = 0;
                p.debuff_applied = false;
            }

            // MultiKill celebration when the full pack goes down
            if sim.mobs.is_empty() && num_mobs > 1 && sim.roll(65) {
                if let Some(ri) = pick_responder(&sim.players, None, &mut sim.rng) {
                    let trigger = ChatTrigger::MultiKill;
                    let cur_sec = sim.sec;
                    let personality = sim.players[ri].personality.clone();
                    let topic = trigger.topic_hint();
                    if let Some(msg) = chat.respond(
                        &trigger,
                        &personality,
                        &sim.players[ri].sim_state,
                        group_size,
                        Some(zone_name.as_str()),
                        cur_sec,
                        &mut sim.rng,
                    ) {
                        sim.players[ri].sim_state.mark_spoke(cur_sec);
                        if let Some(t) = topic {
                            sim.players[ri].sim_state.mark_topic_spoken(t, cur_sec);
                        }
                        emit_group_chat(sim, ctx, ri, &msg);
                    }
                }
            }
        }

        // ── Generic idle chat ─────────────────────────────────────────────────
        let idle_pct: u32 = match sim.chat_level {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 4,
            _ => 7,
        };
        if enc_sec > 5 && sim.roll(idle_pct) {
            if let Some(ri) = pick_responder(&sim.players, None, &mut sim.rng) {
                let trigger = ChatTrigger::Generic;
                let cur_sec = sim.sec;
                let personality = sim.players[ri].personality.clone();
                let topic = trigger.topic_hint();
                if let Some(msg) = chat.respond(
                    &trigger,
                    &personality,
                    &sim.players[ri].sim_state,
                    group_size,
                    Some(zone_name.as_str()),
                    cur_sec,
                    &mut sim.rng,
                ) {
                    sim.players[ri].sim_state.mark_spoke(cur_sec);
                    if let Some(t) = topic {
                        sim.players[ri].sim_state.mark_topic_spoken(t, cur_sec);
                    }
                    emit_group_chat(sim, ctx, ri, &msg);
                }
            }
        }
    }

    sim.encounters_done += 1;
    // Restore neural backend for the next encounter
    #[cfg(feature = "neural")]
    {
        sim.neural = chat.into_neural_backend();
    }
}

fn pick_responder(players: &[Player], exclude: Option<usize>, rng: &mut StdRng) -> Option<usize> {
    let candidates: Vec<usize> = (0..players.len())
        .filter(|&i| exclude.map(|e| i != e).unwrap_or(true) && players[i].hp > 0)
        .collect();
    candidates.choose(rng).copied()
}

/// Prefer monk/ranger/bard as puller, then any non-tank non-healer.
fn pick_puller(players: &[Player], rng: &mut StdRng) -> Option<usize> {
    let preferred: Vec<usize> = (0..players.len())
        .filter(|&i| {
            matches!(
                players[i].classes.primary,
                Class::Monk | Class::Ranger | Class::Bard
            )
        })
        .collect();
    if !preferred.is_empty() {
        return preferred.choose(rng).copied();
    }
    let fallback: Vec<usize> = (0..players.len())
        .filter(|&i| {
            let role = players[i].classes.role();
            role != Role::Tank && role != Role::Healer
        })
        .collect();
    if !fallback.is_empty() {
        return fallback.choose(rng).copied();
    }
    (0..players.len()).collect::<Vec<_>>().choose(rng).copied()
}

/// Prefer enchanter/bard for crowd control, then any non-tank.
fn pick_cc_player(players: &[Player], rng: &mut StdRng) -> Option<usize> {
    let preferred: Vec<usize> = (0..players.len())
        .filter(|&i| matches!(players[i].classes.primary, Class::Enchanter | Class::Bard))
        .collect();
    if !preferred.is_empty() {
        return preferred.choose(rng).copied();
    }
    let fallback: Vec<usize> = (0..players.len())
        .filter(|&i| players[i].classes.role() != Role::Tank)
        .collect();
    fallback.choose(rng).copied()
}

fn emit_group_chat(sim: &Sim, ctx: &mut Ctx, speaker: usize, msg: &str) {
    let dt = sim.dt();
    if speaker == 0 {
        ctx.emit(dt, &format!("You tell your party, '{}'", msg));
    } else {
        ctx.emit(
            dt,
            &format!("{} tells the group, '{}'", sim.players[speaker].name, msg),
        );
    }
}

// ── Recovery break ────────────────────────────────────────────────────────────

fn run_break(sim: &mut Sim, ctx: &mut Ctx, cfg: &GameConfig) {
    let group_size = sim.players.len() as u8;
    let zone_name: String = sim.zone.name.clone();
    #[cfg(feature = "neural")]
    let mut chat: ChatDispatch<'_> = {
        let backend = sim.neural.take();
        if let Some(b) = backend {
            ChatDispatch::Neural(NeuralCtx::new(b, &cfg.phrases, sim.chat_level))
        } else {
            ChatDispatch::Phrasebook(ChatCtx::new(&cfg.phrases, sim.chat_level))
        }
    };
    #[cfg(not(feature = "neural"))]
    let mut chat: ChatDispatch<'_> =
        ChatDispatch::Phrasebook(ChatCtx::new(&cfg.phrases, sim.chat_level));
    if sim.roll(40) {
        if let Some(ri) = pick_responder(&sim.players, None, &mut sim.rng) {
            let trigger = ChatTrigger::Generic;
            let cur_sec = sim.sec;
            let personality = sim.players[ri].personality.clone();
            let topic = trigger.topic_hint();
            if let Some(msg) = chat.respond(
                &trigger,
                &personality,
                &sim.players[ri].sim_state,
                group_size,
                Some(zone_name.as_str()),
                cur_sec,
                &mut sim.rng,
            ) {
                sim.players[ri].sim_state.mark_spoke(cur_sec);
                if let Some(t) = topic {
                    sim.players[ri].sim_state.mark_topic_spoken(t, cur_sec);
                }
                emit_group_chat(sim, ctx, ri, &msg);
            }
        }
    }

    let break_dur = sim.rand_range(30, 65);
    for p in sim.players.iter_mut() {
        p.sim_state.decay(break_dur as f32);
    }
    sim.sec += break_dur;

    for p in sim.players.iter_mut() {
        if p.mana_max > 0 {
            p.mana = p.mana_max;
        }
        let regen = (p.hp_max as f32 * 0.4) as i32;
        p.hp = (p.hp + regen).min(p.hp_max);
    }

    if sim.encounters_done.is_multiple_of(5) && !sim.loot_bag.is_empty() {
        run_vendor(sim, ctx);
    }
    // Restore neural backend after break
    #[cfg(feature = "neural")]
    {
        sim.neural = chat.into_neural_backend();
    }
}

// ── Vendor ────────────────────────────────────────────────────────────────────

fn run_vendor(sim: &mut Sim, ctx: &mut Ctx) {
    let vendor = sim.zone.vendor.clone();
    let items: Vec<(String, u32)> = sim.loot_bag.drain(..).collect();
    if items.is_empty() {
        return;
    }

    let mut dt = sim.dt();
    let mut bulk_items: Vec<(String, u32)> = Vec::new();
    let mut individual_items: Vec<(String, u32)> = Vec::new();

    for (item, val) in &items {
        if *val < 500 {
            bulk_items.push((item.clone(), *val));
        } else {
            individual_items.push((item.clone(), *val));
        }
    }

    for (item, val) in &individual_items {
        let price = vendor_price_str(*val);
        ctx.emit(
            dt,
            &format!(
                "{} told you, 'I'll give you {} for the {}'.",
                vendor, price, item
            ),
        );
        adv(&mut dt, 1);
        ctx.emit(
            dt,
            &format!("You receive {} from {} for the {}(s).", price, vendor, item),
        );
        adv(&mut dt, 1);
    }

    if !bulk_items.is_empty() {
        let bulk_total: u32 = bulk_items.iter().map(|(_, v)| v).sum();
        for (item, val) in &bulk_items {
            let price = vendor_price_str(*val);
            ctx.emit(
                dt,
                &format!(
                    "{} told you, 'I'll give you {} per {}'",
                    vendor, price, item
                ),
            );
        }
        adv(&mut dt, 4);
        let bag_price = vendor_price_str(bulk_total);
        ctx.emit(
            dt,
            &format!(
                "You receive  {} from {} for the contents of your bag.",
                bag_price, vendor
            ),
        );
        adv(&mut dt, 1);
    }

    sim.sec = ((dt - sim.base).num_seconds()).max(0) as u32;
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let cfg = GameConfig::load(args.config_dir.as_deref());

    let mut rng = if let Some(seed) = args.seed {
        StdRng::seed_from_u64(seed)
    } else {
        StdRng::from_entropy()
    };

    // Choose zone
    let zone: ZoneDef = if let Some(ref z) = args.zone {
        cfg.zone_by_key(z).cloned().unwrap_or_else(|| {
            eprintln!("warning: zone '{}' not found, picking randomly", z);
            cfg.zones
                .choose(&mut rng)
                .expect("no zones defined")
                .clone()
        })
    } else {
        cfg.zones
            .choose(&mut rng)
            .expect("no zones defined")
            .clone()
    };

    let player_count = args
        .players
        .unwrap_or_else(|| rng.gen_range(2..=6))
        .clamp(2, 8);

    let you_name = args
        .player_name
        .clone()
        .unwrap_or_else(|| "Soandso".to_string());

    // Generate a random guild name for the whole group
    let guild = namegen::generate_guild_name(&mut rng);

    let base = if args.realtime {
        chrono::Local::now().naive_local()
    } else {
        use chrono::Datelike;
        let now = chrono::Local::now().naive_local();
        let d = now.date();
        let h = rng.gen_range(18u32..23);
        NaiveDate::from_ymd_opt(d.year(), d.month(), d.day())
            .unwrap()
            .and_hms_opt(h, rng.gen_range(0u32..60), 0)
            .unwrap()
    };

    let out_box: Box<dyn Write> = if let Some(ref path) = args.output {
        Box::new(BufWriter::new(
            File::create(path).expect("could not create output file"),
        ))
    } else {
        Box::new(BufWriter::new(io::stdout()))
    };

    let mut ctx = Ctx::new(out_box, args.realtime, base);

    let players = build_group(&you_name, player_count, &mut rng, &guild, zone.min_level);

    let mut sim = Sim {
        players,
        mobs: Vec::new(),
        dots: Vec::new(),
        hots: Vec::new(),
        casts: Vec::new(),
        sec: 0,
        base,
        rng,
        zone,
        loot_bag: Vec::new(),
        encounters_done: 0,
        difficulty: args.difficulty,
        intensity: args.intensity,
        chat_level: args.chat,
        #[cfg(feature = "neural")]
        neural: None,
    };

    // Load neural chat backend (if --model-dir given or model files found next to binary)
    #[cfg(feature = "neural")]
    {
        let model_dir = args.model_dir.as_deref().map(std::path::Path::new);
        sim.neural = try_load_neural_backend(model_dir);
        if sim.neural.is_none() {
            tracing::info!("Chat backend: phrasebook");
        } else {
            tracing::info!("Chat backend: neural");
        }
    }
    #[cfg(not(feature = "neural"))]
    tracing::info!("Chat backend: phrasebook (binary built without --features neural)");

    // /who simulation
    emit_who(&sim, &mut ctx);
    sim.sec += sim.rng.gen_range(5u32..20);

    // Zone entry
    ctx.emit(sim.dt(), &format!("You have entered {}.", sim.zone.name));
    sim.sec += sim.rng.gen_range(5u32..30);

    let deadline = args.duration.map(|d| std::time::Instant::now() + d);
    let mut encounter_count = 0u32;
    loop {
        if let Some(limit) = args.encounters {
            if encounter_count >= limit {
                break;
            }
        }
        if let Some(dl) = deadline {
            if std::time::Instant::now() >= dl {
                break;
            }
        }
        encounter_count += 1;
        run_encounter(&mut sim, &mut ctx, &cfg);

        let needs_break = sim
            .players
            .iter()
            .any(|p| p.mana_max > 0 && p.mana < p.mana_max / 4);
        if needs_break || sim.rng.gen_range(0u32..100) < 20 {
            run_break(&mut sim, &mut ctx, &cfg);
        }
        sim.sec += args.gap;
    }

    if !sim.loot_bag.is_empty() {
        run_vendor(&mut sim, &mut ctx);
    }

    let dt = sim.dt();
    ctx.emit(dt, &format!("You have left {}.", sim.zone.name));
    ctx.out.flush().ok();
}
