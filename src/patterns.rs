use once_cell::sync::Lazy;
use regex::Regex;

/// Length of the EQ log timestamp prefix: "[Fri Feb 27 22:00:07 2026] " (27 chars).
pub const TS_LEN: usize = 27;

// ── Compiled patterns ─────────────────────────────────────────────────────────

pub static RE_MELEE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<src>[A-Za-z][A-Za-z`', -]*?) ",
        r"(?P<verb>hits|hit|slashes|slash|backstabs|backstab|bashes|bash|",
        r"kicks|kick|crushes|crush|pierces|pierce|punches|punch|",
        r"frenzies|frenzy|strikes|strike|slays|slay|mauls|maul|",
        r"bites|bite|claws|claw|stings|sting|rends|rend|",
        r"scratches|scratch|gores|gore|cleaves|cleave|smashes|smash|",
        r"shoots|shoot|slams|slam|slices|slice|stabs|stab|sweeps|sweep|",
        r"smites|smite) ",
        r"(?:on )?",
        r"(?P<tgt>[A-Za-z][A-Za-z `',-]*?) for (?P<dmg>\d+) point"
    ))
    .unwrap()
});

// "X hit Y for N points of TYPE damage by SpellName" — attributed direct-damage spell hit.
// Must be matched BEFORE RE_MELEE so that the generic "hit" verb in spell lines isn't
// misidentified as a mob melee attack.
pub static RE_HIT_BY_SPELL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<src>[A-Za-z][A-Za-z`', -]*?) hit ",
        r"(?P<tgt>[A-Za-z][A-Za-z `',-]*?) ",
        r"for (?P<dmg>\d+) points? of \w+ damage by ",
        r"(?P<spell>[A-Za-z][A-Za-z `'-]+?)\.*$"
    ))
    .unwrap()
});

// "Player's SpellName hit Target for X" — proc/attributed spell
pub static RE_SPELL_ATTR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<src>[A-Za-z][A-Za-z`'-]*)'s (?P<spell>[A-Za-z][A-Za-z `'-]+?) hit (?P<tgt>[A-Za-z][A-Za-z `',-]*?) for (?P<dmg>\d+) point").unwrap()
});

// "SpellName hit Target for X" — needs spell→caster lookup
pub static RE_SPELL_HIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<spell>[A-Za-z][A-Za-z `'-]+?) hit (?P<tgt>[A-Za-z][A-Za-z `',-]*?) for (?P<dmg>\d+) point").unwrap()
});

// "Target has been damaged by Player's SpellName for X" — DoT tick
pub static RE_DOT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',-]*?) has been damaged by (?P<src>[A-Za-z][A-Za-z`'-]*)'s (?P<spell>[A-Za-z][A-Za-z `'-]+?) for (?P<dmg>\d+)").unwrap()
});

// "Target was injured by Player's riposte for X"
pub static RE_RIPOSTE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',-]*?) was injured by (?P<src>[A-Za-z][A-Za-z`'-]*)'s riposte for (?P<dmg>\d+)").unwrap()
});

// "Target was struck by Player's damage shield for X"
pub static RE_DS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',-]*?) was struck by (?P<src>[A-Za-z][A-Za-z`'-]*)'s damage shield for (?P<dmg>\d+)").unwrap()
});

// "Mob is burned by YOUR flames for N points of non-melee damage."
// "Mob is burned by Player's flames for N points of non-melee damage."
// Outbound DS proc: a player's damage shield retaliating against an attacker.
pub static RE_DS_PROC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<tgt>[A-Za-z][A-Za-z `',-]*?) is \w+ by ",
        r"(?:(?P<src>[A-Za-z][A-Za-z`'-]*)'s|YOUR) \w+ ",
        r"for (?P<dmg>\d+) points? of non-melee damage\."
    ))
    .unwrap()
});

// "YOU are burned by orc centurion's flames for 6 points of non-melee damage!"
// Inbound DS proc: a MOB's damage shield burning the player who struck it.
// Distinct from RE_DS_PROC: "YOU are" (not "is"), multi-word possessive mob
// name, and a trailing "!".
pub static RE_DS_BURN_YOU: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^YOU are \w+ by (?P<src>[A-Za-z][A-Za-z `'-]*?)'s \w+ for (?P<dmg>\d+) points? of non-melee damage[.!]",
    )
    .unwrap()
});

// "orc centurion has been mesmerized." / "a Tesch Mas Gnoll has been enthralled."
// Crowd-control landing on a mob: the target is deliberately parked.
pub static RE_CC_PARK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<tgt>[A-Za-z][A-Za-z `',-]*?) has been (?:mesmerized|enthralled|entranced)[.!]$",
    )
    .unwrap()
});

// "Orc centurion has been awakened by Zary." — crowd control broken.
pub static RE_CC_WAKE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<tgt>[A-Za-z][A-Za-z `',-]*?) has been awakened(?: by (?P<src>[A-Za-z`'-]+))?[.!]$",
    )
    .unwrap()
});

// Player-state lines that prove combat is ONGOING even though no mob is
// named: stunned, out of mana, interrupted, life-drained. During these the
// player looks idle in the log while the mobs may be on a merc/pet/tank whose
// combat never appears in this log — without a heartbeat the encounter would
// time out mid-fight.
pub static RE_HEARTBEAT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?:You are stunned!|You are no longer stunned\.",
        r"|You can't cast spells while stunned!",
        r"|You regain your concentration and continue your casting\.",
        r"|You feel your life force drain away\.",
        r"|Insufficient Mana to cast this spell!",
        r"|Your [A-Za-z `'-]+ spell is interrupted\.)$"
    ))
    .unwrap()
});

// "Player begins casting SpellName."
// Spell charset includes ':' and '-': "Lesser Summoning: Water" style names
// silently failed the whole match before, so colon-spells never produced
// Cast events at all.
pub static RE_CAST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<src>[A-Za-z][A-Za-z`', -]*) begins? casting (?P<spell>[A-Za-z][A-Za-z `':-]+?)\.",
    )
    .unwrap()
});

// "X healed Y for Z (optional_overheal) hit points by SpellName."
pub static RE_HEAL: Lazy<Regex> = Lazy::new(|| {
    // src allows spaces: mob healers ("an orc thaumaturgist healed itself…")
    // are multi-word and previously never matched at all.
    Regex::new(r"^(?P<src>[A-Za-z][A-Za-z `'-]*?) (?:has )?healed (?P<tgt>[A-Za-z][A-Za-z `',-]*?) for (?P<amt>\d+)(?: \(\d+\))? hit points?(?: by (?P<spell>[A-Za-z][A-Za-z `'-]+))?\.?$").unwrap()
});

// "X has/have taken N damage from [Player's / your] Spell[ by Player]."
// Covers both player→mob (your / src's) and mob→player (by killer) DoT/DD variant.
pub static RE_HAS_TAKEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<tgt>.+?) (?:has|have) taken (?P<dmg>\d+) damage from ",
        r"(?:(?P<src>[A-Za-z][A-Za-z`'-]*)'s |(?P<your>your) )?",
        r"(?P<spell>.+?)(?: by (?P<by_src>.+?))?\.?\s*$"
    ))
    .unwrap()
});

// "X has taken an extra N points of non-melee damage from [Player's/your] Spell spell."
// Bane/extra proc damage.
pub static RE_EXTRA_DMG: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<tgt>.+?) has taken an extra (?P<dmg>\d+) points? of non-melee damage from ",
        r"(?:(?P<src>[A-Za-z][A-Za-z`'-]*)'s |(?P<your>your) )",
        r"(?P<spell>.+?) spell\.$"
    ))
    .unwrap()
});

