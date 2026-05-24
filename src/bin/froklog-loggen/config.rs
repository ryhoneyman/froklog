use froklog::chat::{PhraseDb, PhrasebookFile};
use serde::Deserialize;
use std::collections::HashMap;

// ── Zone / mob / loot types ───────────────────────────────────────────────────

#[derive(Deserialize, Clone, Debug)]
pub struct LootEntry {
    pub item: String,
    pub sell_value: u32,
    pub keep_chance: u32,
    #[serde(default)]
    pub enhanceable: bool,
}

#[derive(Deserialize, Debug)]
struct LootTable {
    id: String,
    entries: Vec<LootEntry>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct MobDef {
    pub article: String,
    pub name: String,
    pub hp_lo: u32,
    pub hp_hi: u32,
    pub dmg_lo: u32,
    pub dmg_hi: u32,
    pub attack_delay: u32,
    #[serde(default)]
    pub spells: Vec<String>,
    pub loot_table: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ZoneDef {
    pub key: String,
    pub name: String,
    pub vendor: String,
    #[serde(default)]
    pub min_level: u32,
    pub mobs: Vec<MobDef>,
}

#[derive(Deserialize, Debug)]
struct ZonesFile {
    loot_tables: Vec<LootTable>,
    zones: Vec<ZoneDef>,
}

// ── Spell types ───────────────────────────────────────────────────────────────

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpellKind {
    Nuke,
    Dot,
    DirectHeal,
    #[serde(rename = "HoT")]
    HoT,
    PromisedHeal,
    Slow,
    Debuff,
    Buff,
    Snare,
    Root,
    Mez,
    Lifetap,
}

#[derive(Deserialize, Clone, Debug)]
pub struct SpellDef {
    pub class: String,
    pub name: String,
    pub kind: SpellKind,
    #[serde(default)]
    pub level: u32,
    pub mana_cost: i32,
    pub cast_secs: u32,
    #[serde(default)]
    pub min_val: u32,
    #[serde(default)]
    pub max_val: u32,
    #[serde(default)]
    pub ticks: u32,
    #[serde(default)]
    pub tick_secs: u32,
    #[serde(default)]
    pub resist_pct: u32,
    #[serde(default)]
    pub land_msg: String,
}

#[derive(Deserialize, Debug)]
struct SpellsFile {
    spells: Vec<SpellDef>,
}

// ── Gear types ────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Deserialize, Clone, Debug)]
pub struct WeaponDef {
    pub name: String,
    pub slot: String,
    pub dmg_lo: u32,
    pub dmg_hi: u32,
    pub delay: u32,
    pub classes: Vec<String>,
    #[serde(default)]
    pub lore: bool,
}

#[allow(dead_code)]
#[derive(Deserialize, Clone, Debug)]
pub struct ArmorDef {
    pub name: String,
    pub slot: String,
    pub ac: u32,
    pub classes: Vec<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug, Default)]
pub struct GearFile {
    #[serde(default)]
    pub weapons: Vec<WeaponDef>,
    #[serde(default)]
    pub armor: Vec<ArmorDef>,
}

// ── Aggregate config ──────────────────────────────────────────────────────────

pub struct GameConfig {
    pub zones: Vec<ZoneDef>,
    pub loot_tables: HashMap<String, Vec<LootEntry>>,
    pub spells_by_class: HashMap<String, Vec<SpellDef>>,
    #[allow(dead_code)]
    pub gear: GearFile,
    pub phrases: PhraseDb,
}

impl GameConfig {
    pub fn load(config_dir: Option<&str>) -> Self {
        let zones_raw = load_text(
            config_dir,
            "zones.toml",
            include_str!("../../../data/loggen/zones.toml"),
        );
        let spells_raw = load_text(
            config_dir,
            "spells.toml",
            include_str!("../../../data/loggen/spells.toml"),
        );
        let gear_raw = load_text(
            config_dir,
            "gear.toml",
            include_str!("../../../data/loggen/gear.toml"),
        );
        let phrasebook_raw = load_text(
            config_dir,
            "phrasebook.toml",
            include_str!("../../../data/loggen/phrasebook.toml"),
        );

        let zones_file: ZonesFile = toml::from_str(&zones_raw).expect("invalid zones.toml");
        let spells_file: SpellsFile = toml::from_str(&spells_raw).expect("invalid spells.toml");
        let gear_file: GearFile = toml::from_str(&gear_raw).expect("invalid gear.toml");
        let phrasebook_file: PhrasebookFile =
            toml::from_str(&phrasebook_raw).expect("invalid phrasebook.toml");

        let loot_tables: HashMap<String, Vec<LootEntry>> = zones_file
            .loot_tables
            .into_iter()
            .map(|lt| (lt.id, lt.entries))
            .collect();

        let mut spells_by_class: HashMap<String, Vec<SpellDef>> = HashMap::new();
        for spell in spells_file.spells {
            spells_by_class
                .entry(spell.class.clone())
                .or_default()
                .push(spell);
        }
        // Highest-level spells first so pick helpers can use max_by_key efficiently
        for spells in spells_by_class.values_mut() {
            spells.sort_by_key(|s| u32::MAX - s.level);
        }

        GameConfig {
            zones: zones_file.zones,
            loot_tables,
            spells_by_class,
            gear: gear_file,
            phrases: PhraseDb::from_file(phrasebook_file),
        }
    }

    pub fn zone_by_key(&self, key: &str) -> Option<&ZoneDef> {
        let k = key.to_lowercase();
        self.zones
            .iter()
            .find(|z| z.key == k || z.name.to_lowercase().contains(&k))
    }

    pub fn spells_for(&self, class_name: &str) -> &[SpellDef] {
        self.spells_by_class
            .get(class_name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn loot_for(&self, table_id: &str) -> &[LootEntry] {
        self.loot_tables
            .get(table_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

fn load_text(config_dir: Option<&str>, filename: &str, default: &str) -> String {
    if let Some(dir) = config_dir {
        let path = std::path::Path::new(dir).join(filename);
        match std::fs::read_to_string(&path) {
            Ok(content) => return content,
            Err(_) => eprintln!("warning: could not read {:?}, using embedded default", path),
        }
    }
    default.to_string()
}
