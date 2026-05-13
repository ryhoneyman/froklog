use once_cell::sync::Lazy;
use regex::Regex;

/// Length of the EQ log timestamp prefix: "[Fri Feb 27 22:00:07 2026] " (27 chars).
pub const TS_LEN: usize = 27;

// ── Compiled patterns ─────────────────────────────────────────────────────────

pub static RE_MELEE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<src>[A-Za-z][A-Za-z`', ]*?) ",
        r"(?P<verb>hits|hit|slashes|slash|backstabs|backstab|bashes|bash|",
        r"kicks|kick|crushes|crush|pierces|pierce|punches|punch|",
        r"frenzies|frenzy|strikes|strike|slays|slay|mauls|maul|",
        r"bites|bite|claws|claw|stings|sting|rends|rend|",
        r"scratches|scratch|gores|gore|cleaves|cleave|smashes|smash|",
        r"shoots|shoot|slams|slam|slices|slice|stabs|stab|sweeps|sweep) ",
        r"(?:on )?",
        r"(?P<tgt>[A-Za-z][A-Za-z `',]*?) for (?P<dmg>\d+) point"
    )).unwrap()
});

// "X hit Y for N points of TYPE damage by SpellName" — attributed direct-damage spell hit.
// Must be matched BEFORE RE_MELEE so that the generic "hit" verb in spell lines isn't
// misidentified as a mob melee attack.
pub static RE_HIT_BY_SPELL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<src>[A-Za-z][A-Za-z`', ]*?) hit ",
        r"(?P<tgt>[A-Za-z][A-Za-z `',]*?) ",
        r"for (?P<dmg>\d+) points? of \w+ damage by ",
        r"(?P<spell>[A-Za-z][A-Za-z `'-]+?)\.*$"
    )).unwrap()
});

// "Player's SpellName hit Target for X" — proc/attributed spell
pub static RE_SPELL_ATTR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<src>[A-Za-z][A-Za-z`']*)'s (?P<spell>[A-Za-z][A-Za-z `']+?) hit (?P<tgt>[A-Za-z][A-Za-z `',]*?) for (?P<dmg>\d+) point").unwrap()
});

// "SpellName hit Target for X" — needs spell→caster lookup
pub static RE_SPELL_HIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<spell>[A-Za-z][A-Za-z `']+?) hit (?P<tgt>[A-Za-z][A-Za-z `',]*?) for (?P<dmg>\d+) point").unwrap()
});

// "Target has been damaged by Player's SpellName for X" — DoT tick
pub static RE_DOT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',]*?) has been damaged by (?P<src>[A-Za-z][A-Za-z`']*)'s (?P<spell>[A-Za-z][A-Za-z `']+?) for (?P<dmg>\d+)").unwrap()
});

// "Target was injured by Player's riposte for X"
pub static RE_RIPOSTE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',]*?) was injured by (?P<src>[A-Za-z][A-Za-z`']*)'s riposte for (?P<dmg>\d+)").unwrap()
});

// "Target was struck by Player's damage shield for X"
pub static RE_DS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',]*?) was struck by (?P<src>[A-Za-z][A-Za-z`']*)'s damage shield for (?P<dmg>\d+)").unwrap()
});

// "Mob is burned by YOUR flames for N points of non-melee damage."
// "Mob is burned by Player's flames for N points of non-melee damage."
// Outbound DS proc: a player's damage shield retaliating against an attacker.
pub static RE_DS_PROC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<tgt>[A-Za-z][A-Za-z `',]*?) is \w+ by ",
        r"(?:(?P<src>[A-Za-z][A-Za-z`']*)'s|YOUR) \w+ ",
        r"for (?P<dmg>\d+) points? of non-melee damage\."
    )).unwrap()
});

// "Player begins casting SpellName."
pub static RE_CAST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<src>[A-Za-z][A-Za-z`', ]*) begins? casting (?P<spell>[A-Za-z][A-Za-z `']+?)\.").unwrap()
});

// "X healed Y for Z (optional_overheal) hit points by SpellName."
pub static RE_HEAL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<src>[A-Za-z][A-Za-z`']*) (?:has )?healed (?P<tgt>[A-Za-z][A-Za-z `',]*?) for (?P<amt>\d+)(?: \(\d+\))? hit points?(?: by (?P<spell>[A-Za-z][A-Za-z `'-]+))?\.?$").unwrap()
});

// "X has/have taken N damage from [Player's / your] Spell[ by Player]."
// Covers both player→mob (your / src's) and mob→player (by killer) DoT/DD variant.
pub static RE_HAS_TAKEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<tgt>.+?) (?:has|have) taken (?P<dmg>\d+) damage from ",
        r"(?:(?P<src>[A-Za-z][A-Za-z`']*)'s |(?P<your>your) )?",
        r"(?P<spell>.+?)(?: by (?P<by_src>.+?))?\.?\s*$"
    )).unwrap()
});