// "X has slain Y!" — killer=X, target=Y.
pub static RE_SLAY_HAS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<killer>[A-Za-z][A-Za-z`', -]*?) has slain (?P<tgt>[A-Za-z][A-Za-z `',-]+)!")
        .unwrap()
});

// "You have slain Y!" — killer is the local player.
pub static RE_SLAY_YOU: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^You have slain (?P<tgt>[A-Za-z][A-Za-z `',-]+)!").unwrap());

// "Y was slain by X!" / "Y has been slain by X!" — reversed order.
pub static RE_SLAIN_BY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z`', -]*?) (?:was|has been) slain by (?P<killer>[A-Za-z][A-Za-z `',-]+?)!?\s*$").unwrap()
});

// "X died." — entity died with no explicit killer.
pub static RE_DIED: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',-]+) died\.$").unwrap());

// "X tries to VERB Y, but [Y] dodge/parry/miss/block/riposte/INVULNERABLE/absorb…"
// Both src and tgt may be multi-word.  Miss type is the first keyword after "but".
pub static RE_MISS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        // "tr(?:y|ies)": first-person lines use "You try to slash X, but miss!"
        // — a previous "tries?" only matched "trie(s)", silently dropping
        // every player-side miss.
        r"^(?P<src>[A-Za-z][A-Za-z`', -]*?) tr(?:y|ies) to \w+ (?P<tgt>[A-Za-z][A-Za-z `',-]*?),",
        r" but .*?(?P<miss>dodge[sd]?|parr(?:ied|ies|y)|miss(?:ed|es)?|block(?:ed|s)?|",
        r"ripost(?:ed|es)?|INVULNERABLE|absorbs?)"
    ))
    .unwrap()
});

// "X's magical skin absorbs the damage of Y's thorns."
// "YOUR magical skin absorbs the damage of Y's thorns."
pub static RE_ABSORB_SKIN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?:(?P<tgt>[A-Za-z][A-Za-z`'-]*)'s|YOUR) magical skin absorbs the damage of ",
        r"(?P<src>[A-Za-z][A-Za-z`', -]*?)'s .+$"
    ))
    .unwrap()
});

// "X has shielded [itself/herself/himself] from N points of damage."  (Rune absorption)
pub static RE_ABSORB_RUNE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<tgt>[A-Za-z][A-Za-z `',-]*?) has shielded \w+ from \d+ points? of damage\.")
        .unwrap()
});

// "NPC resisted your Spell!"  or  "NPC resisted Player's Spell!"
pub static RE_RESIST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^(?P<tgt>[A-Za-z][A-Za-z `',-]*?) resisted ",
        r"(?:your|(?P<src>[A-Za-z][A-Za-z`'-]*)'s) ",
        r"(?P<spell>[A-Za-z][A-Za-z `'.-]+?)!$"
    ))
    .unwrap()
});

// "You receive 6 platinum, 1 gold, 8 silver and 3 copper from the corpse."
// "You receive 4 silver from the corpse."
pub static RE_CURRENCY_CORPSE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^You receive (?P<amounts>.+?) from the corpse\.$").unwrap());

// "--You have looted a Crystallized Sulfur from an abhorrent's corpse.--"
pub static RE_LOOT_KEPT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^--You have looted (?P<item>.+?) from (?P<mob>.+?)'s corpse\.--$").unwrap()
});

// "You looted 4 Giant Bat Fur from a sonic bat's corpse and sold it for 8 silver and 4 copper."
// "You looted a Bat Meat from a sonic bat's corpse and sold it for free."
// Quantity prefix is optional; article ("a"/"an") stays as part of item name when absent.
pub static RE_LOOT_SOLD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^You looted (?:(?P<qty>\d+) )?(?P<item>.+?) from (?P<mob>.+?)'s corpse and sold it for (?P<price>.+?)\.$").unwrap()
});

// "You looted a Throwing Boulder +1 from a fire giant warrior's corpse and stored it in your Dragon Hoard"
pub static RE_LOOT_HOARD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^You looted (?P<item>.+?) from (?P<mob>.+?)'s corpse and stored it in your Dragon Hoard",
    )
    .unwrap()
});

// "You looted a Darkbrood Mask +1 from Innoruuk, the Prince of Hate's corpse to create a Darkbrood Mask +1"
// `result` is the item you end up holding, which carries the new tier — the
// consumed item's own tier says nothing about what you now own.
pub static RE_LOOT_ENHANCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^You looted (?P<item>.+?) from (?P<mob>.+?)'s corpse to create (?P<result>.+?)\.?$",
    )
    .unwrap()
});

// "You have successfully merged two items together to create a new item: Boots of the Long Road +2"
pub static RE_ITEM_MERGE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^You have successfully merged two items together to create a new item: (?P<result>.+?)\.?$",
    )
    .unwrap()
});

static RE_CURRENCY_PARSE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\d+)\s*(platinum|gold|silver|copper)").unwrap());

/// Parse a currency string (e.g. "6 platinum, 1 gold, 8 silver and 3 copper" or
/// "132 platinum 7 gold 5 silver 9 copper") into a total copper value.
pub fn parse_copper(s: &str) -> u32 {
    let mut total: u32 = 0;
    for caps in RE_CURRENCY_PARSE.captures_iter(s) {
        let n: u32 = caps[1].parse().unwrap_or(0);
        let mult: u32 = match &caps[2] {
            "platinum" => 1000,
            "gold" => 100,
            "silver" => 10,
            _ => 1,
        };
        total = total.saturating_add(n.saturating_mul(mult));
    }
    total
}

