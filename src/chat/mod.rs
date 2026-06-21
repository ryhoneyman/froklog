/// NPC chat personality and state engine.
///
/// Provides the data model for dynamic, corpus-grounded NPC chat generation:
/// multi-dimensional personality profiles, per-character emotional state
/// machines, a directed relationship graph, and the full trigger taxonomy.
///
/// The loggen binary uses its own simpler chatmod.rs — this module is for
/// the live chat generation system (froklog-chatgen).
pub mod backend;
pub mod context;
pub mod engine;
#[cfg(feature = "neural")]
pub mod neural;
pub mod personality;
pub mod phrasebook;
pub mod relationship;
pub mod state;
pub mod trigger;

pub use backend::{ChatBackend, PhrasebookBackend};
pub use context::{RecentMessage, SituationContext};
pub use engine::{ChatEngine, EmittedMessage, PendingMessage};
#[cfg(feature = "neural")]
pub use neural::NeuralBackend;
pub use personality::{Archetype, ExpressionMode, PersonalityProfile, StyleTraits};
pub use phrasebook::{PhraseDb, PhrasebookFile};
pub use relationship::{RelDynamic, Relationship, RelationshipGraph};
pub use state::{CharacterState, ChatTopic, EmotionalRegister, RlEvent, RlKind};
pub use trigger::{ChatTrigger, ReactionClass};
