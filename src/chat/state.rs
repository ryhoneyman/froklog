/// Per-character dynamic state for the chat engine.
///
/// The state machine tracks emotional context (frustration, morale, fatigue)
/// and real-life interruption events. All float fields decay toward neutral
/// over time so a rough fight doesn't poison the entire session.
use std::collections::HashMap;
use std::time::Instant;

// ── Emotional state ───────────────────────────────────────────────────────────

/// Dynamic per-character state. Updated by game events and time passage.
#[derive(Debug)]
pub struct CharacterState {
    /// Accumulated frustration: deaths, resist streaks, bad loot, slow pulls.
    /// Decays toward 0.0 over time.
    pub frustration: f32,

    /// Current morale: clean kills, good loot, level ups, clean streaks.
    /// Decays toward 0.5 (neutral) over time.
    pub morale: f32,

    /// Session fatigue: accumulates slowly over hours; drops after long breaks.
    pub fatigue: f32,

    /// Combat engagement: spikes during active fights, falls in downtime.
    pub engagement: f32,

    /// Active real-life interruption, if any.
    pub rl_active: Option<RlEvent>,

    /// When this character last produced a chat message.
    pub last_spoke: Option<Instant>,

    /// Per-topic cooldowns — prevents repeating the same comment twice fast.
    pub topic_cooldowns: HashMap<ChatTopic, Instant>,

    /// Rolling dedup buffer of recently emitted phrase hashes.
    /// Prevents the same full phrase appearing twice in a session.
    pub recent_phrases: std::collections::VecDeque<u64>,

    /// Count of events of each type since last spoke — drives escalation.
    /// e.g. 3 resists without comment → comment becomes more likely.
    pub silent_event_counts: HashMap<ChatTopic, u32>,
}

impl CharacterState {
    pub fn new() -> Self {
        Self {
            frustration: 0.0,
            morale: 0.5,
            fatigue: 0.0,
            engagement: 0.0,
            rl_active: None,
            last_spoke: None,
            topic_cooldowns: HashMap::new(),
            recent_phrases: std::collections::VecDeque::with_capacity(48),
            silent_event_counts: HashMap::new(),
        }
    }

    /// Apply time-based decay to all continuous emotional state.
    /// Call with elapsed real seconds since last tick.
    pub fn decay_tick(&mut self, elapsed_secs: f32) {
        // Frustration drains over ~5 minutes
        let decay = elapsed_secs / 300.0;
        self.frustration = (self.frustration - decay).max(0.0);

        // Morale drifts back toward 0.5 (neutral) over ~5 minutes
        self.morale += (0.5 - self.morale) * (elapsed_secs / 300.0);

        // Engagement collapses faster — ~90 seconds without combat
        self.engagement = (self.engagement - elapsed_secs / 90.0).max(0.0);

        // Fatigue climbs slowly over 2 hours of continuous play
        self.fatigue = (self.fatigue + elapsed_secs / 7_200.0).min(1.0);

        // Check if RL event has elapsed
        if let Some(ref ev) = self.rl_active {
            if ev.started.elapsed().as_secs() >= ev.expected_duration_secs as u64 {
                self.rl_active = None;
            }
        }
    }

    // ── State mutations from game events ──────────────────────────────────────

    pub fn on_slay(&mut self) {
        self.morale = (self.morale + 0.08).min(1.0);
        self.engagement = (self.engagement + 0.35).min(1.0);
        self.frustration = (self.frustration - 0.04).max(0.0);
        *self.silent_event_counts.entry(ChatTopic::Slay).or_insert(0) += 1;
    }

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

    pub fn on_good_loot(&mut self) {
        self.morale = (self.morale + 0.18).min(1.0);
        *self.silent_event_counts.entry(ChatTopic::Loot).or_insert(0) += 1;
    }

    pub fn on_bad_loot(&mut self) {
        self.frustration = (self.frustration + 0.04).min(1.0);
        *self.silent_event_counts.entry(ChatTopic::Loot).or_insert(0) += 1;
    }

    pub fn on_level_up(&mut self) {
        self.morale = (self.morale + 0.40).min(1.0);
        self.frustration = (self.frustration - 0.15).max(0.0);
    }

    pub fn on_near_death(&mut self) {
        self.frustration = (self.frustration + 0.12).min(1.0);
        self.engagement = (self.engagement + 0.50).min(1.0);
    }

    pub fn on_wipe(&mut self) {
        self.frustration = (self.frustration + 0.50).min(1.0);
        self.morale = (self.morale - 0.35).max(0.0);
    }

    pub fn on_clean_streak(&mut self) {
        self.morale = (self.morale + 0.15).min(1.0);
        self.frustration = (self.frustration - 0.10).max(0.0);
    }

    // ── Real-life interruptions ───────────────────────────────────────────────