// "/who" player listing: "[Level Class1 [Class2 [Class3]]] Name (Race)"
// Matches 1–3 class names (space-separated) inside the brackets, then the player name.
pub static RE_WHO: Lazy<Regex> = Lazy::new(|| {
    // Classes may be full names ("Warrior Monk") or slash-separated short codes ("PAL/MNK/BER").
    Regex::new(
        r"^\[(?P<lvl>\d+) (?P<classes>[A-Za-z][A-Za-z /]+?)\] (?P<name>[A-Za-z][A-Za-z`'-]+) \(",
    )
    .unwrap()
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
/// Extract the owner name from "Playername's warder" → Some("Playername").
/// Returns None if the name doesn't match the pattern or the prefix contains a space.
pub fn parse_warder_owner(name: &str) -> Option<&str> {
    name.strip_suffix("'s warder")
        .or_else(|| name.strip_suffix("`s warder"))
        .filter(|owner| !owner.is_empty() && !owner.contains(' '))
}

/// "<Pet> goes berserk." — the visible landing of a Burnout-family pet haste.
/// Paired with the preceding "begins casting Burnout" this reveals which
/// player owns which summoned pet: Burnout is only castable on the caster's
/// OWN pet, and generated pet names carry no owner of their own.
pub static RE_PET_BERSERK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?P<name>[A-Z][a-z]+) goes berserk\.$").unwrap());

/// Spells castable only on the caster's own pet whose landing is visible in
/// the log. Prefix-matched so ranks ("Burnout II") count.
pub fn is_pet_buff_spell(spell: &str) -> bool {
    spell.starts_with("Burnout")
}

// "Your Light Healing spell fizzles!" — own fizzles only (EQ does not log
// other players' fizzles), with the spell name.
pub static RE_FIZZLE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^Your (?P<sp>[A-Za-z][A-Za-z `':-]+?) spell fizzles!$").unwrap());

/// Pet-summoning spells, by the shapes seen on the server: mage
/// "Lesser Summoning: Water" (any rank/element), enchanter animations
/// ("Sisna's Animation"). The generated-name pet that first appears in
/// combat after one of these belongs to the caster.
pub fn is_pet_summon_spell(spell: &str) -> bool {
    spell.contains("Summoning:") || spell.ends_with(" Animation")
}

/// The classic EQ summoned-pet name generator (Gabann, Labarer, Xobtik…) —
/// the same syllable table the web viewer uses for its Pet badge. Summoned
/// pets draw a random name from this fixed space on every summon.
pub fn is_generated_pet_name(name: &str) -> bool {
    use std::collections::HashSet;
    static NAMES: Lazy<HashSet<String>> = Lazy::new(|| {
        let p1 = ["G", "J", "K", "L", "V", "X", "Z"];
        let p2 = ["", "ab", "ar", "as", "eb", "en", "ib", "ob", "on"];
        let p3 = ["", "an", "ar", "ek", "ob"];
        let p4 = ["er", "ab", "n", "tik"];
        // Real-word collisions excluded, mirroring the viewer.
        let blocked = ["Laser", "Ektik"];
        let mut s = HashSet::new();
        for a in p1 {
            for b in p2 {
                for c in p3 {
                    if b.is_empty() && c.is_empty() {
                        continue;
                    }
                    for d in p4 {
                        let name = format!("{a}{b}{c}{d}");
                        if !blocked.contains(&name.as_str()) {
                            s.insert(name);
                        }
                    }
                }
            }
        }
        s
    });
    NAMES.contains(name)
}

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
        "hit" | "hits" => "hit",
        "slash" | "slashes" => "slash",
        "backstab" | "backstabs" => "backstab",
        "bash" | "bashes" => "bash",
        "kick" | "kicks" => "kick",
        "crush" | "crushes" => "crush",
        "pierce" | "pierces" => "pierce",
        "punch" | "punches" => "punch",
        "frenzy" | "frenzies" => "frenzy",
        "strike" | "strikes" => "strike",
        "slay" | "slays" => "slay",
        "maul" | "mauls" => "maul",
        "bite" | "bites" => "bite",
        "claw" | "claws" => "claw",
        "sting" | "stings" => "sting",
        "rend" | "rends" => "rend",
        "scratch" | "scratches" => "scratch",
        "gore" | "gores" => "gore",
        "cleave" | "cleaves" => "cleave",
        "smite" | "smites" => "smite",
        "smash" | "smashes" => "smash",
        "shoot" | "shoots" => "shoot",
        "slam" | "slams" => "slam",
        "slice" | "slices" => "slice",
        "stab" | "stabs" => "stab",
        "sweep" | "sweeps" => "sweep",
        _ => "hit",
    }
}

/// Map a full EQ class name (case-insensitive) to its 3-letter short code.
/// Handles both "Shadow Knight" (two words) and "Shadowknight" (one word).
pub fn class_name_to_code(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "warrior" | "war" => Some("WAR"),
        "cleric" | "clr" => Some("CLR"),
        "paladin" | "pal" => Some("PAL"),
        "ranger" | "rng" => Some("RNG"),
        "shadow knight" | "shadowknight" | "shd" => Some("SHD"),
        "druid" | "dru" => Some("DRU"),
        "monk" | "mnk" => Some("MNK"),
        "bard" | "brd" => Some("BRD"),
        "rogue" | "rog" => Some("ROG"),
        "shaman" | "shm" => Some("SHM"),
        "necromancer" | "nec" => Some("NEC"),
        "wizard" | "wiz" => Some("WIZ"),
        "magician" | "mag" => Some("MAG"),
        "enchanter" | "enc" => Some("ENC"),
        "beastlord" | "bst" => Some("BST"),
        "berserker" | "ber" => Some("BER"),
        _ => None,
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
        w if w.starts_with("dodge") => "dodge",
        w if w.starts_with("parr") => "parry",
        w if w.starts_with("miss") => "miss",
        w if w.starts_with("block") => "block",
        w if w.starts_with("ripost") => "riposte",
        w if w.starts_with("INVULN") => "invulnerable",
        w if w.starts_with("absorb") => "absorb",
        _ => "miss",
    }
}

/// Any chat line, capturing the speaker and just what was said.
///
/// Deliberately verb-agnostic — matches `<speaker> <lowercase verb phrase>, '<msg>'`
/// without enumerating say/tell/shout/auction/etc., so it isn't a checklist that
/// silently misses a channel (an earlier enumerated version omitted "shouts,"
/// entirely, letting a shouted line reach the combat regexes below). The channel
/// name after the verb is matched loosely (`[^,]*`) on purpose: EQ Legends words
/// tell-to-channel differently from classic EQ — it says "You say to your guild"
/// where classic says "You tell your guild" — and custom channels carry arbitrary
/// names ("Zyro tells General:1, '...'").
///
///   You say to your guild, 'raid start'      <- your own /gu
///   You tell your party, 'raid start'        <- your own /g
///   You shout, 'incoming!'                   <- your own /shout
///   Kermitzalot tells the raid, 'raid start' <- someone else's
///   Zyro tells General:1, 'raid start'       <- a custom channel
///
/// `speaker` is the literal text "You" only for the log owner's own speech —
/// EQ's client never emits that literal prefix for anyone else's chat, and
/// another player's quoted text always lands *after* their own name+verb
/// preamble, never at line-start, so callers that gate on `speaker == "You"`
/// (e.g. raid marks) can't be spoofed by someone else's message content.
///
/// `verb` is the structural "says"/"tells the guild"/"shouts"/etc. segment,
/// separate from `msg` — trigger channel classification keys on `verb`, never
/// on `msg`, so quoted message text can't be crafted to misclassify which
/// channel carried it (see `triggers::engine::chat_channel_matches`).
pub static RE_CHAT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<speaker>[A-Za-z`'-]+) (?P<verb>[a-z][^,]*), '(?P<msg>.*)'$")
        .expect("valid regex")
});

/// Entry into an INSTANCED zone — the game suffixes these with "(Refined)",
/// which plain zone entries never carry:
///
///   You have entered The City of Guk 4 (Refined).
///   You have entered Nagafen's Lair - Group 4 (Refined).
///   You have entered Nagafen's Lair.                    <- NOT an instance
///
/// The capture is the full instance name; strip_instance_label tidies it for
/// display. Creation/invite chatter ("has asked you to join the instance:")
/// deliberately does not match — those fire before the player is actually
/// inside, and sometimes before the attempt even succeeds.
pub static RE_INSTANCE_ENTER: Lazy<Regex> = Lazy::new(|| {
    // `<zone>[ - Group] <tier digit> (<Tier name>).` — the tier NAME is an
    // open set (Awakened/Adaptive/Fused/Refined observed, one per tier
    // digit), so match the shape, not a word list: anchoring on the literal
    // "(Refined)" made every other difficulty invisible, discovered when a
    // tier-2 Plane of Hate produced no segment.
    Regex::new(r"^You have entered (.+ \d+ \([A-Za-z ]+\))\.\s*$").expect("valid regex")
});

/// Display label for an instance: the zone text without the "(Refined)"
/// marker the game appends to every instanced entry.
pub fn instance_label(zone: &str) -> String {
    match zone.rfind(" (") {
        Some(i) if zone.ends_with(')') => zone[..i].to_string(),
        _ => zone.to_string(),
    }
}

/// The log owner creating an instance — the only line that carries the
/// instance NUMBER, and the only signal at all for zones whose entry line
/// has no tier suffix (Plane of Sky enters as plain "You have entered The
/// Plane of Sky."). Captures (creator, zone, number).
pub static RE_INSTANCE_CREATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^Player ([A-Za-z]+) creating instance (.+?) (\d+)\.\s*$").expect("valid regex")
});

