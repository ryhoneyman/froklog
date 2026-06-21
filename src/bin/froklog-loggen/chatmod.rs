use std::collections::HashMap;

use rand::rngs::StdRng;
use rand::Rng;

#[cfg(feature = "neural")]
use froklog::chat::SituationContext;
use froklog::chat::{
    Archetype, ChatTopic, EmotionalRegister, ExpressionMode, PersonalityProfile, PhraseDb,
};

pub use froklog::chat::PersonalityProfile as Personality;

// ── Simulated-time character state ────────────────────────────────────────────

/// Tick-based analog of `CharacterState` for loggen's simulated time.
/// Uses `u32` seconds instead of `Instant`, so no wall-clock dependency.
#[derive(Clone)]
pub struct SimChatState {
    pub frustration: f32,
    pub morale: f32,
    pub fatigue: f32,
    pub engagement: f32,
    pub last_spoke_sec: Option<u32>,
    topic_last_spoke: HashMap<ChatTopic, u32>,
    silent_event_counts: HashMap<ChatTopic, u32>,
}

impl SimChatState {
    pub fn new() -> Self {
        Self {
            frustration: 0.0,
            morale: 0.5,
            fatigue: 0.0,
            engagement: 0.0,
            last_spoke_sec: None,
            topic_last_spoke: HashMap::new(),
            silent_event_counts: HashMap::new(),
        }
    }

    /// Decay emotional state by `elapsed_secs` of simulated time.
    pub fn decay(&mut self, elapsed_secs: f32) {
        self.frustration = (self.frustration - elapsed_secs / 300.0).max(0.0);
        self.morale += (0.5 - self.morale) * (elapsed_secs / 300.0);
        self.engagement = (self.engagement - elapsed_secs / 90.0).max(0.0);
        self.fatigue = (self.fatigue + elapsed_secs / 7_200.0).min(1.0);
    }

    pub fn on_slay(&mut self) {
        self.morale = (self.morale + 0.08).min(1.0);
        self.engagement = (self.engagement + 0.35).min(1.0);
        self.frustration = (self.frustration - 0.04).max(0.0);
        *self.silent_event_counts.entry(ChatTopic::Slay).or_insert(0) += 1;
    }

    #[allow(dead_code)]
    pub fn on_death(&mut self) {
        self.frustration = (self.frustration + 0.30).min(1.0);
        self.morale = (self.morale - 0.20).max(0.0);
        *self
            .silent_event_counts
            .entry(ChatTopic::Death)
            .or_insert(0) += 1;
    }

    pub fn on_resist(&mut self) {
        self.frustration = (self.frustration + 0.10).min(1.0);
        *self
            .silent_event_counts
            .entry(ChatTopic::Resist)
            .or_insert(0) += 1;
    }

    pub fn on_near_death(&mut self) {
        self.frustration = (self.frustration + 0.12).min(1.0);
        self.engagement = (self.engagement + 0.50).min(1.0);
    }

    pub fn on_good_loot(&mut self) {
        self.morale = (self.morale + 0.18).min(1.0);
        *self.silent_event_counts.entry(ChatTopic::Loot).or_insert(0) += 1;
    }

    pub fn on_bad_loot(&mut self) {
        self.frustration = (self.frustration + 0.04).min(1.0);
        *self.silent_event_counts.entry(ChatTopic::Loot).or_insert(0) += 1;
    }

    pub fn register(&self) -> EmotionalRegister {
        if self.frustration > 0.65 {
            EmotionalRegister::Frustrated
        } else if self.morale > 0.80 {
            EmotionalRegister::Elated
        } else if self.fatigue > 0.70 {
            EmotionalRegister::Tired
        } else if self.engagement > 0.55 {
            EmotionalRegister::Engaged
        } else {
            EmotionalRegister::Neutral
        }
    }

    pub fn topic_on_cooldown(&self, topic: &ChatTopic, cooldown_secs: u32, cur_sec: u32) -> bool {
        self.topic_last_spoke
            .get(topic)
            .map(|&last| cur_sec.saturating_sub(last) < cooldown_secs)
            .unwrap_or(false)
    }

