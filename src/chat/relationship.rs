/// Relationship graph for NPC chat characters.
///
/// The key insight from corpus analysis: not all groupmates are equal.
/// Icestorm and Talodar exhibit an "established duo" pattern — asymmetric
/// communication, high trust, comfortable silence from the action-expresser,
/// RL bleed-in permitted without awkwardness. This module encodes that.
use std::collections::HashMap;

// ── Relationship ──────────────────────────────────────────────────────────────

/// The relationship from one character's perspective toward another.
/// Asymmetric by design — A may trust B more than B trusts A.
#[derive(Debug, Clone)]
pub struct Relationship {
    /// General trust level: 0.0 = strangers, 1.0 = close friends.
    pub trust: f32,

    /// Familiarity with their playstyle and patterns.
    /// High familiarity = recognizes their tendencies, can predict their moves.
    pub familiarity: f32,

    /// Current session warmth. Builds with positive interactions,
    /// drops with friction. Resets toward baseline between sessions.
    pub rapport: f32,

    /// The structural dynamic of this relationship.
    pub dynamic: RelDynamic,
}

/// The structural pattern of how two characters relate.
#[derive(Debug, Clone, PartialEq)]
pub enum RelDynamic {
    /// One narrates/acts, the other listens and enables. High trust.
    /// Comfortable silence from the listener is the norm, not a gap to fill.
    /// RL bleed-in from the talker is expected and accepted without comment.
    /// Teasing from either direction reads as warmth, not aggression.
    ///
    /// The Icestorm/Talodar pattern.
    EstablishedDuo,

    /// Standard groupmates who've played together before.
    /// Symmetric expectations; small talk is normal; RL bleed occasional.
    Teammates,

    /// Haven't played together this session before.
    /// Polite, game-focused; no RL bleed; formal courtesy.
    Strangers,

    /// Competitive but cooperative. Each tries to outperform the other
    /// but the dynamic is ultimately functional. Light teasing has an edge.
    Rivals,
}

impl Relationship {
    /// Construct a relationship with defaults matching the dynamic type.
    pub fn new(dynamic: RelDynamic) -> Self {
        let (trust, familiarity, rapport) = match &dynamic {
            RelDynamic::EstablishedDuo => (0.90, 0.90, 0.70),
            RelDynamic::Teammates => (0.55, 0.50, 0.50),
            RelDynamic::Strangers => (0.20, 0.10, 0.30),
            RelDynamic::Rivals => (0.40, 0.72, 0.45),
        };
        Self {
            trust,
            familiarity,
            rapport,
            dynamic,
        }
    }

    // ── Behavioral gates ──────────────────────────────────────────────────────

    /// Whether this relationship permits real-life stories and bleed-in chat.
    /// High trust: yes. Strangers: no.
    pub fn permits_rl_bleed(&self) -> bool {
        self.trust >= 0.60
    }

    /// Whether the listener's silence is comfortable, not a social void to fill.
    /// True for established duos — the action-expresser's quiet IS presence.
    pub fn silence_is_comfortable(&self) -> bool {
        self.dynamic == RelDynamic::EstablishedDuo && self.trust > 0.70
    }

    /// Whether teasing lands as affection rather than aggression.
    pub fn tease_reads_as_warmth(&self) -> bool {
        self.trust > 0.65 && self.familiarity > 0.50
    }

    /// Whether this character would comment on the other's performance
    /// (either praising or ribbing).
    pub fn performance_commentary_ok(&self) -> bool {
        self.familiarity > 0.45 && self.trust > 0.40
    }

    /// Whether this character would react to the other's RL story
    /// (show empathy, ask follow-up, etc.).
    pub fn reacts_to_rl_story(&self) -> bool {
        self.trust > 0.55
    }

    // ── Rapport updates ───────────────────────────────────────────────────────

    pub fn on_positive_interaction(&mut self) {
        self.rapport = (self.rapport + 0.05).min(1.0);
    }

    pub fn on_friction(&mut self) {
        self.rapport = (self.rapport - 0.08).max(0.0);
    }

    /// Between-session decay: rapport drifts toward the trust baseline.
    pub fn session_decay(&mut self) {
        self.rapport += (self.trust - self.rapport) * 0.30;
    }
}

// ── Relationship graph ────────────────────────────────────────────────────────

/// Directed relationship graph for the full group or raid.
/// Key is (from, to) — the perspective of the speaker toward the target.
pub struct RelationshipGraph {
    pairs: HashMap<(String, String), Relationship>,
}

impl RelationshipGraph {
    pub fn new() -> Self {
        Self {
            pairs: HashMap::new(),
        }
    }

    /// Set the relationship from `from` toward `to`.
    pub fn set(&mut self, from: &str, to: &str, rel: Relationship) {
        self.pairs.insert((from.to_string(), to.to_string()), rel);
    }

    /// Get the relationship from `from`'s perspective toward `to`.
    pub fn get(&self, from: &str, to: &str) -> Option<&Relationship> {
        self.pairs.get(&(from.to_string(), to.to_string()))
    }

    pub fn get_mut(&mut self, from: &str, to: &str) -> Option<&mut Relationship> {
        self.pairs.get_mut(&(from.to_string(), to.to_string()))
    }

    /// Establish a symmetric relationship between two characters.
    /// Useful for teammates and strangers where both sides start equal.
    pub fn set_symmetric(&mut self, a: &str, b: &str, dynamic: RelDynamic) {
        let rel_a = Relationship::new(dynamic.clone());
        let rel_b = Relationship::new(dynamic);
        self.set(a, b, rel_a);
        self.set(b, a, rel_b);
    }

    /// Establish an established-duo relationship between talker and listener.
    /// The listener's relationship toward the talker has `silence_is_comfortable`.
    pub fn set_established_duo(&mut self, talker: &str, listener: &str) {
        // Both directions are EstablishedDuo, but the listener side is what
        // gates the silence behavior — checked from the listener's perspective.
        self.set(
            talker,
            listener,
            Relationship::new(RelDynamic::EstablishedDuo),
        );
        self.set(
            listener,
            talker,
            Relationship::new(RelDynamic::EstablishedDuo),
        );
    }

    /// True if `speaker` would permit RL bleed-in when `target` is present.
    /// Uses the weakest trust link in the group if checking against everyone.
    pub fn rl_permitted_toward(&self, speaker: &str, target: &str) -> bool {
        self.get(speaker, target)
            .map(|r| r.permits_rl_bleed())
            .unwrap_or(false)
    }

    /// Average rapport this speaker has across all known relationships.
    pub fn average_rapport(&self, speaker: &str) -> f32 {
        let values: Vec<f32> = self
            .pairs
            .iter()
            .filter(|((s, _), _)| s == speaker)
            .map(|(_, r)| r.rapport)
            .collect();
        if values.is_empty() {
            0.50
        } else {
            values.iter().sum::<f32>() / values.len() as f32
        }
    }

    /// Lowest trust this speaker has toward any group member.
    /// Used to gate RL bleed-in for the whole group (don't share RL stories
    /// if even one stranger is present, unless they don't care).
    pub fn min_trust_toward_others(&self, speaker: &str) -> f32 {
        self.pairs
            .iter()
            .filter(|((s, _), _)| s == speaker)
            .map(|(_, r)| r.trust)
            .fold(1.0f32, f32::min)
    }
}

impl Default for RelationshipGraph {
    fn default() -> Self {
        Self::new()
    }
}
