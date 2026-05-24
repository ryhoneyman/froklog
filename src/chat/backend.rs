/// `ChatBackend` trait and built-in implementations.
///
/// `ChatBackend` is the single seam that separates phrase generation from the
/// rest of the engine.  Swap implementations to change from phrasebook-based
/// selection to neural inference without touching trigger dispatch,
/// speak-probability gating, or style post-processing.
use std::collections::HashMap;

use rand::rngs::StdRng;

use super::context::SituationContext;
use super::phrasebook::PhraseDb;

// ── Trait ─────────────────────────────────────────────────────────────────────

/// A backend capable of generating a chat message given situational context.
///
/// `generate` returns `None` if the backend has no phrase for this context
/// (e.g. unrecognised trigger kind, low-confidence neural output) or if
/// generation failed.  The engine silently skips the character in that case.
pub trait ChatBackend: Send + Sync {
    fn generate(&mut self, ctx: &SituationContext, rng: &mut StdRng) -> Option<String>;
}

// ── PhrasebookBackend ─────────────────────────────────────────────────────────

/// Wraps the existing `PhraseDb`.  Implements `ChatBackend` using template
/// selection → slot-fill → fragment expansion — the original phrase pipeline.
///
/// This is the default backend.  Pass a `PhraseDb` to `ChatEngine::new()` and
/// this wrapper is constructed automatically.
pub struct PhrasebookBackend(pub PhraseDb);

impl ChatBackend for PhrasebookBackend {
    fn generate(&mut self, ctx: &SituationContext, rng: &mut StdRng) -> Option<String> {
        let raw = self
            .0
            .select(ctx.trigger_kind, &ctx.archetype, &ctx.register, rng)?;
        let slots_ref: HashMap<&str, &str> = ctx
            .slots
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Some(self.0.render(&raw, &slots_ref, rng))
    }
}
