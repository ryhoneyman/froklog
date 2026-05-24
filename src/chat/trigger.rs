// Chat trigger events for the NPC personality engine.
//
// Extended from the loggen chatmod.rs version to cover the full taxonomy
// identified in corpus analysis: combat, loot, social, real-life, streaks,
// reactions to other characters, and the relationship-aware meta-layer.

// ── Primary trigger enum ──────────────────────────────────────────────────────

/// An event that may cause one or more characters to speak.
///
/// Owned-string variant to avoid lifetime complications in async/queued dispatch.
#[derive(Debug, Clone)]
pub enum ChatTrigger {
    // ── Combat ────────────────────────────────────────────────────────────────
    /// A mob was killed.
    MobKilled {
        mob: String,
        killer: String,
        /// True if the fight was clean (no deaths, no panic heals).
        clean: bool,
    },

    /// A player character died.
    PlayerDied { who: String, cause: String },

    /// A player is critically low on HP.
    PlayerNearDeath { who: String, hp_pct: u8 },

    /// A healer saved a low-HP player in time.
    NearDeathSave { healer: String, target: String },

    /// A notably large hit landed (spell or melee).
    BigHit {
        src: String,
        tgt: String,
        dmg: u32,
        ability: String,
    },

    /// A spell was resisted. `nth` = how many resists in a row for this mob.
    SpellResisted {
        caster: String,
        mob: String,
        spell: String,
        nth: u32,
    },

    /// Full group wipe.
    Wipe,

    /// N kills in a row without any deaths or panic.
    CleanStreak { kills: u32 },

    /// Tank is critically low.
    TankLowHp { player: String, hp_pct: u8 },

    /// Healer mana is dropping.
    HealerLowMana { player: String, mana_pct: u8 },

    /// A caster has gone fully OOM.
    CasterOom { player: String },

    /// Pull incoming — puller announcing.
    Incoming {
        count: u32,
        mob: String,
        puller: String,
    },

    /// An add appeared or broke from CC.
    Add { mob: String },

    /// A player has been hitting the same mob and missing repeatedly.
    RepeatMiss {
        player: String,
        mob: String,
        count: u32,
    },

    // ── Loot & progression ────────────────────────────────────────────────────
    /// Desirable loot dropped.
    GoodLoot { item: String, recipient: String },

    /// Disappointing loot dropped.
    BadLoot { item: String },

    /// A player leveled up.
    LevelUp { who: String, level: u8 },

    /// Currency dropped from a corpse.
    CurrencyDrop { amount_pp: u32 },

    // ── Real-life layer ───────────────────────────────────────────────────────
    /// A player is stepping away (food, bathroom, family, etc.).
    RlDeparture {
        who: String,
        kind: crate::chat::state::RlKind,
    },

    /// A player has returned from a real-life interruption.
    RlReturn {
        who: String,
        /// Summary of notable events while they were gone.
        missed: Vec<String>,
    },

    /// Player is at keyboard but partially distracted (typing slower, AFK-ish).
    RlDistraction { who: String },

    // ── Session meta ──────────────────────────────────────────────────────────
    /// A new player joined the group.
    PlayerJoined { who: String },

    /// A player left the group.
    PlayerLeft { who: String },

    /// Session has been running for a while — fatigue chat starts appearing.
    LongSession { hours: f32 },

    /// Nothing is happening — idle downtime.
    Idle,

    // ── Reactions to other chat ───────────────────────────────────────────────
    /// One character reacting to something another character just said.
    ReactTo {
        speaker: String,
        class: ReactionClass,
        text: String,
    },

    // ── Streaks & patterns ────────────────────────────────────────────────────
    /// N consecutive resists on the same mob.
    ResistStreak { mob: String, count: u32 },

    /// The same player has died N times this session.
    DeathStreak { who: String, count: u32 },
}

// ── Reaction class ────────────────────────────────────────────────────────────

/// What kind of social content the reactor is responding to.
/// Determines which phrase pools are searched for a reaction.
#[derive(Debug, Clone, PartialEq)]
pub enum ReactionClass {
    /// Something went wrong — complaints, frustration.
    Complaint,
    /// Something went well — excitement, celebration.
    Excitement,
    /// A question directed at the group.
    Question,
    /// An announcement (going AFK, be right back, coming online).
    Announcement,
    /// Teasing or joking aimed at the reactor.
    Teasing,
    /// A real-life story or update from another player.
    RlStory,
    /// A tactical callout that may need acknowledgment.
    TacticalCallout,
}

impl ChatTrigger {
    /// The chat topic category this trigger maps to, for cooldown tracking.
    pub fn topic(&self) -> Option<crate::chat::state::ChatTopic> {
        use crate::chat::state::ChatTopic;
        match self {
            ChatTrigger::MobKilled { .. } | ChatTrigger::CleanStreak { .. } => {
                Some(ChatTopic::Slay)
            }
            ChatTrigger::PlayerDied { .. } | ChatTrigger::Wipe => Some(ChatTopic::Death),
            ChatTrigger::PlayerNearDeath { .. } | ChatTrigger::TankLowHp { .. } => {
                Some(ChatTopic::NearDeath)
            }
            ChatTrigger::SpellResisted { .. } | ChatTrigger::ResistStreak { .. } => {
                Some(ChatTopic::Resist)
            }
            ChatTrigger::HealerLowMana { .. } | ChatTrigger::CasterOom { .. } => {
                Some(ChatTopic::LowMana)
            }
            ChatTrigger::GoodLoot { .. } | ChatTrigger::BadLoot { .. } => Some(ChatTopic::Loot),
            ChatTrigger::LevelUp { .. } => Some(ChatTopic::LevelUp),
            ChatTrigger::Incoming { .. } => Some(ChatTopic::Incoming),
            ChatTrigger::RlDeparture { .. } | ChatTrigger::RlReturn { .. } => {
                Some(ChatTopic::RealLife)
            }
            ChatTrigger::Idle => Some(ChatTopic::Banter),
            _ => None,
        }
    }

    /// Urgency level 0–3. High urgency bypasses some cooldowns and
    /// increases the chance that quiet characters will respond.
    pub fn urgency(&self) -> u8 {
        match self {
            ChatTrigger::Wipe => 3,
            ChatTrigger::PlayerDied { .. } | ChatTrigger::TankLowHp { .. } => 3,
            ChatTrigger::NearDeathSave { .. } | ChatTrigger::PlayerNearDeath { .. } => 2,
            ChatTrigger::ResistStreak { count, .. } if *count >= 3 => 2,
            ChatTrigger::DeathStreak { count, .. } if *count >= 2 => 2,
            ChatTrigger::LevelUp { .. } | ChatTrigger::GoodLoot { .. } => 2,
            ChatTrigger::MobKilled { clean: true, .. } => 1,
            ChatTrigger::Idle => 0,
            _ => 1,
        }
    }
}
