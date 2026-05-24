use rand::Rng;
use serde::Deserialize;
/// Phrase database for the NPC chat engine.
///
/// Extends the loggen ChatFile format with per-archetype and per-register
/// variants.  Lookup priority: archetype+register → archetype → generic.
/// Fragment pools ({$pool}) are expanded inline after slot filling.
///
/// The embedded defaults are a sparse seed — the corpus-enrichment pass
/// will populate denser, more authentic pools from real player data.
use std::collections::HashMap;

use super::personality::Archetype;
use super::state::EmotionalRegister;

// ── TOML schema ───────────────────────────────────────────────────────────────

/// An event template entry. `archetype` and `register` are optional —
/// omitting both makes the entry the generic fallback.
#[derive(Debug, Deserialize)]
pub struct EventEntry {
    pub kind: String,
    /// Archetype name matching the Debug repr of Archetype (e.g. "ChaoticNarrator")
    pub archetype: Option<String>,
    /// Register name matching EmotionalRegister Debug repr (e.g. "Elated")
    pub register: Option<String>,
    pub templates: Vec<String>,
}

/// Top-level phrasebook TOML file.
#[derive(Debug, Deserialize)]
pub struct PhrasebookFile {
    #[serde(default)]
    pub fragments: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub events: Vec<EventEntry>,
}

// ── Phrase database ───────────────────────────────────────────────────────────

/// Key for the internal template map.
/// (trigger_kind, archetype_debug_name, register_debug_name)
type PoolKey = (String, Option<String>, Option<String>);

pub struct PhraseDb {
    pools: HashMap<PoolKey, Vec<String>>,
    fragments: HashMap<String, Vec<String>>,
}

impl PhraseDb {
    /// Build from a loaded TOML file.
    pub fn from_file(file: PhrasebookFile) -> Self {
        let mut db = Self::empty();
        db.fragments = file.fragments;
        for entry in file.events {
            let key: PoolKey = (entry.kind, entry.archetype, entry.register);
            db.pools.insert(key, entry.templates);
        }
        db
    }

    /// Build with embedded defaults only — usable without a TOML file.
    pub fn with_defaults() -> Self {
        let mut db = Self::empty();
        db.load_defaults();
        db
    }

    fn empty() -> Self {
        Self {
            pools: HashMap::new(),
            fragments: HashMap::new(),
        }
    }

    // ── Lookup ────────────────────────────────────────────────────────────────

    /// Select a random template for the given trigger kind, archetype, and
    /// emotional register.  Falls through to less-specific pools automatically.
    pub fn select(
        &self,
        kind: &str,
        archetype: &Archetype,
        register: &EmotionalRegister,
        rng: &mut impl Rng,
    ) -> Option<String> {
        let arch_s = format!("{archetype:?}");
        let reg_s = format!("{register:?}");

        let templates = self
            // 1. Archetype + register specific
            .pool(kind, Some(&arch_s), Some(&reg_s))
            // 2. Archetype only
            .or_else(|| self.pool(kind, Some(&arch_s), None))
            // 3. Generic (no archetype, no register)
            .or_else(|| self.pool(kind, None, None))?;

        let raw = templates.choose(rng)?;
        Some(raw.clone())
    }

    /// Select a random template from the generic "Generic" pool (idle/banter).
    pub fn select_generic(&self, rng: &mut impl Rng) -> Option<String> {
        self.pool("Generic", None, None)
            .and_then(|v| v.choose(rng))
            .cloned()
    }

    fn pool(&self, kind: &str, arch: Option<&str>, reg: Option<&str>) -> Option<&Vec<String>> {
        let key: PoolKey = (
            kind.to_string(),
            arch.map(str::to_string),
            reg.map(str::to_string),
        );
        self.pools.get(&key).filter(|v| !v.is_empty())
    }

    // ── Slot filling & fragment expansion ────────────────────────────────────

    /// Fill named slots ({mob}, {player}, etc.) then expand fragment pools ({$pool}).
    pub fn render(
        &self,
        template: &str,
        slots: &HashMap<&str, &str>,
        rng: &mut impl Rng,
    ) -> String {
        let filled = self.fill_slots(template, slots);
        self.expand_fragments(&filled, rng)
    }

    fn fill_slots(&self, template: &str, slots: &HashMap<&str, &str>) -> String {
        let mut out = template.to_string();
        for (key, val) in slots {
            out = out.replace(&format!("{{{key}}}"), val);
        }
        out
    }

