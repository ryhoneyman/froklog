// Multi-dimensional personality profile for NPC chat characters.
//
// Replaces the simple 5-variant enum in loggen/chatmod.rs with continuous
// dimensions that drive phrase selection, timing, style, and RL behavior.
// Named archetypes are grounded in corpus analysis of real player voices.

// ── Dimensions ────────────────────────────────────────────────────────────────

/// All personality dimensions are f32 in [0.0, 1.0].
#[derive(Debug, Clone)]
pub struct PersonalityProfile {
    /// How often this character speaks at all — gates most trigger responses.
    pub verbosity: f32,

    /// When they do speak, how long/rich the message is.
    /// Low = fragments and single words; high = full sentences and paragraphs.
    pub expressiveness: f32,

    /// Tendency toward jokes, teasing, absurdist observations.
    pub humor: f32,

    /// Optimistic vs cynical framing of the same event.
    /// High: "nice kill"; low: "finally".
    pub positivity: f32,

    /// How long bad events can accumulate before frustration shows.
    /// High patience = slow to complain; low = quick to vent.
    pub patience: f32,

    /// How much this character notices and remarks on game mechanics,
    /// positioning, numbers, class abilities.
    pub tactical_awareness: f32,

    /// Real-life bleed-in tendency and investment in the group's social fabric.
    /// High: shares RL stories, checks on teammates; low: game-only presence.
    pub social_investment: f32,

    /// Primary mode of expressing engagement.
    pub expression_mode: ExpressionMode,

    /// Text-level style traits.
    pub style: StyleTraits,

    /// Named archetype for external reference and phrase pool selection.
    pub archetype: Archetype,
}

// ── Expression mode ───────────────────────────────────────────────────────────

/// How a character primarily expresses that they're engaged and care.
///
/// This is the key dimension unlocked by the Icestorm/Talodar analysis:
/// not everyone responds to events with words. Action-expressers communicate
/// through sustained, competent task execution — and their silence is not
/// disengagement; it's presence.
#[derive(Debug, Clone, PartialEq)]
pub enum ExpressionMode {
    /// Expresses through words — narrates, reacts, tells stories.
    /// Will comment on most events. RL life bleeds in naturally.
    Verbal,

    /// Expresses through task callouts and actions.
    /// "CH Talodar", "inc", "mez'd" — these ARE the responses.
    /// Social words are rare and carry weight when they appear.
    Action,

    /// Situational — tactical when combat demands it, social in downtime.
    Mixed,
}

// ── Style traits ──────────────────────────────────────────────────────────────

/// Text-level traits that produce authentic surface variation.
#[derive(Debug, Clone)]
pub struct StyleTraits {
    /// How often messages are fully lowercase (0.0 = always capitalize, 1.0 = never).
    /// Icestorm: ~0.85 (consistent lowercase "i", no caps); Talodar: ~0.4 (mixed).
    pub lowercase_tendency: f32,

    /// Probability a given message contains profanity.
    pub profanity_rate: f32,

    /// Tendency to use game/chat abbreviations (brb, afk, oom, inc, gg).
    pub abbreviation_rate: f32,

    /// Signature filler or reaction word unique to this character.
    /// Icestorm: "heh"; Talodar: "yeah buddy"; Xackery: "lol" as closer.
    pub signature_filler: Option<String>,

    /// Fraction of messages left as sentence fragments vs complete thoughts.
    pub fragment_tendency: f32,

    /// Probability a typo gets corrected with a follow-up message.
    /// Models the "she's on a rampag" → "+e" pattern.
    pub typo_correction_rate: f32,

    /// Average seconds before this character responds after a trigger.
    /// Spread into a realistic range via ± jitter in the engine.
    pub base_response_delay_secs: f32,
}

// ── Named archetypes ──────────────────────────────────────────────────────────

/// Named archetypes derived from real corpus analysis.
/// Used for phrase pool selection and relationship logic.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Archetype {
    /// Quiet action-expresser. Heals, calls targets, acts instead of narrating.
    /// Dry humor via "heh". Comfortable with silence.  (modeled on Icestorm)
    QuietAnchor,

    /// Verbose narrator. Weaves RL life into game chat without apology.
    /// "yeah buddy" enthusiasm, full stories mid-pull. (modeled on Talodar)
    ChaoticNarrator,

    /// Efficient raid leader. Loot-caller, terse coordinator.
    /// Humor appears rarely and lands harder for it. (modeled on Xackery)
    RaidLeader,

    /// Mostly reactive. Says very little unless something happens directly
    /// to them or is dramatic enough to warrant notice. (modeled on Moth/Zephon)
    ReactiveObserver,

    /// Tactical commentator. Notes positioning, ability usage, group comp.
    /// Less social investment; game is the focus.
    TacticalFocused,

    /// Doesn't fit a named archetype — use generic phrase pools.
    Custom,
}