// "X has taken an extra N points of non-melee damage from [Player's/your] Spell spell."
// Bane/extra proc damage.
pub static RE_EXTRA_DMG: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<tgt>.+?) has taken an extra (?P<dmg>\d+) points? of non-melee damage from ",
        r"(?:(?P<src>[A-Za-z][A-Za-z`']*)'s |(?P<your>your) )",
        r"(?P<spell>.+?) spell\.$"
    )).unwrap()
});

// "X has slain Y!" — killer=X, target=Y.
pub static RE_SLAY_HAS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<killer>[A-Za-z][A-Za-z`', ]*?) has slain (?P<tgt>[A-Za-z][A-Za-z `',]+)!").unwrap()
});

// "You have slain Y!" — killer is the local player.
pub static RE_SLAY_YOU: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^You have slain (?P<tgt>[A-Za-z][A-Za-z `',]+)!").unwrap()
});

// "Y was slain by X!" / "Y has been slain by X!" — reversed order.
pub static RE_SLAIN_BY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z`', ]*?) (?:was|has been) slain by (?P<killer>[A-Za-z][A-Za-z `',]+?)!?\s*$").unwrap()
});

// "X died." — entity died with no explicit killer.
pub static RE_DIED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',]+) died\.$").unwrap()
});

// "X tries to VERB Y, but [Y] dodge/parry/miss/block/riposte/INVULNERABLE/absorb…"
// Both src and tgt may be multi-word.  Miss type is the first keyword after "but".
pub static RE_MISS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<src>[A-Za-z][A-Za-z`', ]*?) tries? to \w+ (?P<tgt>[A-Za-z][A-Za-z `',]*?),",
        r" but .*?(?P<miss>dodge[sd]?|parr(?:ied|ies|y)|miss(?:ed|es)?|block(?:ed|s)?|",
        r"ripost(?:ed|es)?|INVULNERABLE|absorbs?)"
    )).unwrap()
});

// "X's magical skin absorbs the damage of Y's thorns."
// "YOUR magical skin absorbs the damage of Y's thorns."
pub static RE_ABSORB_SKIN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?:(?P<tgt>[A-Za-z][A-Za-z`']*)'s|YOUR) magical skin absorbs the damage of ",
        r"(?P<src>[A-Za-z][A-Za-z`', ]*?)'s .+$"
    )).unwrap()
});

// "X has shielded [itself/herself/himself] from N points of damage."  (Rune absorption)
pub static RE_ABSORB_RUNE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',]*?) has shielded \w+ from \d+ points? of damage\.").unwrap()
});

// "NPC resisted your Spell!"  or  "NPC resisted Player's Spell!"
pub static RE_RESIST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<tgt>[A-Za-z][A-Za-z `',]*?) resisted ",
        r"(?:your|(?P<src>[A-Za-z][A-Za-z`']*)'s) ",
        r"(?P<spell>[A-Za-z][A-Za-z `'.-]+?)!$"
    )).unwrap()
});

// "/who" player listing: "[Level Class1 [Class2 [Class3]]] Name (Race)"
// Matches 1–3 class names (space-separated) inside the brackets, then the player name.
pub static RE_WHO: Lazy<Regex> = Lazy::new(|| {
    // Classes may be full names ("Warrior Monk") or slash-separated short codes ("PAL/MNK/BER").
    Regex::new(r"^\[(?P<lvl>\d+) (?P<classes>[A-Za-z][A-Za-z /]+?)\] (?P<name>[A-Za-z][A-Za-z`']+) \(").unwrap()
});

// ── Pure helper functions ─────────────────────────────────────────────────────

pub fn norm(name: &str, player: &str) -> String {
    if !player.is_empty() && name.eq_ignore_ascii_case("you") {
        return player.to_owned();
    }
    normalize_article_case(name)
}

/// Lowercase "A "/"An " articles at the start of a name.
/// EQ capitalises these when the mob name opens a sentence.
pub fn normalize_article_case(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("A ") {
        format!("a {rest}")
    } else if let Some(rest) = name.strip_prefix("An ") {
        format!("an {rest}")
    } else {
        name.to_owned()
    }
}

pub fn normalize_verb(verb: &str) -> &'static str {
    match verb {
        "hit" | "hits"             => "hit",
        "slash" | "slashes"        => "slash",
        "backstab" | "backstabs"   => "backstab",
        "bash" | "bashes"          => "bash",
        "kick" | "kicks"           => "kick",
        "crush" | "crushes"        => "crush",
        "pierce" | "pierces"       => "pierce",
        "punch" | "punches"        => "punch",
        "frenzy" | "frenzies"      => "frenzy",
        "strike" | "strikes"       => "strike",
        "slay" | "slays"           => "slay",
        "maul" | "mauls"           => "maul",
        "bite" | "bites"           => "bite",
        "claw" | "claws"           => "claw",
        "sting" | "stings"         => "sting",
        "rend" | "rends"           => "rend",
        "scratch" | "scratches"    => "scratch",
        "gore" | "gores"           => "gore",
        "cleave" | "cleaves"       => "cleave",
        "smash" | "smashes"        => "smash",
        "shoot" | "shoots"         => "shoot",
        "slam" | "slams"           => "slam",
        "slice" | "slices"         => "slice",
        "stab" | "stabs"           => "stab",
        "sweep" | "sweeps"         => "sweep",
        _                          => "hit",
    }
}