    pub fn mark_topic_spoken(&mut self, topic: ChatTopic, cur_sec: u32) {
        self.topic_last_spoke.insert(topic.clone(), cur_sec);
        self.silent_event_counts.remove(&topic);
    }

    pub fn mark_spoke(&mut self, cur_sec: u32) {
        self.last_spoke_sec = Some(cur_sec);
    }

    pub fn silent_count(&self, topic: &ChatTopic) -> u32 {
        *self.silent_event_counts.get(topic).unwrap_or(&0)
    }
}

impl Default for SimChatState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Chat triggers ─────────────────────────────────────────────────────────────

pub enum ChatTrigger<'a> {
    BigSpellHit {
        caster: &'a str,
        spell: &'a str,
        mob: &'a str,
        damage: u32,
    },
    NearDeathSave {
        healer: &'a str,
        target: &'a str,
    },
    PlayerNearDeath {
        player: &'a str,
    },
    MobKilled {
        mob: &'a str,
    },
    CasterOom {
        player: &'a str,
    },
    RepeatMiss {
        player: &'a str,
        mob: &'a str,
    },
    SlowResisted,
    Incoming {
        count: u32,
        mob: &'a str,
    },
    HealerMana {
        pct: u32,
    },
    Generic,
    MultiKill,
    TankDying {
        player: &'a str,
    },
}

impl<'a> ChatTrigger<'a> {
    /// 0=low/idle, 1=normal, 2=notable, 3=urgent — scales speak probability.
    pub fn urgency(&self) -> u8 {
        match self {
            ChatTrigger::TankDying { .. } | ChatTrigger::PlayerNearDeath { .. } => 3,
            ChatTrigger::HealerMana { pct } if *pct <= 10 => 3,
            ChatTrigger::NearDeathSave { .. }
            | ChatTrigger::BigSpellHit { .. }
            | ChatTrigger::MultiKill => 2,
            ChatTrigger::HealerMana { pct } if *pct <= 25 => 2,
            ChatTrigger::MobKilled { .. }
            | ChatTrigger::CasterOom { .. }
            | ChatTrigger::SlowResisted
            | ChatTrigger::RepeatMiss { .. }
            | ChatTrigger::HealerMana { .. }
            | ChatTrigger::Incoming { .. } => 1,
            ChatTrigger::Generic => 0,
        }
    }

    /// Maps trigger to the closest `ChatTopic` for cooldown and pressure tracking.
    pub fn topic_hint(&self) -> Option<ChatTopic> {
        match self {
            ChatTrigger::MobKilled { .. } | ChatTrigger::MultiKill => Some(ChatTopic::Slay),
            ChatTrigger::PlayerNearDeath { .. }
            | ChatTrigger::TankDying { .. }
            | ChatTrigger::NearDeathSave { .. } => Some(ChatTopic::NearDeath),
            ChatTrigger::SlowResisted => Some(ChatTopic::Resist),
            ChatTrigger::HealerMana { .. } | ChatTrigger::CasterOom { .. } => {
                Some(ChatTopic::LowMana)
            }
            ChatTrigger::Incoming { .. } => Some(ChatTopic::Incoming),
            ChatTrigger::BigSpellHit { .. }
            | ChatTrigger::RepeatMiss { .. }
            | ChatTrigger::Generic => Some(ChatTopic::Banter),
        }
    }

    /// Tactical triggers don't suppress `Action`-expresser characters.
    pub fn is_tactical(&self) -> bool {
        matches!(
            self,
            ChatTrigger::Incoming { .. }
                | ChatTrigger::CasterOom { .. }
                | ChatTrigger::HealerMana { .. }
                | ChatTrigger::SlowResisted
                | ChatTrigger::TankDying { .. }
        )
    }