/// Any zone entry at all, for confirming a pending instance creation.
pub static RE_ZONE_ANY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^You have entered (.+?)\.\s*$").expect("valid regex"));

/// A chat message that marks a raid boundary: `(kind, label)`, or None.
///
/// Accepts an optional description after a separator, so a night of raiding
/// reads as itself rather than as "Raid, Raid, Raid":
///
///   raid start                 -> ("raid_start", "")
///   raid start -- Vox D3       -> ("raid_start", "Vox D3")
///   raid start: Nagafen        -> ("raid_start", "Nagafen")
///   raid end -- wiped          -> ("raid_end", "wiped")
///
/// A separator is REQUIRED before a description. Without that rule the
/// phrase would only have to be a prefix, and "raid start now?" — or any of
/// the forty-odd messages in one real log that merely mention a raid — would
/// start marking up the timeline.
pub fn raid_mark(message: &str) -> Option<(&'static str, String)> {
    let m = message.trim();
    let lower = m.to_ascii_lowercase();
    for (phrase, kind) in [
        ("raid start", "raid_start"),
        ("raidstart", "raid_start"),
        ("raid end", "raid_end"),
        ("raidend", "raid_end"),
    ] {
        let Some(rest) = lower.strip_prefix(phrase) else {
            continue;
        };
        let rest_raw = &m[phrase.len()..];
        if rest.trim().is_empty() {
            return Some((kind, String::new()));
        }
        let trimmed = rest_raw.trim_start();
        // A RUN of separator characters, not a fixed one: people type
        // "-", "--" and "---" interchangeably (all three appear in one real
        // log), and matching a fixed "--" left the third dash stuck to the
        // front of the description.
        let is_sep = |c: char| c == '-' || c == '\u{2014}' || c == ':';
        if trimmed.starts_with(is_sep) {
            let label = trimmed.trim_start_matches(is_sep).trim();
            return Some((kind, label.to_string()));
        }
        return None; // trailing words with no separator: not a marker
    }
    None
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    // ── norm ────────────────────────────────────────────────────────────────────
    #[test]
    fn heartbeat_matches_player_state_lines() {
        for line in [
            "You are stunned!",
            "You are no longer stunned.",
            "You can't cast spells while stunned!",
            "You regain your concentration and continue your casting.",
            "You feel your life force drain away.",
            "Insufficient Mana to cast this spell!",
            "Your Healing spell is interrupted.",
        ] {
            assert!(RE_HEARTBEAT.is_match(line), "should match: {line}");
        }
        assert!(!RE_HEARTBEAT.is_match("You are not currently assigned to an adventure."));
        assert!(!RE_HEARTBEAT.is_match("Your wounds begin to heal."));
    }

    #[test]
    fn norm_you_returns_player() {
        assert_eq!(norm("you", "Rysk"), "Rysk");
    }
    #[test]
    fn norm_you_case_insensitive() {
        assert_eq!(norm("YOU", "Rysk"), "Rysk");
    }
    #[test]
    fn norm_you_empty_player_passthrough() {
        assert_eq!(norm("you", ""), "you");
    }
    #[test]
    fn norm_non_you_passthrough() {
        assert_eq!(norm("Goblin", "Rysk"), "Goblin");
    }
    #[test]
    fn norm_article_a_via_norm() {
        assert_eq!(norm("A Goblin", "Rysk"), "a Goblin");
    }

    // ── normalize_article_case ──────────────────────────────────────────────────
    #[test]
    fn article_a() {
        assert_eq!(normalize_article_case("A Goblin"), "a Goblin");
    }
    #[test]
    fn article_an() {
        assert_eq!(normalize_article_case("An Orc"), "an Orc");
    }
    #[test]
    fn article_no_prefix() {
        assert_eq!(normalize_article_case("Goblin"), "Goblin");
    }
    #[test]
    fn article_already_lower() {
        assert_eq!(normalize_article_case("a goblin"), "a goblin");
    }
    #[test]
    fn article_not_a_prefix() {
        assert_eq!(normalize_article_case("Aardvark"), "Aardvark");
    }

    // ── normalize_verb ──────────────────────────────────────────────────────────
    #[test]
    fn verb_hit() {
        assert_eq!(normalize_verb("hit"), "hit");
    }
    #[test]
    fn verb_hits() {
        assert_eq!(normalize_verb("hits"), "hit");
    }
    #[test]
    fn verb_slash() {
        assert_eq!(normalize_verb("slash"), "slash");
    }
    #[test]
    fn verb_slashes() {
        assert_eq!(normalize_verb("slashes"), "slash");
    }
    #[test]
    fn verb_backstab() {
        assert_eq!(normalize_verb("backstab"), "backstab");
    }
    #[test]
    fn verb_backstabs() {
        assert_eq!(normalize_verb("backstabs"), "backstab");
    }
    #[test]
    fn verb_bash() {
        assert_eq!(normalize_verb("bash"), "bash");
    }
    #[test]
    fn verb_bashes() {
        assert_eq!(normalize_verb("bashes"), "bash");
    }
    #[test]
    fn verb_kick() {
        assert_eq!(normalize_verb("kick"), "kick");
    }
    #[test]
    fn verb_kicks() {
        assert_eq!(normalize_verb("kicks"), "kick");
    }
    #[test]
    fn verb_crush() {
        assert_eq!(normalize_verb("crush"), "crush");
    }
    #[test]
    fn verb_crushes() {
        assert_eq!(normalize_verb("crushes"), "crush");
    }
    #[test]
    fn verb_pierce() {
        assert_eq!(normalize_verb("pierce"), "pierce");
    }
    #[test]
    fn verb_pierces() {
        assert_eq!(normalize_verb("pierces"), "pierce");
    }
    #[test]
    fn verb_punch() {
        assert_eq!(normalize_verb("punch"), "punch");
    }
    #[test]
    fn verb_punches() {
        assert_eq!(normalize_verb("punches"), "punch");
    }
    #[test]
    fn verb_frenzy() {
        assert_eq!(normalize_verb("frenzy"), "frenzy");
    }
    #[test]
    fn verb_frenzies() {
        assert_eq!(normalize_verb("frenzies"), "frenzy");
    }
    #[test]
    fn verb_strike() {
        assert_eq!(normalize_verb("strike"), "strike");
    }
    #[test]
    fn verb_strikes() {
        assert_eq!(normalize_verb("strikes"), "strike");
    }
    #[test]
    fn verb_slay() {
        assert_eq!(normalize_verb("slay"), "slay");
    }
    #[test]
    fn verb_slays() {
        assert_eq!(normalize_verb("slays"), "slay");
    }
    #[test]
    fn verb_maul() {
        assert_eq!(normalize_verb("maul"), "maul");
    }
    #[test]
    fn verb_mauls() {
        assert_eq!(normalize_verb("mauls"), "maul");
    }
    #[test]
    fn verb_bite() {
        assert_eq!(normalize_verb("bite"), "bite");
    }
    #[test]
    fn verb_bites() {
        assert_eq!(normalize_verb("bites"), "bite");
    }
    #[test]
    fn verb_claw() {
        assert_eq!(normalize_verb("claw"), "claw");
    }
    #[test]
    fn verb_claws() {
        assert_eq!(normalize_verb("claws"), "claw");
    }
    #[test]
    fn verb_sting() {
        assert_eq!(normalize_verb("sting"), "sting");
    }
    #[test]
    fn verb_stings() {
        assert_eq!(normalize_verb("stings"), "sting");
    }
    #[test]
    fn verb_rend() {
        assert_eq!(normalize_verb("rend"), "rend");
    }
    #[test]
    fn verb_rends() {
        assert_eq!(normalize_verb("rends"), "rend");
    }
    #[test]
    fn verb_scratch() {
        assert_eq!(normalize_verb("scratch"), "scratch");
    }
    #[test]
    fn verb_scratches() {
        assert_eq!(normalize_verb("scratches"), "scratch");
    }
    #[test]
    fn verb_gore() {
        assert_eq!(normalize_verb("gore"), "gore");
    }
    #[test]
    fn verb_gores() {
        assert_eq!(normalize_verb("gores"), "gore");
    }
    #[test]
    fn verb_cleave() {
        assert_eq!(normalize_verb("cleave"), "cleave");
    }
    #[test]
    fn verb_cleaves() {
        assert_eq!(normalize_verb("cleaves"), "cleave");
    }
    #[test]
    fn verb_smash() {
        assert_eq!(normalize_verb("smash"), "smash");
    }
    #[test]
    fn verb_smashes() {
        assert_eq!(normalize_verb("smashes"), "smash");
    }
    #[test]
    fn verb_shoot() {
        assert_eq!(normalize_verb("shoot"), "shoot");
    }
    #[test]
    fn verb_shoots() {
        assert_eq!(normalize_verb("shoots"), "shoot");
    }
    #[test]
    fn verb_slam() {
        assert_eq!(normalize_verb("slam"), "slam");
    }
    #[test]
    fn verb_slams() {
        assert_eq!(normalize_verb("slams"), "slam");
    }
    #[test]
    fn verb_slice() {
        assert_eq!(normalize_verb("slice"), "slice");
    }
    #[test]
    fn verb_slices() {
        assert_eq!(normalize_verb("slices"), "slice");
    }
    #[test]
    fn verb_stab() {
        assert_eq!(normalize_verb("stab"), "stab");
    }
    #[test]
    fn verb_stabs() {
        assert_eq!(normalize_verb("stabs"), "stab");
    }
    #[test]
    fn verb_sweep() {
        assert_eq!(normalize_verb("sweep"), "sweep");
    }
    #[test]
    fn verb_sweeps() {
        assert_eq!(normalize_verb("sweeps"), "sweep");
    }
    #[test]
    fn verb_unknown_fallback() {
        assert_eq!(normalize_verb("unknown"), "hit");
    }

    // ── class_name_to_code ──────────────────────────────────────────────────────
    #[test]
    fn class_warrior() {
        assert_eq!(class_name_to_code("warrior"), Some("WAR"));
    }
    #[test]
    fn class_war_code() {
        assert_eq!(class_name_to_code("war"), Some("WAR"));
    }
    #[test]
    fn class_cleric() {
        assert_eq!(class_name_to_code("cleric"), Some("CLR"));
    }
    #[test]
    fn class_paladin() {
        assert_eq!(class_name_to_code("paladin"), Some("PAL"));
    }
    #[test]
    fn class_ranger() {
        assert_eq!(class_name_to_code("ranger"), Some("RNG"));
    }
    #[test]
    fn class_shadow_knight() {
        assert_eq!(class_name_to_code("shadow knight"), Some("SHD"));
    }
    #[test]
    fn class_shadowknight() {
        assert_eq!(class_name_to_code("shadowknight"), Some("SHD"));
    }
    #[test]
    fn class_shd_code() {
        assert_eq!(class_name_to_code("shd"), Some("SHD"));
    }
    #[test]
    fn class_druid() {
        assert_eq!(class_name_to_code("druid"), Some("DRU"));
    }
    #[test]
    fn class_monk() {
        assert_eq!(class_name_to_code("monk"), Some("MNK"));
    }
    #[test]
    fn class_bard() {
        assert_eq!(class_name_to_code("bard"), Some("BRD"));
    }
    #[test]
    fn class_rogue() {
        assert_eq!(class_name_to_code("rogue"), Some("ROG"));
    }
    #[test]
    fn class_shaman() {
        assert_eq!(class_name_to_code("shaman"), Some("SHM"));
    }
    #[test]
    fn class_necromancer() {
        assert_eq!(class_name_to_code("necromancer"), Some("NEC"));
    }
    #[test]
    fn class_wizard() {
        assert_eq!(class_name_to_code("wizard"), Some("WIZ"));
    }
    #[test]
    fn class_magician() {
        assert_eq!(class_name_to_code("magician"), Some("MAG"));
    }
    #[test]
    fn class_enchanter() {
        assert_eq!(class_name_to_code("enchanter"), Some("ENC"));
    }
    #[test]
    fn class_beastlord() {
        assert_eq!(class_name_to_code("beastlord"), Some("BST"));
    }
    #[test]
    fn class_berserker() {
        assert_eq!(class_name_to_code("berserker"), Some("BER"));
    }
    #[test]
    fn class_case_insensitive() {
        assert_eq!(class_name_to_code("WARRIOR"), Some("WAR"));
    }
    #[test]
    fn class_unknown() {
        assert_eq!(class_name_to_code("unknown"), None);
    }
    #[test]
    fn class_empty() {
        assert_eq!(class_name_to_code(""), None);
    }

    // ── normalize_miss ──────────────────────────────────────────────────────────
    #[test]
    fn miss_dodge() {
        assert_eq!(normalize_miss("dodge"), "dodge");
    }
    #[test]
    fn miss_dodges() {
        assert_eq!(normalize_miss("dodges"), "dodge");
    }
    #[test]
    fn miss_dodged() {
        assert_eq!(normalize_miss("dodged"), "dodge");
    }
    #[test]
    fn miss_parry() {
        assert_eq!(normalize_miss("parry"), "parry");
    }
    #[test]
    fn miss_parried() {
        assert_eq!(normalize_miss("parried"), "parry");
    }
    #[test]
    fn miss_parries() {
        assert_eq!(normalize_miss("parries"), "parry");
    }
    #[test]
    fn miss_miss() {
        assert_eq!(normalize_miss("miss"), "miss");
    }
    #[test]
    fn miss_missed() {
        assert_eq!(normalize_miss("missed"), "miss");
    }
    #[test]
    fn miss_misses() {
        assert_eq!(normalize_miss("misses"), "miss");
    }
    #[test]
    fn miss_block() {
        assert_eq!(normalize_miss("block"), "block");
    }
    #[test]
    fn miss_blocked() {
        assert_eq!(normalize_miss("blocked"), "block");
    }
    #[test]
    fn miss_blocks() {
        assert_eq!(normalize_miss("blocks"), "block");
    }
    #[test]
    fn miss_riposte() {
        assert_eq!(normalize_miss("riposte"), "riposte");
    }
    #[test]
    fn miss_riposted() {
        assert_eq!(normalize_miss("riposted"), "riposte");
    }
    #[test]
    fn miss_ripostes() {
        assert_eq!(normalize_miss("ripostes"), "riposte");
    }
    #[test]
    fn miss_invulnerable() {
        assert_eq!(normalize_miss("INVULNERABLE"), "invulnerable");
    }
    #[test]
    fn miss_absorb() {
        assert_eq!(normalize_miss("absorb"), "absorb");
    }
    #[test]
    fn miss_absorbs() {
        assert_eq!(normalize_miss("absorbs"), "absorb");
    }
    #[test]
    fn miss_unknown() {
        assert_eq!(normalize_miss("xyzzy"), "miss");
    }
}

