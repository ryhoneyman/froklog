use rand::rngs::StdRng;
use rand::Rng;

// Phoneme tables tuned to produce EverQuest-style fantasy names.
// Names are built as: PREFIX + (optional MIDDLE) + SUFFIX

const PREFIXES: &[&str] = &[
    "Vel", "Sor", "Bry", "Kaz", "Del", "Mor", "Pyr", "Tel", "Zel", "Fen", "Tor", "Mel", "Zan",
    "Ash", "Drov", "Kel", "Xap", "Ret", "Aur", "Cor", "Nyl", "Mar", "Val", "Vae", "Gre", "Thal",
    "Mah", "Wyn", "Ith", "Reth", "Elh", "Vor", "Sorn", "Bral", "Keth", "Dral", "Fael", "Zyn",
    "Jor", "Lyh", "Ald", "Bryn", "Ceth", "Dwyn", "Elar", "Forn", "Gath", "Hel", "Ilth", "Jael",
    "Korn", "Lyr", "Myth", "Naer", "Orvyn", "Pael", "Quor", "Ryh", "Sel", "Thel", "Ulv", "Veld",
    "Wyr", "Xor", "Yael", "Zael", "Aeld", "Bael", "Cael", "Dryn",
];

const MIDDLES: &[&str] = &[
    "ar", "or", "al", "el", "il", "eth", "oth", "an", "en", "ith", "ath", "orn", "aer", "alh",
    "olh", "ur", "in", "on", "ash", "esh", "ish", "osh", "om", "em",
];

const SUFFIXES: &[&str] = &[
    "rath", "dris", "thar", "wyn", "nar", "lis", "ath", "ox", "oth", "ar", "is", "in", "an", "ix",
    "on", "or", "yn", "us", "os", "el", "riel", "olyn", "than", "seth", "this", "eem", "ian",
    "sar", "wen", "ith", "nor", "vex", "mael", "dyn", "thox", "rell", "mir", "soth", "dor", "vel",
    "kin", "mar", "tor", "fen", "ash", "keth", "lorn", "drax", "nox", "wyl", "fael", "gorn",
    "heth", "jal", "kael", "leth",
];

pub fn generate_name(rng: &mut StdRng) -> String {
    let prefix = PREFIXES[rng.gen_range(0..PREFIXES.len())];
    let suffix = SUFFIXES[rng.gen_range(0..SUFFIXES.len())];

    // 35% chance to insert a middle syllable for longer names
    if rng.gen_bool(0.35) {
        let mid = MIDDLES[rng.gen_range(0..MIDDLES.len())];
        format!("{}{}{}", prefix, mid, suffix)
    } else {
        format!("{}{}", prefix, suffix)
    }
}

// ── Guild name generation ─────────────────────────────────────────────────────
//
// Inspired by EQ guild names such as:
//   "Unified Council", "Crimson Tide", "Sanctum of Shadows",
//   "The Wayfarers Brotherhood", "Kingdom of Darkness", "Fires of Heaven",
//   "Azure Twilight", "Nest of Serpents", "Raging Fury",
//   "Prophecy of Ro", "Silent Redemption", "Ascending Dawn"
//
// Three structural patterns are chosen at random:
//   1. [Adjective] [Noun]           — "Crimson Tide", "Silent Redemption"
//   2. [Noun] of [Noun]             — "Sanctum of Shadows", "Fires of Heaven"
//   3. The [Collective] [Noun]      — "The Wayfarers Brotherhood"

const GUILD_ADJECTIVES: &[&str] = &[
    "Ascending",
    "Azure",
    "Blazing",
    "Crimson",
    "Dark",
    "Defiant",
    "Devoted",
    "Eternal",
    "Fallen",
    "Fierce",
    "Forsaken",
    "Gilded",
    "Hallowed",
    "Immortal",
    "Iron",
    "Jade",
    "Molten",
    "Obsidian",
    "Radiant",
    "Raging",
    "Revered",
    "Righteous",
    "Sacred",
    "Scarlet",
    "Shadowed",
    "Silent",
    "Silver",
    "Sombre",
    "Sovereign",
    "Steeled",
    "Swift",
    "Twilight",
    "Undying",
    "Unified",
    "Unyielding",
    "Valiant",
    "Veiled",
    "Verdant",
    "Vile",
    "Wrathful",
];