// ── Pre-built profiles ────────────────────────────────────────────────────────

impl PersonalityProfile {
    /// The Icestorm voice: action-expresser, terse, lowercase, dry.
    ///
    /// Measured from corpus: 22.9 avg msg len, 33% reactive, 5.6% tactical,
    /// median 13s reaction lag, consistent lowercase "i", signature "heh".
    pub fn quiet_anchor() -> Self {
        Self {
            verbosity: 0.28,
            expressiveness: 0.18,
            humor: 0.40,
            positivity: 0.62,
            patience: 0.78,
            tactical_awareness: 0.82,
            social_investment: 0.45,
            expression_mode: ExpressionMode::Action,
            style: StyleTraits {
                lowercase_tendency: 0.85,
                profanity_rate: 0.15,
                abbreviation_rate: 0.72,
                signature_filler: Some("heh".into()),
                fragment_tendency: 0.88,
                typo_correction_rate: 0.60,
                base_response_delay_secs: 13.0,
            },
            archetype: Archetype::QuietAnchor,
        }
    }

    /// The Talodar voice: verbose narrator, RL bleeder, "yeah buddy" energy.
    ///
    /// Measured from corpus: 33.6 avg msg len, 37% reactive, 3.8% RL meta,
    /// median 11s lag, full paragraphs, self-deprecating lol-closer.
    pub fn chaotic_narrator() -> Self {
        Self {
            verbosity: 0.78,
            expressiveness: 0.88,
            humor: 0.65,
            positivity: 0.60,
            patience: 0.58,
            tactical_awareness: 0.65,
            social_investment: 0.92,
            expression_mode: ExpressionMode::Verbal,
            style: StyleTraits {
                lowercase_tendency: 0.38,
                profanity_rate: 0.38,
                abbreviation_rate: 0.44,
                signature_filler: Some("yeah buddy".into()),
                fragment_tendency: 0.38,
                typo_correction_rate: 0.28,
                base_response_delay_secs: 11.0,
            },
            archetype: Archetype::ChaoticNarrator,
        }
    }

    /// The Xackery voice: efficient raid leader, loot-caller, brief.
    ///
    /// Measured from corpus: 23.7 avg msg len, 8% loot class (highest),
    /// 29% reactive, 11s lag, terse humor, XD/lol emoticons.
    pub fn raid_leader() -> Self {
        Self {
            verbosity: 0.48,
            expressiveness: 0.32,
            humor: 0.30,
            positivity: 0.55,
            patience: 0.72,
            tactical_awareness: 0.90,
            social_investment: 0.28,
            expression_mode: ExpressionMode::Mixed,
            style: StyleTraits {
                lowercase_tendency: 0.50,
                profanity_rate: 0.10,
                abbreviation_rate: 0.62,
                signature_filler: Some("lol".into()),
                fragment_tendency: 0.72,
                typo_correction_rate: 0.18,
                base_response_delay_secs: 11.0,
            },
            archetype: Archetype::RaidLeader,
        }
    }

    /// Mostly quiet; reacts when directly involved; median 5s lag (fast when they do respond).
    /// Modeled on Zephon corpus profile.
    pub fn reactive_observer() -> Self {
        Self {
            verbosity: 0.18,
            expressiveness: 0.25,
            humor: 0.25,
            positivity: 0.58,
            patience: 0.80,
            tactical_awareness: 0.55,
            social_investment: 0.22,
            expression_mode: ExpressionMode::Mixed,
            style: StyleTraits {
                lowercase_tendency: 0.60,
                profanity_rate: 0.12,
                abbreviation_rate: 0.55,
                signature_filler: None,
                fragment_tendency: 0.80,
                typo_correction_rate: 0.15,
                base_response_delay_secs: 5.0,
            },
            archetype: Archetype::ReactiveObserver,
        }
    }

    /// Weighted random selection — maintains corpus-proportional archetype distribution.
    pub fn random(rng: &mut impl rand::Rng) -> Self {
        match rng.gen_range(0u8..10) {
            0..=2 => Self::quiet_anchor(),
            3..=5 => Self::chaotic_narrator(),
            6..=7 => Self::raid_leader(),
            _ => Self::reactive_observer(),
        }
    }
}
