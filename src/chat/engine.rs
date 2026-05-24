/// NPC chat engine — the dispatcher that drives dynamic group chat.
///
/// Flow:
///   1. Caller feeds game events via `on_trigger()`
///   2. Engine evaluates each character: should they speak? what do they say?
///   3. Speak probability is gated by personality × state × relationship × urgency
///   4. Selected phrases are styled per character, then queued with realistic delay
///   5. Caller drains ready messages via `poll(now)`
///
/// The established-duo relationship (e.g. Icestorm/Talodar) makes the
/// action-expresser's silence comfortable and permits RL bleed-in from the talker.
use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::backend::{ChatBackend, PhrasebookBackend};
use super::context::SituationContext;
use super::personality::{ExpressionMode, PersonalityProfile};
use super::phrasebook::PhraseDb;
use super::relationship::{RelDynamic, Relationship, RelationshipGraph};
use super::state::{CharacterState, ChatTopic, RlKind};
use super::trigger::ChatTrigger;

// ── Types ─────────────────────────────────────────────────────────────────────

struct Character {
    name: String,
    profile: PersonalityProfile,
    state: CharacterState,
}

/// A message scheduled for future emission.
pub struct PendingMessage {
    pub emit_at: Instant,
    pub speaker: String,
    pub channel: String,
    pub text: String,
}

/// A message ready to emit now.
pub struct EmittedMessage {
    pub speaker: String,
    pub channel: String,
    pub text: String,
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct ChatEngine {
    characters: HashMap<String, Character>,
    relationships: RelationshipGraph,
    backend: Box<dyn ChatBackend>,
    rng: StdRng,
    pending: VecDeque<PendingMessage>,
    last_decay: Instant,
}

impl ChatEngine {
    /// Create an engine using the built-in phrasebook backend.
    pub fn new(phrases: PhraseDb, seed: u64) -> Self {
        Self::with_backend(Box::new(PhrasebookBackend(phrases)), seed)
    }

    /// Create an engine with a custom backend (e.g. `NeuralBackend`).
    pub fn with_backend(backend: Box<dyn ChatBackend>, seed: u64) -> Self {
        Self {
            characters: HashMap::new(),
            relationships: RelationshipGraph::new(),
            backend,
            rng: StdRng::seed_from_u64(seed),
            pending: VecDeque::new(),
            last_decay: Instant::now(),
        }
    }

    // ── Roster management ─────────────────────────────────────────────────────

    pub fn add_character(&mut self, name: &str, profile: PersonalityProfile) {
        self.characters.insert(
            name.to_string(),
            Character {
                name: name.to_string(),
                profile,
                state: CharacterState::new(),
            },
        );
    }

    pub fn set_relationship(&mut self, from: &str, to: &str, rel: Relationship) {
        self.relationships.set(from, to, rel);
    }

    /// Convenience: establish the asymmetric established-duo archetype.
    pub fn set_established_duo(&mut self, talker: &str, listener: &str) {
        self.relationships.set_established_duo(talker, listener);
    }

    /// Convenience: set symmetric teammate relationship.
    pub fn set_teammates(&mut self, a: &str, b: &str) {
        self.relationships
            .set_symmetric(a, b, RelDynamic::Teammates);
    }

    // ── State access ──────────────────────────────────────────────────────────

    pub fn character_state(&self, name: &str) -> Option<&CharacterState> {
        self.characters.get(name).map(|c| &c.state)
    }

    // ── Trigger dispatch ──────────────────────────────────────────────────────