    pub fn expand_fragments(&self, template: &str, rng: &mut impl Rng) -> String {
        let mut result = String::with_capacity(template.len() + 16);
        let mut rest = template;
        while let Some(start) = rest.find("{$") {
            result.push_str(&rest[..start]);
            let tail = &rest[start + 2..];
            if let Some(end) = tail.find('}') {
                let key = &tail[..end];
                if let Some(pool) = self.fragments.get(key) {
                    if let Some(item) = pool.choose(rng) {
                        result.push_str(item);
                    }
                } else {
                    result.push_str(&rest[start..start + 2 + end + 1]);
                }
                rest = &tail[end + 1..];
            } else {
                result.push_str(rest);
                return result;
            }
        }
        result.push_str(rest);
        result
    }

    // ── Internal helpers for defaults ─────────────────────────────────────────

    fn add(&mut self, kind: &str, arch: Option<&str>, reg: Option<&str>, msgs: &[&str]) {
        let key: PoolKey = (
            kind.into(),
            arch.map(str::to_string),
            reg.map(str::to_string),
        );
        self.pools
            .entry(key)
            .or_default()
            .extend(msgs.iter().map(|s| s.to_string()));
    }

    fn add_frags(&mut self, pool: &str, items: &[&str]) {
        self.fragments
            .entry(pool.into())
            .or_default()
            .extend(items.iter().map(|s| s.to_string()));
    }

    // ── Embedded defaults ─────────────────────────────────────────────────────