#[cfg(test)]
mod regex_tests {
    use super::*;

    // ── RE_MELEE ─────────────────────────────────────────────────────────────────
    #[test]
    fn re_melee_slash() {
        let caps = RE_MELEE
            .captures("Rysk slashes a goblin for 150 points of damage.")
            .unwrap();
        assert_eq!(caps["src"].trim(), "Rysk");
        assert_eq!(&caps["verb"], "slashes");
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(&caps["dmg"], "150");
    }
    #[test]
    fn re_melee_kick() {
        let caps = RE_MELEE
            .captures("Talodar kicks a skeleton for 75 points of damage.")
            .unwrap();
        assert_eq!(&caps["verb"], "kicks");
    }
    #[test]
    fn re_melee_backstab() {
        let caps = RE_MELEE
            .captures("Rysk backstabs a guard for 2500 points of damage.")
            .unwrap();
        assert_eq!(&caps["verb"], "backstabs");
        assert_eq!(&caps["dmg"], "2500");
    }
    #[test]
    fn re_melee_does_match_spell_line_but_parser_checks_hit_by_spell_first() {
        // RE_MELEE CAN match this line (via the "hit" verb), but the parser checks
        // RE_HIT_BY_SPELL before RE_MELEE to handle "hit … by Spell" correctly.
        assert!(RE_MELEE
            .captures("Rysk hit a goblin for 500 points of magic damage by Fireball.")
            .is_some());
    }
    #[test]
    fn re_melee_on_prefix() {
        // Some mobs use "hits on" syntax
        let caps = RE_MELEE.captures("a skeleton hits on Rysk for 50 points of damage.");
        assert!(caps.is_some());
    }