    /// Process a trigger. Returns messages queued for future emission.
    /// Caller should feed these to the pending queue or handle them directly.
    pub fn on_trigger(&mut self, trigger: ChatTrigger, channel: &str) -> Vec<PendingMessage> {
        // Apply state changes across all characters for this event
        self.apply_state_updates(&trigger);

        let now = Instant::now();
        let urgency = trigger.urgency();
        let names: Vec<String> = self.characters.keys().cloned().collect();
        let mut results = Vec::new();

        for name in &names {
            // Characters don't react to themselves for most triggers
            if self.is_self_trigger(&trigger, name) && !self.self_trigger_speaks(&trigger) {
                continue;
            }

            let prob = self.speak_probability(&trigger, name);
            if self.rng.gen::<f32>() > prob {
                // Even if silent, track the miss for pressure buildup
                continue;
            }

            let Some(text) = self.build_message(&trigger, name) else {
                continue;
            };

            let delay_secs = self.response_delay(name, urgency);
            let emit_at = now + Duration::from_secs_f32(delay_secs);

            // Mark state — topic spoken, last spoke time
            if let Some(ch) = self.characters.get_mut(name) {
                ch.state.mark_spoke();
                if let Some(topic) = trigger.topic() {
                    ch.state.mark_topic_spoken(topic);
                }
            }

            // Maybe schedule a typo correction as a follow-up message
            if let Some(correction) = self.maybe_correction(&text, name, emit_at) {
                results.push(correction);
            }

            results.push(PendingMessage {
                emit_at,
                speaker: name.clone(),
                channel: channel.to_string(),
                text,
            });
        }

        // Stable sort by emit time
        results.sort_by_key(|m| m.emit_at);
        results
    }

    /// Drain messages that are ready at `now`.
    pub fn poll(&mut self, now: Instant) -> Vec<EmittedMessage> {
        let mut out = Vec::new();
        while self
            .pending
            .front()
            .map(|m| m.emit_at <= now)
            .unwrap_or(false)
        {
            let m = self.pending.pop_front().unwrap();
            out.push(EmittedMessage {
                speaker: m.speaker,
                channel: m.channel,
                text: m.text,
            });
        }
        out
    }

    /// Queue pending messages from a trigger dispatch into the internal queue.
    /// Messages must be in emit-time order (on_trigger guarantees this).
    pub fn queue(&mut self, messages: Vec<PendingMessage>) {
        for m in messages {
            // Insert in sorted position
            let pos = self.pending.partition_point(|p| p.emit_at <= m.emit_at);
            self.pending.insert(pos, m);
        }
    }

    // ── Periodic decay ────────────────────────────────────────────────────────

    /// Apply time-based emotional decay. Call on a 30-second-ish cadence.
    pub fn decay_tick(&mut self) {
        let elapsed = self.last_decay.elapsed().as_secs_f32();
        self.last_decay = Instant::now();
        for ch in self.characters.values_mut() {
            ch.state.decay_tick(elapsed);
        }
    }

    // ── Speak probability ─────────────────────────────────────────────────────

    fn speak_probability(&self, trigger: &ChatTrigger, name: &str) -> f32 {
        let Some(ch) = self.characters.get(name) else {
            return 0.0;
        };

        let urgency_scale = match trigger.urgency() {
            3 => 2.20,
            2 => 1.55,
            1 => 1.00,
            _ => 0.45, // idle
        };

        let mut prob = ch.profile.verbosity * urgency_scale;

        // Action-expressers are nearly silent for non-tactical social events
        if ch.profile.expression_mode == ExpressionMode::Action
            && !self.is_tactical_trigger(trigger)
        {
            prob *= 0.12;
        }

        // Pressure buildup: silent event count increases probability
        if let Some(topic) = trigger.topic() {
            let pressure = (ch.state.silent_count(&topic) as f32 * 0.22).min(0.75);
            prob = (prob + pressure).min(1.0);
        }

        // Topic cooldown heavily suppresses (but high-urgency can break through)
        if let Some(topic) = trigger.topic() {
            let cooldown = self.cooldown_for_topic(&topic);
            if ch.state.topic_on_cooldown(&topic, cooldown) && trigger.urgency() < 3 {
                prob *= 0.08;
            }
        }

        // RL distraction reduces presence
        if ch.state.is_rl_distracted() {
            prob *= 0.25;
        }

        // Fatigue quietly erodes verbosity
        prob *= 1.0 - (ch.state.fatigue * 0.28);

        // Frustration boosts probability for frustration-adjacent triggers
        if matches!(
            trigger,
            ChatTrigger::SpellResisted { .. }
                | ChatTrigger::ResistStreak { .. }
                | ChatTrigger::PlayerDied { .. }
                | ChatTrigger::Wipe
                | ChatTrigger::BadLoot { .. }
        ) && ch.state.frustration > 0.55
        {
            prob = (prob * 1.45).min(1.0);
        }

        prob.clamp(0.0, 1.0)
    }