    pub fn start_rl_event(&mut self, kind: RlKind) {
        let secs = kind.typical_duration_secs();
        self.rl_active = Some(RlEvent {
            kind,
            started: Instant::now(),
            expected_duration_secs: secs,
        });
    }

    pub fn end_rl_event(&mut self) -> Option<RlEvent> {
        self.rl_active.take()
    }

    pub fn is_rl_distracted(&self) -> bool {
        self.rl_active.is_some()
    }

    // ── Phrase dedup ──────────────────────────────────────────────────────────

    pub fn phrase_is_fresh(&self, hash: u64) -> bool {
        !self.recent_phrases.contains(&hash)
    }

    pub fn record_phrase(&mut self, hash: u64) {
        if self.recent_phrases.len() >= 48 {
            self.recent_phrases.pop_front();
        }
        self.recent_phrases.push_back(hash);
    }

    // ── Topic cooldowns ───────────────────────────────────────────────────────

    pub fn topic_on_cooldown(&self, topic: &ChatTopic, cooldown_secs: u64) -> bool {
        self.topic_cooldowns
            .get(topic)
            .map(|t| t.elapsed().as_secs() < cooldown_secs)
            .unwrap_or(false)
    }

    pub fn mark_topic_spoken(&mut self, topic: ChatTopic) {
        self.topic_cooldowns.insert(topic.clone(), Instant::now());
        // Reset silent accumulation for this topic
        self.silent_event_counts.remove(&topic);
    }

    /// How many times has this event type fired without a comment?
    /// Higher values increase speak probability (pressure builds with silence).
    pub fn silent_count(&self, topic: &ChatTopic) -> u32 {
        *self.silent_event_counts.get(topic).unwrap_or(&0)
    }

    // ── Timing ────────────────────────────────────────────────────────────────

    pub fn mark_spoke(&mut self) {
        self.last_spoke = Some(Instant::now());
    }

    pub fn secs_since_spoke(&self) -> Option<u64> {
        self.last_spoke.map(|t| t.elapsed().as_secs())
    }

    // ── Emotional register ────────────────────────────────────────────────────

    /// Summarized emotional state for phrase pool selection.
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
}

impl Default for CharacterState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Emotional register ────────────────────────────────────────────────────────

/// Summarized emotional context — drives which phrase variant is selected.
#[derive(Debug, Clone, PartialEq)]
pub enum EmotionalRegister {
    /// High frustration (>0.65): terse, dark, or passive-aggressive
    Frustrated,
    /// High morale (>0.80): enthusiastic, hype, celebratory
    Elated,
    /// High fatigue (>0.70): slower, quieter, "one more pull" energy
    Tired,
    /// High engagement (>0.55): sharp, fast, in-the-moment
    Engaged,
    /// Baseline: normal conversational tone
    Neutral,
}

// ── Chat topics ───────────────────────────────────────────────────────────────

/// Topics tracked for cooldown and silent-count pressure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChatTopic {
    Resist,
    Slay,
    Loot,
    Death,
    NearDeath,
    LowMana,
    LevelUp,
    Incoming,
    RealLife,
    Banter,
    Wipe,
    CleanStreak,
}

// ── Real-life events ──────────────────────────────────────────────────────────

/// An active real-life interruption.
#[derive(Debug, Clone)]
pub struct RlEvent {
    pub kind: RlKind,
    pub started: Instant,
    /// Expected duration — actual end time may differ.
    pub expected_duration_secs: u32,
}

/// Types of real-life interruptions, grounded in corpus examples.
#[derive(Debug, Clone, PartialEq)]
pub enum RlKind {
    /// Getting food or drink ("gonna grab a snack", "brb food")
    Food,
    /// Bathroom break
    Bathroom,
    /// Phone call or text
    Phone,
    /// Family member needing attention (child, partner, parent)
    Family,
    /// Pet needing attention
    Pet,
    /// Household chore (dishes, laundry, etc.)
    Chores,
    /// Partial distraction — still at keyboard but attention divided
    Distraction,
}

impl RlKind {
    /// Typical duration in seconds for this type of interruption.
    pub fn typical_duration_secs(&self) -> u32 {
        match self {
            RlKind::Bathroom => 90,
            RlKind::Food => 300,
            RlKind::Phone => 240,
            RlKind::Family => 600,
            RlKind::Pet => 120,
            RlKind::Chores => 480,
            RlKind::Distraction => 60,
        }
    }

    /// Probability weight for random RL event selection.
    /// Sums to 100 for easy percentage reasoning.
    pub fn weight(&self) -> u32 {
        match self {
            RlKind::Food => 25,
            RlKind::Bathroom => 20,
            RlKind::Distraction => 20,
            RlKind::Family => 18,
            RlKind::Phone => 10,
            RlKind::Pet => 5,
            RlKind::Chores => 2,
        }
    }
}