const GUILD_ABSTRACT_NOUNS: &[&str] = &[
    "Accord",
    "Ascension",
    "Brotherhood",
    "Calling",
    "Covenant",
    "Dawn",
    "Decree",
    "Defiance",
    "Dominion",
    "Dusk",
    "Eternity",
    "Exile",
    "Fate",
    "Fury",
    "Genesis",
    "Glory",
    "Grace",
    "Honor",
    "Judgment",
    "Legacy",
    "Malice",
    "Nexus",
    "Oath",
    "Oblivion",
    "Omen",
    "Order",
    "Pact",
    "Prophecy",
    "Rage",
    "Reckoning",
    "Redemption",
    "Requiem",
    "Resolve",
    "Retribution",
    "Revelation",
    "Ruin",
    "Sorrow",
    "Sovereignty",
    "Tempest",
    "Tide",
    "Twilight",
    "Unity",
    "Vengeance",
    "Verdict",
    "Vigil",
    "Virtue",
    "Void",
    "Wrath",
    "Zeal",
];

const GUILD_OF_PLACES: &[&str] = &[
    "Darkness",
    "Dawn",
    "Death",
    "Despair",
    "Eternity",
    "Fire",
    "Heaven",
    "Honor",
    "Light",
    "Night",
    "Ro",
    "Ruin",
    "Shadow",
    "Silence",
    "Storm",
    "the Abyss",
    "the Ancients",
    "the Beyond",
    "the Deep",
    "the Fallen",
    "the Forsaken",
    "the Void",
    "Thunder",
    "Twilight",
    "War",
    "Wrath",
];

const GUILD_PLACE_NOUNS: &[&str] = &[
    "Bastion", "Circle", "Citadel", "Council", "Cult", "Domain", "Ember", "Empire", "Enclave",
    "Fire", "Fires", "Forge", "Halls", "Haven", "Keep", "Kingdom", "Legion", "Nest", "Order",
    "Realm", "Sanctum", "Siege", "Temple", "Throne", "Tower", "Vanguard", "Vault",
];

const GUILD_COLLECTIVES: &[&str] = &[
    "Ancient",
    "Arcane",
    "Battleworn",
    "Bloodsworn",
    "Dawnbringers",
    "Deathknights",
    "Devoted",
    "Dreadguard",
    "Elderborn",
    "Eternal",
    "Exiled",
    "Faithful",
    "Fallen",
    "Forsaken",
    "Frostborn",
    "Hallowed",
    "Immortal",
    "Ironclad",
    "Lightbearers",
    "Oathsworn",
    "Obsidian",
    "Risen",
    "Sacred",
    "Sages",
    "Sentinels",
    "Shadowborn",
    "Shieldsworn",
    "Silentblades",
    "Soulbound",
    "Stoneborn",
    "Sworn",
    "Templar",
    "Undying",
    "Vanguard",
    "Wayfarers",
    "Wraithborn",
];

const GUILD_GROUP_NOUNS: &[&str] = &[
    "Alliance",
    "Assembly",
    "Brotherhood",
    "Circle",
    "Clan",
    "Covenant",
    "Dominion",
    "Fellowship",
    "Guard",
    "Guild",
    "Legion",
    "Order",
    "Pact",
    "Sisterhood",
    "Society",
    "Union",
    "Vanguard",
];

pub fn generate_guild_name(rng: &mut StdRng) -> String {
    match rng.gen_range(0u32..3) {
        0 => {
            // [Adjective] [Abstract Noun]
            let adj = GUILD_ADJECTIVES[rng.gen_range(0..GUILD_ADJECTIVES.len())];
            let noun = GUILD_ABSTRACT_NOUNS[rng.gen_range(0..GUILD_ABSTRACT_NOUNS.len())];
            format!("{} {}", adj, noun)
        }
        1 => {
            // [Place/Power Noun] of [Noun/Place]
            let place = GUILD_PLACE_NOUNS[rng.gen_range(0..GUILD_PLACE_NOUNS.len())];
            let of_noun = GUILD_OF_PLACES[rng.gen_range(0..GUILD_OF_PLACES.len())];
            format!("{} of {}", place, of_noun)
        }
        _ => {
            // The [Collective] [Group Noun]
            let coll = GUILD_COLLECTIVES[rng.gen_range(0..GUILD_COLLECTIVES.len())];
            let group = GUILD_GROUP_NOUNS[rng.gen_range(0..GUILD_GROUP_NOUNS.len())];
            format!("The {} {}", coll, group)
        }
    }
}

pub const RACES: &[&str] = &[
    "Human",
    "Barbarian",
    "Erudite",
    "Wood Elf",
    "High Elf",
    "Dark Elf",
    "Half Elf",
    "Dwarf",
    "Troll",
    "Ogre",
    "Halfling",
    "Gnome",
    "Iksar",
    "Vah Shir",
    "Froglok",
];