    fn is_tactical_trigger(&self, trigger: &ChatTrigger) -> bool {
        matches!(
            trigger,
            ChatTrigger::Incoming { .. }
                | ChatTrigger::PlayerNearDeath { .. }
                | ChatTrigger::TankLowHp { .. }
                | ChatTrigger::HealerLowMana { .. }
                | ChatTrigger::CasterOom { .. }
                | ChatTrigger::Add { .. }
        )
    }

    fn cooldown_for_topic(&self, topic: &ChatTopic) -> u64 {
        match topic {
            ChatTopic::Slay => 45,
            ChatTopic::Resist => 30,
            ChatTopic::Death => 20,
            ChatTopic::Loot => 60,
            ChatTopic::LevelUp => 5,
            ChatTopic::NearDeath => 15,
            ChatTopic::LowMana => 25,
            ChatTopic::Incoming => 8,
            ChatTopic::Wipe => 10,
            ChatTopic::Banter => 90,
            ChatTopic::RealLife => 120,
            ChatTopic::CleanStreak => 120,
        }
    }

    // ── Self-trigger logic ────────────────────────────────────────────────────

    fn is_self_trigger(&self, trigger: &ChatTrigger, name: &str) -> bool {
        match trigger {
            ChatTrigger::LevelUp { who, .. } => who == name,
            ChatTrigger::TankLowHp { player, .. } => player == name,
            ChatTrigger::HealerLowMana { player, .. } => player == name,
            ChatTrigger::NearDeathSave { healer, .. } => healer == name,
            ChatTrigger::BigHit { src, .. } => src == name,
            ChatTrigger::RepeatMiss { player, .. } => player == name,
            ChatTrigger::MobKilled { killer, .. } => killer == name,
            ChatTrigger::RlDeparture { who, .. } => who == name,
            ChatTrigger::RlReturn { who, .. } => who == name,
            _ => false,
        }
    }

    /// Some self-triggers should still speak (announcing AFK, reacting to own OOM).
    fn self_trigger_speaks(&self, trigger: &ChatTrigger) -> bool {
        matches!(
            trigger,
            ChatTrigger::CasterOom { .. }
                | ChatTrigger::HealerLowMana { .. }
                | ChatTrigger::RlDeparture { .. }
                | ChatTrigger::RlReturn { .. }
                | ChatTrigger::PlayerDied { .. } // can react to own death ("ugh")
        )
    }

    // ── Phrase building ───────────────────────────────────────────────────────

    fn build_message(&mut self, trigger: &ChatTrigger, name: &str) -> Option<String> {
        let ctx = self.build_context(trigger, name)?;
        let text = self.backend.generate(&ctx, &mut self.rng)?;
        let text = self.apply_style(text, name);
        let text = self.deduplicate(text, name)?;
        Some(text)
    }

    /// Assemble a `SituationContext` for `name` reacting to `trigger`.
    fn build_context(&self, trigger: &ChatTrigger, name: &str) -> Option<SituationContext> {
        let ch = self.characters.get(name)?;
        let other = Self::trigger_primary_other(trigger, name);
        let relationship = other
            .and_then(|o| self.relationships.get(name, o))
            .map(|r| r.dynamic.clone());
        Some(SituationContext {
            trigger_kind: Self::trigger_kind(trigger),
            slots: Self::extract_slots(trigger, name),
            archetype: ch.profile.archetype.clone(),
            register: ch.state.register(),
            verbosity: ch.profile.verbosity,
            humor: ch.profile.humor,
            positivity: ch.profile.positivity,
            patience: ch.profile.patience,
            tactical_awareness: ch.profile.tactical_awareness,
            social_investment: ch.profile.social_investment,
            expression_mode: ch.profile.expression_mode.clone(),
            frustration: ch.state.frustration,
            morale: ch.state.morale,
            fatigue: ch.state.fatigue,
            relationship,
            zone: None,
            group_size: self.characters.len() as u8,
            recent_messages: Vec::new(),
        })
    }