    fn load_defaults(&mut self) {
        // Fragment pools
        self.add_frags(
            "wow",
            &[
                "lol", "omg", "haha", "wtf", "dude", "geez", "holy", "sheesh",
            ],
        );
        self.add_frags(
            "easy",
            &["ez", "easy", "clean", "smooth", "no sweat", "effortless"],
        );
        self.add_frags(
            "rough",
            &["oof", "rough", "rip", "yikes", "brutal", "ouch", "ugly"],
        );
        self.add_frags(
            "hype",
            &["let's go", "we're cooking", "no stopping us", "on a roll"],
        );
        self.add_frags(
            "miss_jab",
            &[
                "hitting with eyes closed",
                "forgot how to make contact",
                "swinging at ghosts",
                "might as well be afk",
                "can't buy a hit",
            ],
        );

        // ── MobKilled ─────────────────────────────────────────────────────────
        self.add(
            "MobKilled",
            None,
            None,
            &[
                "nice",
                "gg",
                "good fight",
                "{mob} down",
                "{$easy}",
                "another one",
                "next",
                "on to the next",
            ],
        );
        self.add(
            "MobKilled",
            Some("ChaoticNarrator"),
            None,
            &[
                "yeah buddy",
                "hell yeah",
                "got em",
                "nice kill",
                "told you we'd get it",
                "{mob} had no chance",
                "that's how you do it",
                "one down, {$hype}",
            ],
        );
        self.add(
            "MobKilled",
            Some("ChaoticNarrator"),
            Some("Elated"),
            &[
                "YEAH BUDDY",
                "lets go!!",
                "we are COOKING",
                "absolutely unstoppable right now",
                "hell yeah brother",
            ],
        );
        self.add(
            "MobKilled",
            Some("ChaoticNarrator"),
            Some("Frustrated"),
            &["finally", "took long enough", "ok", "bout time"],
        );
        self.add(
            "MobKilled",
            Some("QuietAnchor"),
            None,
            &["nice", "gg", "{mob} down", "good"],
        );
        self.add(
            "MobKilled",
            Some("RaidLeader"),
            None,
            &[
                "next target",
                "good, moving on",
                "looting and moving",
                "on to the next",
            ],
        );
        self.add(
            "MobKilled",
            Some("ReactiveObserver"),
            None,
            &["gg", "nice", "good one"],
        );

        // ── PlayerDied ────────────────────────────────────────────────────────
        self.add(
            "PlayerDied",
            None,
            None,
            &["rip {player}", "gg {player}", "f", "rip", "ouch"],
        );
        self.add(
            "PlayerDied",
            Some("ChaoticNarrator"),
            None,
            &[
                "rip {player} lol",
                "oh no",
                "f in chat for {player}",
                "we'll get em next time {player}",
                "{$rough} {player}",
            ],
        );
        self.add(
            "PlayerDied",
            Some("ChaoticNarrator"),
            Some("Frustrated"),
            &[
                "come on guys",
                "we gotta stop dying",
                "that's on me, my bad",
                "ugh",
                "okay we need to tighten up",
            ],
        );
        self.add(
            "PlayerDied",
            Some("QuietAnchor"),
            None,
            &["rip", "f", "gg {player}"],
        );

        // ── NearDeathSave ─────────────────────────────────────────────────────
        self.add(
            "NearDeathSave",
            None,
            None,
            &[
                "nice save {healer}",
                "good heal {healer}",
                "whew, close one",
                "{target} living on the edge",
                "{$wow}... {healer} with the save",
            ],
        );
        self.add(
            "NearDeathSave",
            Some("ChaoticNarrator"),
            None,
            &[
                "jesus {target}",
                "nearly lost you there {target}",
                "{healer} is carrying this group",
                "good eyes {healer}",
                "okay I saw my life flash before my eyes just then",
            ],
        );
        self.add(
            "NearDeathSave",
            Some("QuietAnchor"),
            None,
            &["nice", "good heal", "close"],
        );

        // ── PlayerNearDeath ───────────────────────────────────────────────────
        self.add(
            "PlayerNearDeath",
            None,
            None,
            &[
                "HEAL {player}!",
                "{player}!!",
                "hp {player}!",
                "watch {player}!",
            ],
        );
        self.add(
            "PlayerNearDeath",
            Some("ChaoticNarrator"),
            None,
            &[
                "HEAL {player}!",
                "oh no {player}",
                "{player} is going down!",
                "someone on {player}!",
                "{$rough} {player}!!",
            ],
        );

        // ── SpellResisted ─────────────────────────────────────────────────────
        self.add(
            "SpellResisted",
            None,
            None,
            &[
                "it resisted again??",
                "slow is not landing",
                "keep trying",
                "this mob is immune or something",
            ],
        );
        self.add(
            "SpellResisted",
            Some("ChaoticNarrator"),
            None,
            &[
                "oh come on",
                "are you kidding me",
                "every single time",
                "this mob said no",
                "we might just tank it",
            ],
        );
        self.add(
            "SpellResisted",
            Some("ChaoticNarrator"),
            Some("Frustrated"),
            &[
                "I am DONE with this resist mechanic",
                "forget it, just burn it down",
                "this is actually insane",
            ],
        );
        self.add(
            "SpellResisted",
            Some("QuietAnchor"),
            None,
            &["heh", "resist", "again"],
        );

        // ── ResistStreak ──────────────────────────────────────────────────────
        self.add(
            "ResistStreak",
            None,
            None,
            &[
                "that's {count} resists in a row",
                "the slow curse continues",
                "okay forget slow, just burn it",
            ],
        );
        self.add(
            "ResistStreak",
            Some("ChaoticNarrator"),
            None,
            &[
                "{count} resists?? what is this mob made of",
                "I've never seen anything this resistant",
                "okay we are just tanking this full speed",
            ],
        );

        // ── Wipe ──────────────────────────────────────────────────────────────
        self.add(
            "Wipe",
            None,
            None,
            &["gg", "rip", "that happened", "f", "well that sucked"],
        );
        self.add(
            "Wipe",
            Some("ChaoticNarrator"),
            None,
            &[
                "okay that was a disaster lol",
                "we got absolutely wrecked",
                "alright, regroup",
                "that sucked, let's go again",
                "I'm not even mad, that was impressive",
            ],
        );
        self.add(
            "Wipe",
            Some("ChaoticNarrator"),
            Some("Frustrated"),
            &[
                "I cannot believe that",
                "come on guys, we're better than this",
                "okay what went wrong there",
            ],
        );
        self.add("Wipe", Some("QuietAnchor"), None, &["rip", "gg", "heh"]);

        // ── CleanStreak ───────────────────────────────────────────────────────
        self.add(
            "CleanStreak",
            None,
            None,
            &["clean sweep", "nice work", "we're in the zone", "{$hype}"],
        );
        self.add(
            "CleanStreak",
            Some("ChaoticNarrator"),
            None,
            &[
                "{count} clean kills, yeah buddy",
                "group is firing on all cylinders",
                "I love when a session goes like this",
                "okay seriously great group tonight",
            ],
        );

        // ── Incoming ──────────────────────────────────────────────────────────
        self.add(
            "Incoming",
            None,
            None,
            &[
                "incoming",
                "inc",
                "incoming {count}",
                "on the way",
                "pulling {mob}",
                "got one, incoming",
            ],
        );
        self.add(
            "Incoming",
            Some("ChaoticNarrator"),
            None,
            &[
                "pulling now",
                "incoming {count}, get ready",
                "{mob} incoming",
                "pulling {mob}",
            ],
        );
        self.add(
            "Incoming",
            Some("QuietAnchor"),
            None,
            &["inc", "incoming", "pulling {count}", "got {mob}"],
        );
        self.add(
            "Incoming",
            Some("RaidLeader"),
            None,
            &[
                "incoming {count}",
                "on the way",
                "heads up {count} up",
                "pulling {mob}, hold positions",
            ],
        );

        // ── CasterOom ─────────────────────────────────────────────────────────
        self.add(
            "CasterOom",
            None,
            None,
            &[
                "oom",
                "med please",
                "oom, 1 sec",
                "need to sit",
                "running on fumes",
                "almost dry",
            ],
        );
        self.add(
            "CasterOom",
            Some("ChaoticNarrator"),
            None,
            &[
                "oom, hang on",
                "I have like zero mana left",
                "don't pull, I'm oom",
                "give me a sec to med",
                "if you pull right now I literally cannot help you",
            ],
        );
        self.add(
            "CasterOom",
            Some("QuietAnchor"),
            None,
            &["oom", "sec", "med"],
        );

        // ── GoodLoot ─────────────────────────────────────────────────────────
        self.add(
            "GoodLoot",
            None,
            None,
            &["nice drop", "ooh {item}", "good loot", "let's go"],
        );
        self.add(
            "GoodLoot",
            Some("ChaoticNarrator"),
            None,
            &[
                "oh hell yeah {item}",
                "that's what I'm talking about",
                "{item} let's go!!",
                "yeah buddy we're eating tonight",
                "finally some good loot",
            ],
        );
        self.add(
            "GoodLoot",
            Some("ChaoticNarrator"),
            Some("Elated"),
            &[
                "YEAH BUDDY {item}",
                "okay this is the run",
                "I called it, I called it",
                "best session ever",
            ],
        );
        self.add(
            "GoodLoot",
            Some("QuietAnchor"),
            None,
            &["nice", "ooh", "good drop"],
        );
        self.add(
            "GoodLoot",
            Some("RaidLeader"),
            None,
            &["{item}", "nice, who needs that", "good drop"],
        );

        // ── BadLoot ───────────────────────────────────────────────────────────
        self.add(
            "BadLoot",
            None,
            None,
            &["meh", "oof", "trash", "{item}... pass"],
        );
        self.add(
            "BadLoot",
            Some("ChaoticNarrator"),
            None,
            &[
                "lol what is this {item}",
                "garbage loot as usual",
                "the loot gods hate us",
                "okay moving on",
                "{item} why is this even in the game",
            ],
        );
        self.add(
            "BadLoot",
            Some("ChaoticNarrator"),
            Some("Frustrated"),
            &[
                "I'm done with this loot table",
                "we genuinely cannot catch a break",
                "whoever designed this loot table...",
            ],
        );
        self.add("BadLoot", Some("QuietAnchor"), None, &["meh", "heh", "gg"]);

        // ── LevelUp ───────────────────────────────────────────────────────────
        self.add(
            "LevelUp",
            None,
            None,
            &[
                "grats {who}!",
                "gz {who}",
                "level {level}!",
                "nice ding {who}",
            ],
        );
        self.add(
            "LevelUp",
            Some("ChaoticNarrator"),
            None,
            &[
                "yeah buddy {who} hit {level}!",
                "LETS GO {who}",
                "grats!! {level} is a good level",
                "oh shit {who} dinged",
                "{who} with the ding, yeah buddy",
            ],
        );
        self.add(
            "LevelUp",
            Some("QuietAnchor"),
            None,
            &["gz {who}", "grats", "nice"],
        );

        // ── RlDeparture ───────────────────────────────────────────────────────
        self.add("RlDeparture", None, None, &["brb", "sec", "one sec", "afk"]);
        self.add(
            "RlDeparture",
            Some("ChaoticNarrator"),
            None,
            &[
                "brb one sec",
                "gonna grab a snack real quick",
                "afk a couple mins",
                "brb {rl_reason}",
                "sec, {rl_reason}",
                "one sec, need to deal with something",
            ],
        );
        self.add(
            "RlDeparture",
            Some("QuietAnchor"),
            None,
            &["brb", "sec", "afk"],
        );

        // ── RlReturn ─────────────────────────────────────────────────────────
        self.add(
            "RlReturn",
            None,
            None,
            &["back", "ok back", "back, what I miss"],
        );
        self.add(
            "RlReturn",
            Some("ChaoticNarrator"),
            None,
            &[
                "ok back",
                "sorry, back",
                "back, what happened",
                "ok back, did we die?",
                "back. she's {rl_context} lol",
                "sorry about that, back now",
            ],
        );
        self.add(
            "RlReturn",
            Some("QuietAnchor"),
            None,
            &["back", "ok", "back."],
        );

        // ── RepeatMiss ────────────────────────────────────────────────────────
        self.add(
            "RepeatMiss",
            None,
            None,
            &[
                "{player} having a rough round",
                "{player} might need a new weapon",
                "lol {player}",
                "{$wow} {player}, {$miss_jab}",
            ],
        );
        self.add(
            "RepeatMiss",
            Some("ChaoticNarrator"),
            None,
            &[
                "did you equip a weapon {player}",
                "lol {player} can't buy a hit",
                "{player} and {mob} are just vibing at this point",
                "maybe try actually swinging {player}",
                "{player} {$miss_jab}",
                "{$wow} {player} is {$miss_jab}",
            ],
        );
        self.add(
            "RepeatMiss",
            Some("QuietAnchor"),
            None,
            &["lol {player}", "heh", "{player}..."],
        );

        // ── BigHit ────────────────────────────────────────────────────────────
        self.add(
            "BigHit",
            None,
            None,
            &[
                "nice {ability}!",
                "{src} with the big hit",
                "{src} cooking",
                "dayum {src}",
                "BOOM",
            ],
        );
        self.add(
            "BigHit",
            Some("ChaoticNarrator"),
            None,
            &[
                "{src} you absolute monster",
                "okay {src} calm down lol",
                "{src} said delete {tgt}",
                "I felt that from here",
                "yeah buddy {src}!!",
                "{src} is {$hype}",
            ],
        );
        self.add(
            "BigHit",
            Some("QuietAnchor"),
            None,
            &["nice", "{$wow} {src}", "damn"],
        );

        // ── Generic (idle/banter) ─────────────────────────────────────────────
        self.add(
            "Generic",
            None,
            None,
            &[
                "ready when you are",
                "nice",
                "lol",
                "how's everyone's health?",
                "good fight",
                "pull whenever",
                "eyes open",
                "this is a good camp",
                "okay med break",
                "one more",
                "this mob hits like a truck",
                "exp is looking good tonight",
                "we got this",
                "stay focused",
                "{$hype}",
                "I could do this all night",
                "solid group tonight",
                "good pace",
                "what time is it even",
                "I need sleep but one more pull",
            ],
        );
        self.add(
            "Generic",
            Some("ChaoticNarrator"),
            None,
            &[
                "yeah buddy",
                "this is the run",
                "I love a clean pull",
                "okay seriously having fun tonight",
                "lol classic",
                "best group I've been in",
                "I trust this group",
                "I feel unstoppable right now",
            ],
        );
        self.add(
            "Generic",
            Some("QuietAnchor"),
            None,
            &["heh", "nice", "yeah", "ready", "good"],
        );
        self.add(
            "Generic",
            Some("RaidLeader"),
            None,
            &[
                "good job everyone",
                "keep the pace",
                "nice work",
                "stay on target",
                "eyes open",
            ],
        );
        self.add(
            "Generic",
            Some("ChaoticNarrator"),
            Some("Elated"),
            &[
                "I am having the BEST time right now",
                "this is exactly why I play this game",
                "okay we are legitimately unstoppable",
                "every pull just goes, it's beautiful",
                "I want to screenshot this group",
            ],
        );
        self.add(
            "Generic",
            Some("ChaoticNarrator"),
            Some("Tired"),
            &[
                "okay I might be falling asleep lol",
                "one more then I really gotta sleep",
                "wife's gonna kill me, one more pull",
                "my eyes are literally closing",
                "I keep zoning out, sorry",
            ],
        );

        // ── ExtraMobCC ────────────────────────────────────────────────────────
        self.add(
            "ExtraMobCC",
            None,
            None,
            &[
                "CC on {mob}",
                "got the extra, don't touch it",
                "mez on {mob}, leave it",
                "rooted, focus main mob",
                "extra is CC'd",
            ],
        );
        self.add(
            "ExtraMobCC",
            Some("QuietAnchor"),
            None,
            &["got {mob}", "cc'd", "mez on {mob}", "rooted"],
        );
        self.add(
            "ExtraMobCC",
            Some("RaidLeader"),
            None,
            &["CC on {mob}", "hold {mob}, focus main", "extra is handled"],
        );

        // ── BeingHit ──────────────────────────────────────────────────────────
        self.add(
            "BeingHit",
            None,
            None,
            &[
                "add on me",
                "one on me, need help",
                "I've got one on me!",
                "{mob} is on me!",
                "someone get this off me",
                "off me please!",
                "someone peel this",
            ],
        );
        self.add(
            "BeingHit",
            Some("QuietAnchor"),
            None,
            &["add on me", "one on me, need help", "{mob} on me"],
        );

        // ── MultiKill ─────────────────────────────────────────────────────────
        self.add(
            "MultiKill",
            None,
            None,
            &[
                "all down",
                "clean sweep",
                "handled",
                "nice work on the pack",
                "entire pack cleared",
            ],
        );
        self.add(
            "MultiKill",
            Some("ChaoticNarrator"),
            None,
            &[
                "yeah buddy!! ALL of them",
                "we absolutely handled that pack",
                "clean sweep, that's what I'm talking about",
                "every. single. one.",
                "okay we do not wipe, I said what I said",
            ],
        );
        self.add(
            "MultiKill",
            Some("ChaoticNarrator"),
            Some("Elated"),
            &[
                "WE ARE UNTOUCHABLE",
                "nothing can stop us right now",
                "entire pack, zero effort, YEAH BUDDY",
            ],
        );
        self.add(
            "MultiKill",
            Some("QuietAnchor"),
            None,
            &["handled", "clean", "gg"],
        );
        self.add(
            "MultiKill",
            Some("RaidLeader"),
            None,
            &["good, moving on", "all down, next", "pack cleared"],
        );

        // ── TankDying ─────────────────────────────────────────────────────────
        self.add(
            "TankDying",
            None,
            None,
            &[
                "TANK!!",
                "healer wake up",
                "HEAL {player}!!",
                "uh oh",
                "{player}!!",
            ],
        );
        self.add(
            "TankDying",
            Some("ChaoticNarrator"),
            None,
            &[
                "TANK IS DYING",
                "oh no oh no oh no",
                "CLR NOW",
                "{player} is going down!!",
                "EMERGENCY HEALS {player}",
                "HEAL HIM HEAL HIM",
            ],
        );
        self.add(
            "TankDying",
            Some("QuietAnchor"),
            None,
            &["HEAL {player}", "tank", "CH {player}"],
        );
        self.add(
            "TankDying",
            Some("RaidLeader"),
            None,
            &[
                "CLR on {player} NOW",
                "healer on tank",
                "all heals on {player}",
            ],
        );

        // ── HealerMana ────────────────────────────────────────────────────────
        self.add(
            "HealerMana",
            None,
            None,
            &[
                "{pct}% mana",
                "at {pct}%",
                "low mana",
                "need to sit",
                "conserving mana",
            ],
        );
        self.add(
            "HealerMana",
            Some("ChaoticNarrator"),
            None,
            &[
                "oom at {pct}%, hang on",
                "I'm at {pct}%, need to conserve",
                "running dry, {pct}%",
                "almost out of mana lol",
                "don't pull, I'm at {pct}%",
            ],
        );
        self.add(
            "HealerMana",
            Some("QuietAnchor"),
            None,
            &["{pct}m", "oom", "{pct}%"],
        );
        self.add(
            "HealerMana",
            Some("RaidLeader"),
            None,
            &["{pct}% mana, slowing down", "healers conserve mana"],
        );
    }
}

// ── Trait helpers ─────────────────────────────────────────────────────────────

trait SliceChoose<T> {
    fn choose(&self, rng: &mut impl Rng) -> Option<&T>;
}

impl<T> SliceChoose<T> for Vec<T> {
    fn choose(&self, rng: &mut impl Rng) -> Option<&T> {
        if self.is_empty() {
            None
        } else {
            Some(&self[rng.gen_range(0..self.len())])
        }
    }
}