/// Map a full EQ class name (case-insensitive) to its 3-letter short code.
/// Handles both "Shadow Knight" (two words) and "Shadowknight" (one word).
pub fn class_name_to_code(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "warrior"  | "war"         => Some("WAR"),
        "cleric"   | "clr"         => Some("CLR"),
        "paladin"  | "pal"         => Some("PAL"),
        "ranger"   | "rng"         => Some("RNG"),
        "shadow knight" | "shadowknight" | "shd" => Some("SHD"),
        "druid"    | "dru"         => Some("DRU"),
        "monk"     | "mnk"         => Some("MNK"),
        "bard"     | "brd"         => Some("BRD"),
        "rogue"    | "rog"         => Some("ROG"),
        "shaman"   | "shm"         => Some("SHM"),
        "necromancer" | "nec"      => Some("NEC"),
        "wizard"   | "wiz"         => Some("WIZ"),
        "magician" | "mag"         => Some("MAG"),
        "enchanter" | "enc"        => Some("ENC"),
        "beastlord" | "bst"        => Some("BST"),
        "berserker" | "ber"        => Some("BER"),
        _                          => None,
    }
}

/// Parse the class section of a /who bracket into up to 3 short-codes.
/// Handles slash-separated short codes ("PAL/MNK/BER"), the "Shadow Knight"
/// two-word case, and single/multi full class names.
pub fn parse_who_classes(classes_str: &str) -> Vec<String> {
    // New server format: codes already present, separated by '/'
    if classes_str.contains('/') {
        return classes_str
            .split('/')
            .take(3)
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
    }
    let words: Vec<&str> = classes_str.split_whitespace().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < words.len() && result.len() < 3 {
        // Try two-word combination first (for "Shadow Knight").
        if i + 1 < words.len() {
            let two = format!("{} {}", words[i], words[i + 1]);
            if let Some(code) = class_name_to_code(&two) {
                result.push(code.to_owned());
                i += 2;
                continue;
            }
        }
        if let Some(code) = class_name_to_code(words[i]) {
            result.push(code.to_owned());
        }
        i += 1;
    }
    result
}

pub fn normalize_miss(word: &str) -> &'static str {
    match word {
        w if w.starts_with("dodge")  => "dodge",
        w if w.starts_with("parr")   => "parry",
        w if w.starts_with("miss")   => "miss",
        w if w.starts_with("block")  => "block",
        w if w.starts_with("ripost") => "riposte",
        w if w.starts_with("INVULN") => "invulnerable",
        w if w.starts_with("absorb") => "absorb",
        _                            => "miss",
    }
}

#[cfg(test)]
mod who_tests {
    use super::*;
    #[test]
    fn re_who_single_class() {
        let caps = RE_WHO.captures("[65 Warrior] Crunchy (Human)").unwrap();
        assert_eq!(&caps["name"], "Crunchy");
        assert_eq!(parse_who_classes(&caps["classes"]), vec!["WAR"]);
    }
    #[test]
    fn re_who_triple_class() {
        let caps = RE_WHO.captures("[50 Warrior Monk Rogue] Talodar (Barbarian) <Valor>").unwrap();
        assert_eq!(&caps["name"], "Talodar");
        assert_eq!(parse_who_classes(&caps["classes"]), vec!["WAR","MNK","ROG"]);
    }
    #[test]
    fn re_who_shadow_knight() {
        let caps = RE_WHO.captures("[60 Shadow Knight Cleric] Darkbane (Dark Elf)").unwrap();
        assert_eq!(&caps["name"], "Darkbane");
        assert_eq!(parse_who_classes(&caps["classes"]), vec!["SHD","CLR"]);
    }
    #[test]
    fn re_who_slash_codes() {
        let caps = RE_WHO.captures("[44 PAL/MNK/BER] Talodar (Wood Elf) <Stone> ZONE: The Greater Faydark (gfaydark)").unwrap();
        assert_eq!(&caps["name"], "Talodar");
        assert_eq!(parse_who_classes(&caps["classes"]), vec!["PAL","MNK","BER"]);
    }
    #[test]
    fn re_who_slash_single() {
        let caps = RE_WHO.captures("[60 WAR] Crunchy (Human)").unwrap();
        assert_eq!(&caps["name"], "Crunchy");
        assert_eq!(parse_who_classes(&caps["classes"]), vec!["WAR"]);
    }
    #[test]
    fn re_who_no_match_players_header() {
        assert!(RE_WHO.captures("Players in EverQuest:").is_none());
    }
}