    /// Return the name of the trigger's primary other participant (not `self_name`),
    /// used to look up a relationship for context.
    fn trigger_primary_other<'a>(trigger: &'a ChatTrigger, self_name: &str) -> Option<&'a str> {
        match trigger {
            ChatTrigger::NearDeathSave { healer, target } => {
                if healer == self_name {
                    Some(target.as_str())
                } else {
                    Some(healer.as_str())
                }
            }
            ChatTrigger::BigHit { src, tgt, .. } => {
                if src == self_name {
                    Some(tgt.as_str())
                } else {
                    Some(src.as_str())
                }
            }
            ChatTrigger::MobKilled { killer, .. } => Some(killer.as_str()),
            ChatTrigger::RlDeparture { who, .. } | ChatTrigger::RlReturn { who, .. } => {
                Some(who.as_str())
            }
            _ => None,
        }
    }

    fn trigger_kind(trigger: &ChatTrigger) -> &'static str {
        match trigger {
            ChatTrigger::MobKilled { .. } => "MobKilled",
            ChatTrigger::PlayerDied { .. } => "PlayerDied",
            ChatTrigger::NearDeathSave { .. } => "NearDeathSave",
            ChatTrigger::PlayerNearDeath { .. } => "PlayerNearDeath",
            ChatTrigger::BigHit { .. } => "BigHit",
            ChatTrigger::SpellResisted { .. } => "SpellResisted",
            ChatTrigger::ResistStreak { .. } => "ResistStreak",
            ChatTrigger::Wipe => "Wipe",
            ChatTrigger::CleanStreak { .. } => "CleanStreak",
            ChatTrigger::TankLowHp { .. } => "PlayerNearDeath",
            ChatTrigger::HealerLowMana { .. } => "CasterOom",
            ChatTrigger::CasterOom { .. } => "CasterOom",
            ChatTrigger::Incoming { .. } => "Incoming",
            ChatTrigger::Add { .. } => "Incoming",
            ChatTrigger::RepeatMiss { .. } => "RepeatMiss",
            ChatTrigger::GoodLoot { .. } => "GoodLoot",
            ChatTrigger::BadLoot { .. } => "BadLoot",
            ChatTrigger::LevelUp { .. } => "LevelUp",
            ChatTrigger::RlDeparture { .. } => "RlDeparture",
            ChatTrigger::RlReturn { .. } => "RlReturn",
            _ => "Generic",
        }
    }

    fn extract_slots(trigger: &ChatTrigger, self_name: &str) -> HashMap<String, String> {
        let mut s = HashMap::new();
        s.insert("self".into(), self_name.into());
        match trigger {
            ChatTrigger::MobKilled { mob, killer, .. } => {
                s.insert("mob".into(), mob.clone());
                s.insert("killer".into(), killer.clone());
            }
            ChatTrigger::PlayerDied { who, cause } => {
                s.insert("player".into(), who.clone());
                s.insert("cause".into(), cause.clone());
            }
            ChatTrigger::NearDeathSave { healer, target } => {
                s.insert("healer".into(), healer.clone());
                s.insert("target".into(), target.clone());
            }
            ChatTrigger::PlayerNearDeath { who, .. }
            | ChatTrigger::TankLowHp { player: who, .. }
            | ChatTrigger::CasterOom { player: who }
            | ChatTrigger::HealerLowMana { player: who, .. } => {
                s.insert("player".into(), who.clone());
            }
            ChatTrigger::BigHit {
                src,
                tgt,
                dmg,
                ability,
            } => {
                s.insert("src".into(), src.clone());
                s.insert("tgt".into(), tgt.clone());
                s.insert("dmg".into(), dmg.to_string());
                s.insert("ability".into(), ability.clone());
            }
            ChatTrigger::SpellResisted {
                caster,
                mob,
                spell,
                nth,
            } => {
                s.insert("caster".into(), caster.clone());
                s.insert("mob".into(), mob.clone());
                s.insert("spell".into(), spell.clone());
                s.insert("count".into(), nth.to_string());
            }
            ChatTrigger::ResistStreak { mob, count } => {
                s.insert("mob".into(), mob.clone());
                s.insert("count".into(), count.to_string());
            }
            ChatTrigger::Incoming { count, mob, puller } => {
                s.insert("count".into(), count.to_string());
                s.insert("mob".into(), mob.clone());
                s.insert("puller".into(), puller.clone());
            }
            ChatTrigger::Add { mob } => {
                s.insert("mob".into(), mob.clone());
            }
            ChatTrigger::RepeatMiss { player, mob, count } => {
                s.insert("player".into(), player.clone());
                s.insert("mob".into(), mob.clone());
                s.insert("count".into(), count.to_string());
            }
            ChatTrigger::GoodLoot { item, recipient } => {
                s.insert("item".into(), item.clone());
                s.insert("recipient".into(), recipient.clone());
            }
            ChatTrigger::BadLoot { item } => {
                s.insert("item".into(), item.clone());
            }
            ChatTrigger::LevelUp { who, level } => {
                s.insert("who".into(), who.clone());
                s.insert("level".into(), level.to_string());
            }
            ChatTrigger::CleanStreak { kills } => {
                s.insert("count".into(), kills.to_string());
            }
            ChatTrigger::RlDeparture { who, kind } => {
                s.insert("who".into(), who.clone());
                s.insert("rl_reason".into(), rl_reason_text(kind).into());
            }
            ChatTrigger::RlReturn { who, missed } => {
                s.insert("who".into(), who.clone());
                s.insert(
                    "rl_context".into(),
                    missed.first().cloned().unwrap_or_default(),
                );
            }
            _ => {}
        }
        s
    }

    // ── Style transforms ──────────────────────────────────────────────────────

    fn apply_style(&mut self, mut text: String, name: &str) -> String {
        let Some(ch) = self.characters.get(name) else {
            return text;
        };
        let style = ch.profile.style.clone();
        let profile = ch.profile.clone();

        // Lowercase transform
        if self.rng.gen::<f32>() < style.lowercase_tendency {
            text = text.to_lowercase();
        }

        // Signature filler — low probability append
        if let Some(ref filler) = style.signature_filler {
            if self.rng.gen::<f32>() < 0.14
                && !text
                    .to_lowercase()
                    .ends_with(filler.to_lowercase().as_str())
            {
                text = format!("{text} {filler}");
            }
        }

        // Fragment truncation for low-expressiveness characters
        if profile.expressiveness < 0.35 {
            text = truncate_to_fragment(text);
        }

        text
    }

    fn deduplicate(&mut self, text: String, name: &str) -> Option<String> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        let hash = h.finish();

        let ch = self.characters.get_mut(name)?;
        if ch.state.phrase_is_fresh(hash) {
            ch.state.record_phrase(hash);
            Some(text)
        } else {
            // Phrase repeated — suppress this emit
            None
        }
    }

    // ── Timing ────────────────────────────────────────────────────────────────

    fn response_delay(&mut self, name: &str, urgency: u8) -> f32 {
        let base = self
            .characters
            .get(name)
            .map(|c| c.profile.style.base_response_delay_secs)
            .unwrap_or(10.0);

        let urgency_scale = match urgency {
            3 => 0.25,
            2 => 0.55,
            1 => 1.00,
            _ => 1.80,
        };

        let rl_penalty = if self
            .characters
            .get(name)
            .map(|c| c.state.is_rl_distracted())
            .unwrap_or(false)
        {
            1.8
        } else {
            1.0
        };

        // ±30% jitter
        let jitter: f32 = self.rng.gen_range(-0.30..=0.30);
        (base * urgency_scale * rl_penalty * (1.0 + jitter)).max(0.5)
    }

    // ── Typo corrections ──────────────────────────────────────────────────────

    fn maybe_correction(
        &mut self,
        text: &str,
        name: &str,
        after: Instant,
    ) -> Option<PendingMessage> {
        let rate = self
            .characters
            .get(name)?
            .profile
            .style
            .typo_correction_rate;

        // Typos only make sense on messages long enough to have one
        if text.len() < 8 || self.rng.gen::<f32>() >= rate * 0.12 {
            return None;
        }

        let corrected = inject_typo(text, &mut self.rng);
        if corrected == text {
            return None;
        }

        // Correction fires 2-8 seconds after the typo'd message
        let gap = Duration::from_secs(self.rng.gen_range(2..=8));

        // The original message gets the typo; we emit a "* correction" follow-up.
        let correction_text = format!("*{}", &text[text.len().saturating_sub(8)..]);
        Some(PendingMessage {
            emit_at: after + gap,
            speaker: name.to_string(),
            channel: String::new(), // caller fills channel context if needed
            text: correction_text,
        })
    }

    // ── State updates ─────────────────────────────────────────────────────────

    fn apply_state_updates(&mut self, trigger: &ChatTrigger) {
        for ch in self.characters.values_mut() {
            match trigger {
                ChatTrigger::MobKilled { .. } => ch.state.on_slay(),
                ChatTrigger::PlayerDied { .. } => ch.state.on_death(),
                ChatTrigger::Wipe => ch.state.on_wipe(),
                ChatTrigger::SpellResisted { .. } => ch.state.on_resist(),
                ChatTrigger::ResistStreak { .. } => ch.state.on_resist(),
                ChatTrigger::GoodLoot { .. } => ch.state.on_good_loot(),
                ChatTrigger::BadLoot { .. } => ch.state.on_bad_loot(),
                ChatTrigger::LevelUp { .. } => ch.state.on_level_up(),
                ChatTrigger::PlayerNearDeath { .. } | ChatTrigger::TankLowHp { .. } => {
                    ch.state.on_near_death()
                }
                ChatTrigger::CleanStreak { .. } => ch.state.on_clean_streak(),
                ChatTrigger::RlDeparture { who, kind } if ch.name == *who => {
                    ch.state.start_rl_event(kind.clone());
                }
                ChatTrigger::RlReturn { who, .. } if ch.name == *who => {
                    ch.state.end_rl_event();
                }
                _ => {}
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Truncate a message to its first sentence or meaningful fragment.
fn truncate_to_fragment(text: String) -> String {
    // Stop at sentence-ending punctuation
    for (i, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?') && i > 2 {
            return text[..=i].trim().to_string();
        }
    }
    // Stop at a comma if the message is long
    if text.len() > 30 {
        if let Some(pos) = text.find(',') {
            return text[..pos].trim().to_string();
        }
    }
    text
}

/// Inject a simple typo into a message for the correction pattern.
fn inject_typo(text: &str, rng: &mut impl Rng) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 4 {
        return text.to_string();
    }
    // Drop the last character of a word (most common real-player typo)
    let last_space = text.rfind(' ').unwrap_or(0);
    if last_space + 2 < text.len() {
        let cut = rng.gen_range(last_space + 1..text.len());
        text[..cut].to_string()
    } else {
        text.to_string()
    }
}

/// Produce a human-readable RL reason string for departure templates.
fn rl_reason_text(kind: &RlKind) -> &'static str {
    match kind {
        RlKind::Food => "grabbing food",
        RlKind::Bathroom => "bio break",
        RlKind::Phone => "phone",
        RlKind::Family => "one sec",
        RlKind::Pet => "dog needs out",
        RlKind::Chores => "sec",
        RlKind::Distraction => "one sec",
    }
}