    // ── RE_HIT_BY_SPELL ──────────────────────────────────────────────────────────
    #[test]
    fn re_hit_by_spell_basic() {
        let caps = RE_HIT_BY_SPELL
            .captures("Rysk hit a goblin for 500 points of magic damage by Fireball.")
            .unwrap();
        assert_eq!(caps["src"].trim(), "Rysk");
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(&caps["dmg"], "500");
        assert_eq!(&caps["spell"], "Fireball");
    }
    #[test]
    fn re_hit_by_spell_multi_word() {
        let caps = RE_HIT_BY_SPELL
            .captures("Rysk hit the Arch Lich for 1200 points of fire damage by Rend the Void.")
            .unwrap();
        assert_eq!(&caps["spell"], "Rend the Void");
    }

    // ── RE_SPELL_ATTR ─────────────────────────────────────────────────────────────
    #[test]
    fn re_spell_attr_basic() {
        let caps = RE_SPELL_ATTR
            .captures("Rysk's Fireball hit a goblin for 300 points of damage.")
            .unwrap();
        assert_eq!(&caps["src"], "Rysk");
        assert_eq!(&caps["spell"], "Fireball");
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(&caps["dmg"], "300");
    }

    // ── RE_DOT ───────────────────────────────────────────────────────────────────
    #[test]
    fn re_dot_basic() {
        let caps = RE_DOT
            .captures("a goblin has been damaged by Rysk's Envenomed Bolt for 150 damage.")
            .unwrap();
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(&caps["src"], "Rysk");
        assert_eq!(&caps["spell"], "Envenomed Bolt");
        assert_eq!(&caps["dmg"], "150");
    }

    // ── RE_RIPOSTE ────────────────────────────────────────────────────────────────
    #[test]
    fn re_riposte_basic() {
        let caps = RE_RIPOSTE
            .captures("a skeleton was injured by Rysk's riposte for 200 damage.")
            .unwrap();
        assert_eq!(&caps["tgt"], "a skeleton");
        assert_eq!(&caps["src"], "Rysk");
        assert_eq!(&caps["dmg"], "200");
    }

    // ── RE_DS ────────────────────────────────────────────────────────────────────
    #[test]
    fn re_ds_basic() {
        let caps = RE_DS
            .captures("a goblin was struck by Rysk's damage shield for 40 damage.")
            .unwrap();
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(&caps["src"], "Rysk");
        assert_eq!(&caps["dmg"], "40");
    }

    // ── RE_DS_PROC ───────────────────────────────────────────────────────────────
    #[test]
    fn re_ds_proc_your() {
        let caps = RE_DS_PROC
            .captures("a goblin is burned by YOUR flames for 20 points of non-melee damage.")
            .unwrap();
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(&caps["dmg"], "20");
        assert!(caps.name("src").is_none());
    }
    #[test]
    fn re_ds_proc_named() {
        let caps = RE_DS_PROC
            .captures("a goblin is burned by Rysk's flames for 20 points of non-melee damage.")
            .unwrap();
        assert_eq!(&caps["src"], "Rysk");
    }

    // ── RE_CAST ──────────────────────────────────────────────────────────────────
    #[test]
    fn re_cast_basic() {
        let caps = RE_CAST.captures("Rysk begins casting Fireball.").unwrap();
        assert_eq!(caps["src"].trim(), "Rysk");
        assert_eq!(&caps["spell"], "Fireball");
    }

    // ── RE_HEAL ──────────────────────────────────────────────────────────────────
    #[test]
    fn re_heal_with_spell() {
        let caps = RE_HEAL
            .captures("Healer healed Rysk for 1500 hit points by Complete Heal.")
            .unwrap();
        assert_eq!(&caps["src"], "Healer");
        assert_eq!(caps["tgt"].trim(), "Rysk");
        assert_eq!(&caps["amt"], "1500");
        assert_eq!(caps["spell"].trim_end_matches('.'), "Complete Heal");
    }
    #[test]
    fn re_heal_no_spell() {
        let caps = RE_HEAL
            .captures("Healer healed Rysk for 500 hit points.")
            .unwrap();
        assert_eq!(&caps["amt"], "500");
        assert!(caps.name("spell").is_none());
    }

    // ── RE_HAS_TAKEN ─────────────────────────────────────────────────────────────
    #[test]
    fn re_has_taken_your() {
        let caps = RE_HAS_TAKEN
            .captures("a goblin has taken 300 damage from your Shadowbolt.")
            .unwrap();
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(&caps["dmg"], "300");
        assert!(caps.name("your").is_some());
    }
    #[test]
    fn re_has_taken_src() {
        let caps = RE_HAS_TAKEN
            .captures("a goblin has taken 300 damage from Rysk's Shadowbolt.")
            .unwrap();
        assert_eq!(&caps["src"], "Rysk");
        assert_eq!(&caps["spell"], "Shadowbolt");
    }

    // ── RE_EXTRA_DMG ─────────────────────────────────────────────────────────────
    #[test]
    fn re_extra_dmg_your() {
        let caps = RE_EXTRA_DMG
            .captures(
                "a goblin has taken an extra 50 points of non-melee damage from your Bane spell.",
            )
            .unwrap();
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(&caps["dmg"], "50");
        assert_eq!(&caps["spell"], "Bane");
        assert!(caps.name("your").is_some());
    }