    /// Per-topic cooldown in simulated seconds.
    pub fn cooldown_secs(&self) -> u32 {
        match self {
            ChatTrigger::MobKilled { .. } | ChatTrigger::MultiKill => 45,
            ChatTrigger::BigSpellHit { .. }
            | ChatTrigger::SlowResisted
            | ChatTrigger::RepeatMiss { .. }
            | ChatTrigger::CasterOom { .. } => 30,
            ChatTrigger::HealerMana { .. } => 60,
            ChatTrigger::Incoming { .. } => 20,
            ChatTrigger::PlayerNearDeath { .. }
            | ChatTrigger::TankDying { .. }
            | ChatTrigger::NearDeathSave { .. } => 15,
            ChatTrigger::Generic => 90,
        }
    }
}

// ── Chat dispatcher ───────────────────────────────────────────────────────────

pub struct ChatCtx<'a> {
    phrases: &'a PhraseDb,
    chat_level: u8,
}

impl<'a> ChatCtx<'a> {
    pub fn new(phrases: &'a PhraseDb, chat_level: u8) -> Self {
        Self {
            phrases,
            chat_level,
        }
    }

    /// Try to produce a message for `trigger` given `personality` and `state`.
    /// Returns None if personality, cooldowns, or RNG suppress the response.
    ///
    /// The caller is responsible for calling `state.mark_spoke()` and
    /// `state.mark_topic_spoken()` when Some is returned.
    pub fn respond(
        &self,
        trigger: &ChatTrigger<'_>,
        personality: &PersonalityProfile,
        state: &SimChatState,
        cur_sec: u32,
        rng: &mut StdRng,
    ) -> Option<String> {
        if self.chat_level == 0 {
            return None;
        }

        // Cooldown gate (bypassed for urgent events)
        if trigger.urgency() < 3 {
            if let Some(ref t) = trigger.topic_hint() {
                if state.topic_on_cooldown(t, trigger.cooldown_secs(), cur_sec) {
                    tracing::debug!(
                        archetype = ?personality.archetype,
                        register  = ?state.register(),
                        urgency   = trigger.urgency(),
                        "chat suppressed: topic on cooldown"
                    );
                    return None;
                }
            }
        }

        // Speak probability
        let prob = self.speak_probability(trigger, personality, state);
        let roll: f32 = rng.gen();
        tracing::debug!(
            archetype   = ?personality.archetype,
            register    = ?state.register(),
            urgency     = trigger.urgency(),
            verbosity   = personality.verbosity,
            humor       = personality.humor,
            positivity  = personality.positivity,
            patience    = personality.patience,
            frustration = state.frustration,
            morale      = state.morale,
            fatigue     = state.fatigue,
            speak_prob  = prob,
            roll        = roll,
            passed      = roll < prob,
            "phrasebook speak-probability gate"
        );
        if roll >= prob {
            return None;
        }

        // Get raw message — PhraseDb first, TOML fallback
        let register = state.register();
        let raw = self.raw_message(trigger, &personality.archetype, &register, rng)?;
        tracing::debug!(
            archetype = ?personality.archetype,
            register  = ?register,
            raw       = %raw,
            "phrasebook template selected"
        );

        // Apply personality style transforms
        Some(apply_style(&raw, personality, rng))
    }

    pub(crate) fn speak_probability(
        &self,
        trigger: &ChatTrigger<'_>,
        personality: &PersonalityProfile,
        state: &SimChatState,
    ) -> f32 {
        let urgency_scale: f32 = match trigger.urgency() {
            3 => 2.20,
            2 => 1.40,
            1 => 1.00,
            _ => 0.45,
        };

        let silent_pressure: f32 = if let Some(ref t) = trigger.topic_hint() {
            let count = state.silent_count(t);
            1.0 + (count as f32 * 0.15).min(0.60)
        } else {
            1.0
        };

        let fatigue_scale = 1.0 - (state.fatigue * 0.30);

        // Action-expressers stay quiet on non-tactical social events
        let expression_scale: f32 =
            if personality.expression_mode == ExpressionMode::Action && !trigger.is_tactical() {
                0.12
            } else {
                1.0
            };

        let chat_scale: f32 = match self.chat_level {
            0 => 0.0,
            1 => 0.40,
            2 => 0.70,
            3 => 1.10,
            _ => 1.50,
        };

        (personality.verbosity
            * urgency_scale
            * silent_pressure
            * fatigue_scale
            * expression_scale
            * chat_scale)
            .min(0.95)
    }

