/// Situation context passed to a `ChatBackend` for response generation.
///
/// Carries everything a backend needs to produce a contextually appropriate
/// message: the trigger kind and template slots (for `PhrasebookBackend`),
/// plus the full personality / state / relationship snapshot (for `NeuralBackend`).
use std::collections::HashMap;

use super::personality::{Archetype, ExpressionMode};
use super::relationship::RelDynamic;
use super::state::EmotionalRegister;

// ── Supporting types ──────────────────────────────────────────────────────────

/// A recent message observed in the channel, used as conversational context.
#[derive(Debug, Clone)]
pub struct RecentMessage {
    /// Name of the speaker.
    pub speaker: String,
    /// Verbatim message text.
    pub text: String,
    /// Approximate seconds since this message was sent.
    pub secs_ago: f32,
}

// ── SituationContext ──────────────────────────────────────────────────────────

/// Full situational snapshot assembled at trigger-dispatch time.
///
/// `PhrasebookBackend` uses `trigger_kind` + `slots` + `archetype` + `register`.
/// `NeuralBackend` uses all fields to build the generation prompt prefix.
#[derive(Debug)]
pub struct SituationContext {
    // ── Phrasebook compat (required by PhrasebookBackend) ─────────────────────

    /// Canonical trigger kind string (e.g. `"MobKilled"`, `"SpellResisted"`).
    pub trigger_kind: &'static str,

    /// Named template slots (e.g. `"mob"` → `"Yael"`, `"dmg"` → `"2340"`).
    pub slots: HashMap<String, String>,

    // ── Personality ───────────────────────────────────────────────────────────

    /// The speaking character's named archetype.
    pub archetype: Archetype,

    /// Current emotional register, derived from continuous state.
    pub register: EmotionalRegister,

    pub verbosity: f32,
    pub humor: f32,
    pub positivity: f32,
    pub patience: f32,
    pub tactical_awareness: f32,
    pub social_investment: f32,
    pub expression_mode: ExpressionMode,

    // ── Emotional state ───────────────────────────────────────────────────────

    pub frustration: f32,
    pub morale: f32,
    pub fatigue: f32,

    // ── Social context ────────────────────────────────────────────────────────

    /// Structural dynamic of the speaker's relationship to the trigger's
    /// primary other participant (healer, killer, target, etc.).
    /// `None` when the trigger has no obvious other participant.
    pub relationship: Option<RelDynamic>,

    // ── Game state ────────────────────────────────────────────────────────────

    /// Current zone name. `None` if the caller has not provided it.
    pub zone: Option<String>,

    /// Number of characters currently registered in the engine.
    pub group_size: u8,

    /// Last N messages observed in the channel, oldest first.
    /// Empty until the engine gains a rolling history buffer.
    pub recent_messages: Vec<RecentMessage>,
}