    // ── RE_SLAY_HAS ──────────────────────────────────────────────────────────────
    #[test]
    fn re_slay_has_basic() {
        let caps = RE_SLAY_HAS.captures("Rysk has slain a goblin!").unwrap();
        assert_eq!(caps["killer"].trim(), "Rysk");
        assert_eq!(&caps["tgt"], "a goblin");
    }

    // ── RE_SLAY_YOU ──────────────────────────────────────────────────────────────
    #[test]
    fn re_slay_you_basic() {
        let caps = RE_SLAY_YOU.captures("You have slain a goblin!").unwrap();
        assert_eq!(&caps["tgt"], "a goblin");
    }

    // ── RE_SLAIN_BY ──────────────────────────────────────────────────────────────
    #[test]
    fn re_slain_by_was() {
        let caps = RE_SLAIN_BY.captures("a goblin was slain by Rysk!").unwrap();
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(caps["killer"].trim(), "Rysk");
    }
    #[test]
    fn re_slain_by_has_been() {
        let caps = RE_SLAIN_BY
            .captures("a skeleton has been slain by Talodar!")
            .unwrap();
        assert_eq!(&caps["tgt"], "a skeleton");
    }

    // ── RE_DIED ──────────────────────────────────────────────────────────────────
    #[test]
    fn re_died_basic() {
        let caps = RE_DIED.captures("a goblin died.").unwrap();
        assert_eq!(&caps["tgt"], "a goblin");
    }

    // ── RE_MISS ──────────────────────────────────────────────────────────────────
    #[test]
    fn re_miss_dodge() {
        let caps = RE_MISS
            .captures("a skeleton tries to slash Rysk, but Rysk dodges!")
            .unwrap();
        assert_eq!(caps["src"].trim(), "a skeleton");
        assert_eq!(caps["tgt"].trim(), "Rysk");
        assert_eq!(&caps["miss"], "dodges");
    }
    #[test]
    fn re_miss_parry() {
        let caps = RE_MISS
            .captures("a goblin tries to bash Rysk, but Rysk parries!")
            .unwrap();
        assert_eq!(&caps["miss"], "parries");
    }
    #[test]
    fn re_miss_riposte() {
        let caps = RE_MISS
            .captures("a goblin tries to hit Rysk, but Rysk ripostes!")
            .unwrap();
        assert_eq!(&caps["miss"], "ripostes");
    }
    #[test]
    fn re_miss_invulnerable() {
        let caps = RE_MISS
            .captures("a goblin tries to hit Rysk, but Rysk is INVULNERABLE!")
            .unwrap();
        assert_eq!(&caps["miss"], "INVULNERABLE");
    }

    // ── RE_ABSORB_SKIN ────────────────────────────────────────────────────────────
    #[test]
    fn re_absorb_skin_named() {
        let caps = RE_ABSORB_SKIN
            .captures("Rysk's magical skin absorbs the damage of a goblin's thorns.")
            .unwrap();
        assert_eq!(&caps["tgt"], "Rysk");
        assert_eq!(caps["src"].trim(), "a goblin");
    }
    #[test]
    fn re_absorb_skin_your() {
        let caps = RE_ABSORB_SKIN
            .captures("YOUR magical skin absorbs the damage of a goblin's thorns.")
            .unwrap();
        assert!(caps.name("tgt").is_none());
        assert_eq!(caps["src"].trim(), "a goblin");
    }

    // ── RE_ABSORB_RUNE ────────────────────────────────────────────────────────────
    #[test]
    fn re_absorb_rune_basic() {
        let caps = RE_ABSORB_RUNE
            .captures("Rysk has shielded itself from 200 points of damage.")
            .unwrap();
        assert_eq!(caps["tgt"].trim(), "Rysk");
    }