    fn raw_message(
        &self,
        trigger: &ChatTrigger<'_>,
        archetype: &Archetype,
        register: &EmotionalRegister,
        rng: &mut StdRng,
    ) -> Option<String> {
        match trigger {
            ChatTrigger::BigSpellHit {
                caster,
                spell,
                mob,
                damage,
            } => {
                let dmg = damage.to_string();
                let mut slots = HashMap::new();
                slots.insert("caster", *caster);
                slots.insert("src", *caster);
                slots.insert("spell", *spell);
                slots.insert("ability", *spell);
                slots.insert("mob", *mob);
                slots.insert("tgt", *mob);
                slots.insert("damage", dmg.as_str());
                slots.insert("dmg", dmg.as_str());
                let t = self.phrases.select("BigHit", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
            ChatTrigger::NearDeathSave { healer, target } => {
                let mut slots = HashMap::new();
                slots.insert("healer", *healer);
                slots.insert("target", *target);
                let t = self
                    .phrases
                    .select("NearDeathSave", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
            ChatTrigger::PlayerNearDeath { player } => {
                let mut slots = HashMap::new();
                slots.insert("player", *player);
                let t = self
                    .phrases
                    .select("PlayerNearDeath", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
            ChatTrigger::MobKilled { mob } => {
                let mut slots = HashMap::new();
                slots.insert("mob", *mob);
                let t = self.phrases.select("MobKilled", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
            ChatTrigger::CasterOom { player } => {
                let mut slots = HashMap::new();
                slots.insert("player", *player);
                let t = self.phrases.select("CasterOom", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
            ChatTrigger::RepeatMiss { player, mob } => {
                let mut slots = HashMap::new();
                slots.insert("player", *player);
                slots.insert("mob", *mob);
                let t = self
                    .phrases
                    .select("RepeatMiss", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
            ChatTrigger::SlowResisted => {
                let slots = HashMap::new();
                let t = self
                    .phrases
                    .select("SpellResisted", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
            ChatTrigger::Incoming { count, mob } => {
                let count_s = count.to_string();
                let mut slots = HashMap::new();
                slots.insert("count", count_s.as_str());
                slots.insert("mob", *mob);
                let t = self.phrases.select("Incoming", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
            ChatTrigger::HealerMana { pct } => {
                let pct_s = pct.to_string();
                let mut slots = HashMap::new();
                slots.insert("pct", pct_s.as_str());
                let t = self
                    .phrases
                    .select("HealerMana", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
            ChatTrigger::Generic => {
                let empty: HashMap<&str, &str> = HashMap::new();
                let t = self.phrases.select("Generic", archetype, register, rng)?;
                Some(self.phrases.render(&t, &empty, rng))
            }
            ChatTrigger::MultiKill => {
                let empty: HashMap<&str, &str> = HashMap::new();
                let t = self.phrases.select("MultiKill", archetype, register, rng)?;
                Some(self.phrases.render(&t, &empty, rng))
            }
            ChatTrigger::TankDying { player } => {
                let mut slots = HashMap::new();
                slots.insert("player", *player);
                let t = self.phrases.select("TankDying", archetype, register, rng)?;
                Some(self.phrases.render(&t, &slots, rng))
            }
        }
    }

    /// Pick a phrase from a named pool, bypassing personality and state checks.
    /// Fragments ({$pool}) are expanded; named slots ({mob} etc.) are left for
    /// the caller to fill with `.replace()`.
    pub fn pick_template(&self, kind: &str, rng: &mut StdRng) -> Option<String> {
        let empty: HashMap<&str, &str> = HashMap::new();
        let t = self
            .phrases
            .select(kind, &Archetype::Custom, &EmotionalRegister::Neutral, rng)?;
        Some(self.phrases.render(&t, &empty, rng))
    }

    /// Public proxy for `raw_message`, used by `NeuralCtx` as phrasebook fallback.
    #[cfg(feature = "neural")]
    #[allow(dead_code)]
    pub fn raw_message_pub(
        &self,
        trigger: &ChatTrigger<'_>,
        archetype: &Archetype,
        register: &EmotionalRegister,
        rng: &mut StdRng,
    ) -> Option<String> {
        self.raw_message(trigger, archetype, register, rng)
    }
}

// ── Style transforms ──────────────────────────────────────────────────────────

fn apply_style(msg: &str, personality: &PersonalityProfile, rng: &mut StdRng) -> String {
    let mut out = msg.to_string();

    // Lowercase tendency
    if rng.gen::<f32>() < personality.style.lowercase_tendency {
        out = out.to_lowercase();
    }

    // Signature filler (low probability per message)
    if let Some(ref filler) = personality.style.signature_filler {
        if rng.gen::<f32>() < 0.12 {
            out = format!("{} {}", out, filler);
        }
    }

    // Fragment truncation for low-expressiveness, high-fragment characters
    if personality.expressiveness < 0.35
        && personality.style.fragment_tendency > 0.60
        && rng.gen::<f32>() < personality.style.fragment_tendency
    {
        let words: Vec<&str> = out.split_whitespace().collect();
        if words.len() > 3 {
            let take = (words.len() * 2 / 3).max(2);
            out = words[..take].join(" ");
        }
    }

    out
}

// ── Neural auto-detection ─────────────────────────────────────────────────────

/// Try to load a `NeuralBackend` from `model_dir` (if given) or the directory
/// that contains the running binary.  Returns `None` if the `neural` feature
/// is not active, or if any of the required files (`model.onnx`, `vocab.model`,
/// `model_meta.json`) are absent, or if loading fails (a warning is logged).
///
/// Callers should fall back to `ChatCtx` (phrasebook) when this returns `None`.
#[cfg(feature = "neural")]
#[allow(dead_code)]
pub fn try_load_neural_backend(
    model_dir: Option<&std::path::Path>,
) -> Option<froklog::chat::NeuralBackend> {
    use std::path::PathBuf;

    let dir: PathBuf = if let Some(d) = model_dir {
        d.to_path_buf()
    } else {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    };

    let required = ["model.onnx", "vocab.model", "model_meta.json"];
    let missing: Vec<&str> = required
        .iter()
        .filter(|f| !dir.join(f).exists())
        .copied()
        .collect();
    if !missing.is_empty() {
        tracing::warn!(
            "Neural backend unavailable — missing from {}: {} (using phrasebook)",
            dir.display(),
            missing.join(", ")
        );
        return None;
    }

    match froklog::chat::NeuralBackend::from_dir(&dir) {
        Ok(backend) => {
            tracing::info!("Neural chat backend loaded from {}", dir.display());
            Some(backend)
        }
        Err(e) => {
            tracing::warn!("Neural backend load failed (using phrasebook): {e}");
            None
        }
    }
}

#[cfg(not(feature = "neural"))]
#[allow(dead_code)]
pub fn try_load_neural_backend(
    _model_dir: Option<&std::path::Path>,
) -> Option<std::convert::Infallible> {
    None
}

// ── NeuralCtx ─────────────────────────────────────────────────────────────────
//
// A drop-in companion to `ChatCtx` that delegates generation to `NeuralBackend`
// and falls back to `ChatCtx` if the neural backend returns `None`.
//
// Only compiled when the `neural` feature is active.

#[cfg(feature = "neural")]
use froklog::chat::ChatBackend;

#[cfg(feature = "neural")]
pub struct NeuralCtx<'a> {
    neural: froklog::chat::NeuralBackend,
    fallback: &'a PhraseDb,
    chat_level: u8,
}

#[cfg(feature = "neural")]
impl<'a> NeuralCtx<'a> {
    pub fn new(
        neural: froklog::chat::NeuralBackend,
        fallback: &'a PhraseDb,
        chat_level: u8,
    ) -> Self {
        Self {
            neural,
            fallback,
            chat_level,
        }
    }

    /// Same interface as `ChatCtx::respond()`.
    pub fn respond(
        &mut self,
        trigger: &ChatTrigger<'_>,
        personality: &PersonalityProfile,
        state: &SimChatState,
        group_size: u8,
        zone: Option<&str>,
        cur_sec: u32,
        rng: &mut StdRng,
    ) -> Option<String> {
        if self.chat_level == 0 {
            return None;
        }

        // Cooldown / urgency gate — same as ChatCtx
        if trigger.urgency() < 3 {
            if let Some(ref t) = trigger.topic_hint() {
                if state.topic_on_cooldown(t, trigger.cooldown_secs(), cur_sec) {
                    return None;
                }
            }
        }

        // Speak probability  — compute via a temporary ChatCtx
        let prob = {
            let phrasebook_ctx = ChatCtx::new(self.fallback, self.chat_level);
            phrasebook_ctx.speak_probability(trigger, personality, state)
        };
        if rng.gen::<f32>() >= prob {
            return None;
        }

        // Build SituationContext for neural backend
        let ctx = build_situation_context(trigger, personality, state, group_size, zone);
        tracing::debug!(
            trigger_kind        = ctx.trigger_kind,
            archetype           = ?ctx.archetype,
            register            = ?ctx.register,
            expression_mode     = ?ctx.expression_mode,
            verbosity           = ctx.verbosity,
            humor               = ctx.humor,
            positivity          = ctx.positivity,
            patience            = ctx.patience,
            tactical_awareness  = ctx.tactical_awareness,
            social_investment   = ctx.social_investment,
            frustration         = ctx.frustration,
            morale              = ctx.morale,
            fatigue             = ctx.fatigue,
            relationship        = ?ctx.relationship,
            group_size          = ctx.group_size,
            zone                = ?ctx.zone,
            slots               = ?ctx.slots,
            "neural SituationContext built"
        );

        // Try neural first; fall back to phrasebook on None
        let raw = self.neural.generate(&ctx, rng).or_else(|| {
            tracing::debug!("neural returned None — falling back to phrasebook");
            let phrasebook_ctx = ChatCtx::new(self.fallback, self.chat_level);
            phrasebook_ctx.raw_message_pub(trigger, &personality.archetype, &state.register(), rng)
        });

        if let Some(ref r) = raw {
            tracing::debug!(output = %r, "neural/fallback raw output");
        }

        raw.map(|r: String| apply_style(&r, personality, rng))
    }

    /// Consume `self` and return the owned `NeuralBackend`.
    pub fn into_backend(self) -> froklog::chat::NeuralBackend {
        self.neural
    }

    /// Pick a phrase template via the fallback phrasebook (no neural involvement).
    pub fn pick_template(&self, kind: &str, rng: &mut StdRng) -> Option<String> {
        ChatCtx::new(self.fallback, self.chat_level).pick_template(kind, rng)
    }
}

/// Build a `SituationContext` from loggen's trigger/personality/state types.
#[cfg(feature = "neural")]
#[allow(dead_code)]
fn build_situation_context(
    trigger: &ChatTrigger<'_>,
    personality: &PersonalityProfile,
    state: &SimChatState,
    group_size: u8,
    zone: Option<&str>,
) -> SituationContext {
    use froklog::chat::SituationContext;
    // RecentMessage not yet used (future: rolling history buffer)
    #[allow(unused_imports)]
    use froklog::chat::context::RecentMessage;

    let (trigger_kind, slots) = trigger_kind_and_slots(trigger);

    SituationContext {
        trigger_kind,
        slots,
        archetype: personality.archetype.clone(),
        register: state.register(),
        verbosity: personality.verbosity,
        humor: personality.humor,
        positivity: personality.positivity,
        patience: personality.patience,
        tactical_awareness: personality.tactical_awareness,
        social_investment: personality.social_investment,
        expression_mode: personality.expression_mode.clone(),
        frustration: state.frustration,
        morale: state.morale,
        fatigue: state.fatigue,
        relationship: None,
        zone: zone.map(|s| s.to_owned()),
        group_size,
        recent_messages: Vec::new(),
    }
}

/// Map a `ChatTrigger` to the canonical `trigger_kind` string and named slots.
#[cfg(feature = "neural")]
#[allow(dead_code)]
fn trigger_kind_and_slots(trigger: &ChatTrigger<'_>) -> (&'static str, HashMap<String, String>) {
    let mut slots = HashMap::new();
    let kind = match trigger {
        ChatTrigger::BigSpellHit {
            caster,
            spell,
            mob,
            damage,
        } => {
            slots.insert("caster".into(), caster.to_string());
            slots.insert("src".into(), caster.to_string());
            slots.insert("spell".into(), spell.to_string());
            slots.insert("ability".into(), spell.to_string());
            slots.insert("mob".into(), mob.to_string());
            slots.insert("tgt".into(), mob.to_string());
            slots.insert("damage".into(), damage.to_string());
            slots.insert("dmg".into(), damage.to_string());
            "BigHit"
        }
        ChatTrigger::NearDeathSave { healer, target } => {
            slots.insert("healer".into(), healer.to_string());
            slots.insert("target".into(), target.to_string());
            "NearDeathSave"
        }
        ChatTrigger::PlayerNearDeath { player } => {
            slots.insert("player".into(), player.to_string());
            "NearDeath"
        }
        ChatTrigger::MobKilled { mob } => {
            slots.insert("mob".into(), mob.to_string());
            "MobKilled"
        }
        ChatTrigger::CasterOom { player } => {
            slots.insert("player".into(), player.to_string());
            "CasterOom"
        }
        ChatTrigger::RepeatMiss { player, mob } => {
            slots.insert("player".into(), player.to_string());
            slots.insert("mob".into(), mob.to_string());
            "RepeatMiss"
        }
        ChatTrigger::SlowResisted => "SpellResisted",
        ChatTrigger::Incoming { count, mob } => {
            slots.insert("count".into(), count.to_string());
            slots.insert("mob".into(), mob.to_string());
            "Incoming"
        }
        ChatTrigger::HealerMana { pct } => {
            slots.insert("pct".into(), pct.to_string());
            "HealerMana"
        }
        ChatTrigger::Generic => "Generic",
        ChatTrigger::MultiKill => "MultiKill",
        ChatTrigger::TankDying { player } => {
            slots.insert("player".into(), player.to_string());
            "TankDying"
        }
    };
    (kind, slots)
}

// ── Chat dispatch (unifies phrasebook and neural) ─────────────────────────────

/// Unified dispatcher held across encounters.  Holds either a phrasebook
/// context or a neural backend.  `group_size` and `zone` are passed through to
/// the neural variant and ignored by the phrasebook variant.
pub enum ChatDispatch<'a> {
    Phrasebook(ChatCtx<'a>),
    #[cfg(feature = "neural")]
    Neural(NeuralCtx<'a>),
}

impl<'a> ChatDispatch<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn respond(
        &mut self,
        trigger: &ChatTrigger<'_>,
        personality: &PersonalityProfile,
        state: &SimChatState,
        #[allow(unused_variables)] group_size: u8,
        #[allow(unused_variables)] zone: Option<&str>,
        cur_sec: u32,
        rng: &mut StdRng,
    ) -> Option<String> {
        match self {
            Self::Phrasebook(ctx) => ctx.respond(trigger, personality, state, cur_sec, rng),
            #[cfg(feature = "neural")]
            Self::Neural(ctx) => {
                ctx.respond(trigger, personality, state, group_size, zone, cur_sec, rng)
            }
        }
    }

    pub fn pick_template(&self, kind: &str, rng: &mut StdRng) -> Option<String> {
        match self {
            Self::Phrasebook(ctx) => ctx.pick_template(kind, rng),
            #[cfg(feature = "neural")]
            Self::Neural(ctx) => ctx.pick_template(kind, rng),
        }
    }

    /// Consume `self` and return the `NeuralBackend` if this is a neural
    /// dispatch; `None` if it is a phrasebook dispatch.
    #[cfg(feature = "neural")]
    pub fn into_neural_backend(self) -> Option<froklog::chat::NeuralBackend> {
        match self {
            Self::Neural(ctx) => Some(ctx.into_backend()),
            Self::Phrasebook(_) => None,
        }
    }
}