    // ── RE_RESIST ────────────────────────────────────────────────────────────────
    #[test]
    fn re_resist_your() {
        let caps = RE_RESIST
            .captures("a goblin resisted your Shadowbolt!")
            .unwrap();
        assert_eq!(&caps["tgt"], "a goblin");
        assert_eq!(&caps["spell"], "Shadowbolt");
        assert!(caps.name("src").is_none());
    }
    #[test]
    fn re_resist_named() {
        let caps = RE_RESIST
            .captures("a goblin resisted Rysk's Shadowbolt!")
            .unwrap();
        assert_eq!(&caps["src"], "Rysk");
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
        let caps = RE_WHO
            .captures("[50 Warrior Monk Rogue] Talodar (Barbarian) <Valor>")
            .unwrap();
        assert_eq!(&caps["name"], "Talodar");
        assert_eq!(
            parse_who_classes(&caps["classes"]),
            vec!["WAR", "MNK", "ROG"]
        );
    }
    #[test]
    fn re_who_shadow_knight() {
        let caps = RE_WHO
            .captures("[60 Shadow Knight Cleric] Darkbane (Dark Elf)")
            .unwrap();
        assert_eq!(&caps["name"], "Darkbane");
        assert_eq!(parse_who_classes(&caps["classes"]), vec!["SHD", "CLR"]);
    }
    #[test]
    fn re_who_slash_codes() {
        let caps = RE_WHO
            .captures(
                "[44 PAL/MNK/BER] Talodar (Wood Elf) <Stone> ZONE: The Greater Faydark (gfaydark)",
            )
            .unwrap();
        assert_eq!(&caps["name"], "Talodar");
        assert_eq!(
            parse_who_classes(&caps["classes"]),
            vec!["PAL", "MNK", "BER"]
        );
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

#[cfg(test)]
mod raid_mark_tests {
    use super::*;

    #[test]
    fn re_chat_matches_both_directions() {
        let cases = [
            ("You say to your guild, 'raid start'", "You", "raid start"),
            ("You say, 'raid start'", "You", "raid start"),
            ("You tell your party, 'raid start'", "You", "raid start"),
            ("You say to your raid, 'raid end'", "You", "raid end"),
            ("You shout, 'incoming!'", "You", "incoming!"),
            (
                "Kermitzalot tells the raid, 'raid start'",
                "Kermitzalot",
                "raid start",
            ),
            ("Zyro tells General:1, 'raid start'", "Zyro", "raid start"),
            (
                "Gnominated tells the guild, 'hello there'",
                "Gnominated",
                "hello there",
            ),
            ("Raimier says, 'hi'", "Raimier", "hi"),
            // "shouts," was missing from the old enumerated verb list — a
            // verb-agnostic regex must not repeat that gap.
            ("Raimier shouts, 'hi'", "Raimier", "hi"),
            ("Raimier says out of character, 'hi'", "Raimier", "hi"),
        ];
        for (line, want_speaker, want_msg) in cases {
            let caps = RE_CHAT
                .captures(line)
                .unwrap_or_else(|| panic!("no match: {line}"));
            assert_eq!(&caps["speaker"], want_speaker, "{line}");
            assert_eq!(&caps["msg"], want_msg, "{line}");
        }
    }

    #[test]
    fn re_chat_ignores_combat_lines() {
        for line in [
            "You slash a greater skeleton for 96 points of damage. (Critical)",
            "Welcome to EverQuest Legends!",
            "[45 CLR/SHD/BER] Zyro (Human) <Ancient Artifacts>",
        ] {
            assert!(RE_CHAT.captures(line).is_none(), "should not match: {line}");
        }
    }

    /// A shouted (or otherwise spoken) line that quotes combat-shaped text
    /// must still be recognized as chat, so parser.rs's chat guard swallows
    /// it before the combat regexes get a chance to misread it as a real
    /// kill/hit. This is the injection this whole pattern exists to block.
    #[test]
    fn re_chat_catches_spoofed_combat_text_in_any_channel() {
        for line in [
            "Rysk shouts, 'Rysk has slain a goblin!'",
            "Rysk says, 'Rysk has slain a goblin!'",
            "Rysk says out of character, 'Rysk has slain a goblin!'",
            "Rysk tells you, 'Rysk hits you for 500 points of damage.'",
        ] {
            let caps = RE_CHAT
                .captures(line)
                .unwrap_or_else(|| panic!("no match: {line}"));
            assert_eq!(&caps["speaker"], "Rysk", "{line}");
        }
    }

    /// The whole message must be the phrase: guild chat is full of people
    /// asking when the raid starts, and none of that should move a marker.
    #[test]
    fn raid_mark_needs_the_whole_message() {
        let kind = |m| raid_mark(m).map(|(k, _)| k);
        assert_eq!(kind("raid start"), Some("raid_start"));
        assert_eq!(kind("  Raid Start  "), Some("raid_start"));
        assert_eq!(kind("RAIDSTART"), Some("raid_start"));
        assert_eq!(kind("raid end"), Some("raid_end"));

        assert_eq!(kind("when does the raid start?"), None);
        assert_eq!(kind("raid starts in 10"), None);
        assert_eq!(kind("is the raid ending soon"), None);
        assert_eq!(kind(""), None);
    }

    /// A description after a separator names the raid.
    #[test]
    fn raid_mark_takes_a_description() {
        assert_eq!(
            raid_mark("raid start -- Vox D3"),
            Some(("raid_start", "Vox D3".to_string()))
        );
        assert_eq!(
            raid_mark("Raid Start: Nagafen"),
            Some(("raid_start", "Nagafen".to_string()))
        );
        assert_eq!(
            raid_mark("raid start - Plane of Fear"),
            Some(("raid_start", "Plane of Fear".to_string()))
        );
        assert_eq!(
            raid_mark("raid end -- wiped on trash"),
            Some(("raid_end", "wiped on trash".to_string()))
        );
    }

    /// Separator runs vary by typist. All of these appeared in one real log.
    #[test]
    fn any_run_of_separators_works() {
        for line in [
            "Raid Start - Naggy D2 ",
            "Raid Start -- Naggy D2",
            "Raid Start --- Naggy D2",
            "Raid Start: Naggy D2",
            "Raid Start :- Naggy D2",
        ] {
            assert_eq!(
                raid_mark(line),
                Some(("raid_start", "Naggy D2".to_string())),
                "{line}"
            );
        }
        // A bare phrase with trailing whitespace is still a plain marker.
        assert_eq!(
            raid_mark("Raid Start "),
            Some(("raid_start", String::new()))
        );
    }

    /// Without a separator the phrase would only need to be a PREFIX, which
    /// is how ordinary chat starts marking up the timeline.
    #[test]
    fn a_description_needs_a_separator() {
        assert_eq!(raid_mark("raid start now?"), None);
        assert_eq!(raid_mark("raid start in 5 minutes"), None);
        assert_eq!(raid_mark("raid ending soon folks"), None);
    }
}

#[cfg(test)]
mod hyphenated_name_tests {
    use super::*;

    /// Cazic-Thule was completely invisible: every name class matched
    /// letters, backticks, apostrophes and spaces — no hyphen — so an entire
    /// god fight produced no meter, no viewer and no stream. Lines here are
    /// verbatim from the log of the night it was noticed.
    #[test]
    fn a_hyphenated_mob_exists_in_every_direction() {
        let caps = RE_MELEE
            .captures("Zyro kicks Cazic-Thule for 48 points of damage.")
            .expect("player -> mob");
        assert_eq!(&caps["tgt"], "Cazic-Thule");

        let caps = RE_MELEE
            .captures("Cazic-Thule slashes Zyro for 654 points of damage.")
            .expect("mob -> player");
        assert_eq!(&caps["src"], "Cazic-Thule");

        let caps = RE_MELEE
            .captures("You slash Cazic-Thule for 62 points of damage.")
            .expect("you -> mob");
        assert_eq!(&caps["tgt"], "Cazic-Thule");

        let caps = RE_MISS
            .captures("You try to slash Cazic-Thule, but miss!")
            .expect("you miss mob");
        assert_eq!(&caps["tgt"], "Cazic-Thule");

        let caps = RE_MISS
            .captures("Cazic-Thule tries to slash Zyro, but Zyro parries!")
            .expect("mob misses player");
        assert_eq!(&caps["src"], "Cazic-Thule");

        let caps = RE_CAST
            .captures("Cazic-Thule begins casting Rain of Fear.")
            .expect("mob casts");
        assert_eq!(&caps["src"], "Cazic-Thule");

        let caps = RE_SLAIN_BY
            .captures("Cazic-Thule has been slain by Zyro!")
            .expect("mob dies");
        assert_eq!(&caps["tgt"], "Cazic-Thule");

        let caps = RE_SLAY_HAS
            .captures("Zyro has slain Cazic-Thule!")
            .expect("player slays mob");
        assert_eq!(&caps["tgt"], "Cazic-Thule");
    }
}

#[cfg(test)]
mod instance_tests {
    use super::*;

    /// Every instanced-entry shape from a real log matches; plain zones and
    /// the invite line (fires before you are inside) do not.
    #[test]
    fn instanced_entries_match_and_plain_zones_do_not() {
        for (line, want) in [
            (
                "You have entered The City of Guk 4 (Refined).",
                "The City of Guk 4 (Refined)",
            ),
            (
                "You have entered Befallen 4 (Refined).",
                "Befallen 4 (Refined)",
            ),
            (
                "You have entered Nagafen's Lair - Group 4 (Refined).",
                "Nagafen's Lair - Group 4 (Refined)",
            ),
            // The other three tiers of the same ladder — anchoring on
            // "(Refined)" alone made these invisible.
            (
                "You have entered The Plane of Hate 2 (Adaptive).",
                "The Plane of Hate 2 (Adaptive)",
            ),
            (
                "You have entered The Ruins of Old Guk 3 (Fused).",
                "The Ruins of Old Guk 3 (Fused)",
            ),
            (
                "You have entered The Ruins of Old Paineel 1 (Awakened).",
                "The Ruins of Old Paineel 1 (Awakened)",
            ),
        ] {
            let caps = RE_INSTANCE_ENTER
                .captures(line)
                .unwrap_or_else(|| panic!("no match: {line}"));
            assert_eq!(&caps[1], want);
        }
        for line in [
            "You have entered Nagafen's Lair.",
            "You have entered East Freeport.",
            "Raimier has asked you to join the instance: The Ruins of Old Paineel - Group 4 (Refined).",
        ] {
            assert!(RE_INSTANCE_ENTER.captures(line).is_none(), "{line}");
        }
    }

    #[test]
    fn labels_drop_the_refined_marker() {
        assert_eq!(
            instance_label("The City of Guk 4 (Refined)"),
            "The City of Guk 4"
        );
        assert_eq!(
            instance_label("The Plane of Hate 2 (Adaptive)"),
            "The Plane of Hate 2"
        );
        assert_eq!(
            instance_label("Nagafen's Lair - Group 4 (Refined)"),
            "Nagafen's Lair - Group 4"
        );
    }
}
