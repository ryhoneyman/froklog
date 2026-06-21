/// `NeuralBackend` — tract-onnx powered chat generation.
///
/// Gated on the `neural` Cargo feature.  When enabled, `froklog-loggen` will
/// probe for `model.onnx` / `vocab.model` / `model_meta.json` at startup and,
/// if found, use this backend instead of `PhrasebookBackend`.
///
/// Model interface (see scripts/export_onnx.py):
///   Input  tokens  : int64  [1, seq_len]   (dynamic axis)
///   Output logits  : f32    [1, seq_len, vocab_size]
///
/// Inference is autoregressive — we call the model once per new token until
/// EOS or `max_new_tokens` is reached, slicing the last logit row each time.
#[cfg(feature = "neural")]
pub mod neural_impl {
    use std::collections::HashMap;
    use std::path::Path;

    use rand::rngs::StdRng;
    use rand::Rng;
    use sentencepiece_rs::SentencePieceProcessor;
    use tract_onnx::prelude::*;

    use crate::chat::backend::ChatBackend;
    use crate::chat::context::SituationContext;
    use crate::chat::personality::Archetype;
    use crate::chat::state::EmotionalRegister;

    // ── Runtime metadata ──────────────────────────────────────────────────────

    /// Mirrors `model_meta.json` produced by `scripts/export_onnx.py`.
    #[derive(Clone, Debug)]
    pub struct ModelMeta {
        pub vocab_size: usize,
        /// How many ids belong to the base SentencePiece vocabulary.
        pub base_vocab: usize,
        /// Ordered list of control token names (matches `build_dataset.CONTROL_TOKENS`).
        pub control_tokens: Vec<String>,
        pub max_len: usize,
        pub pad_id: i64,
        pub bos_id: i64,
        pub eos_id: i64,
        /// `control_token_name -> id`
        pub ctrl: HashMap<String, i64>,
    }

    impl ModelMeta {
        pub fn from_json(text: &str) -> Result<Self, String> {
            let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;

            let base_vocab = v["base_vocab"].as_u64().ok_or("missing base_vocab")? as usize;
            let ctrl_obj = v["control_tokens"]
                .as_object()
                .ok_or("missing control_tokens")?;
            let ctrl: HashMap<String, i64> = ctrl_obj
                .iter()
                .filter_map(|(k, v)| v.as_i64().map(|id| (k.clone(), id)))
                .collect();
            let control_tokens: Vec<String> = ctrl_obj.keys().cloned().collect();

            Ok(Self {
                vocab_size: v["vocab_size"].as_u64().ok_or("missing vocab_size")? as usize,
                base_vocab,
                control_tokens,
                max_len: v["max_len"].as_u64().ok_or("missing max_len")? as usize,
                pad_id: v["pad_id"].as_i64().ok_or("missing pad_id")?,
                bos_id: v["bos_id"].as_i64().ok_or("missing bos_id")?,
                eos_id: v["eos_id"].as_i64().ok_or("missing eos_id")?,
                ctrl,
            })
        }
    }

    // ── Control token helpers ─────────────────────────────────────────────────

    fn archetype_tag(arch: &Archetype) -> &'static str {
        match arch {
            Archetype::QuietAnchor => "QuietAnchor",
            Archetype::ChaoticNarrator => "ChaoticNarrator",
            Archetype::RaidLeader => "RaidLeader",
            Archetype::ReactiveObserver => "ReactiveObserver",
            Archetype::TacticalFocused => "TacticalFocused",
            Archetype::Custom => "Generic",
        }
    }

    fn register_tag(reg: &EmotionalRegister) -> &'static str {
        match reg {
            EmotionalRegister::Neutral => "Neutral",
            EmotionalRegister::Engaged => "Engaged",
            EmotionalRegister::Elated => "Elated",
            EmotionalRegister::Frustrated => "Frustrated",
            EmotionalRegister::Tired => "Tired",
        }
    }

    /// Return the most contextually useful slot value for the model to condition on.
    /// Priority: mob/target entity names first, then spell names, then actor names.
    fn pick_primary_slot<'a>(slots: &'a HashMap<String, String>) -> Option<&'a str> {
        for key in &[
            "mob", "player", "healer", "target", "spell", "ability", "caster", "src",
        ] {
            if let Some(v) = slots.get(*key) {
                return Some(v.as_str());
            }
        }
        None
    }

    /// Build the prefix token sequence:
    /// `[ARCH] <arch> [REG] <reg> [TRIG] <trig> [personality…] <slot_text> [RESPONSE]`
    ///
    /// Personality bucket tokens and SP-encoded slot text are appended when
    /// the relevant control tokens exist in `meta.ctrl` (graceful degradation
    /// against models trained without them).
    fn build_prefix(
        meta: &ModelMeta,
        sp: &SentencePieceProcessor,
        ctx: &SituationContext,
    ) -> Vec<i64> {
        let arch_tag = format!("[{}]", archetype_tag(&ctx.archetype));
        let reg_tag = format!("[{}]", register_tag(&ctx.register));
        let trig_tag = format!("[{}]", ctx.trigger_kind.replace(' ', "_"));

        let mut ids = Vec::with_capacity(16);
        for tag in &[
            "[ARCH]",
            arch_tag.as_str(),
            "[REG]",
            reg_tag.as_str(),
            "[TRIG]",
            trig_tag.as_str(),
        ] {
            if let Some(&id) = meta.ctrl.get(*tag) {
                ids.push(id);
            }
        }

        // Personality bucket tokens (thresholds mirror build_dataset.py build_prefix_ids)
        let verbosity_tag = if ctx.verbosity > 0.55 {
            Some("[VERBOSE]")
        } else if ctx.verbosity < 0.40 {
            Some("[TERSE]")
        } else {
            None
        };
        let humor_tag = if ctx.humor > 0.55 {
            Some("[HUMOROUS]")
        } else if ctx.humor < 0.35 {
            Some("[SERIOUS]")
        } else {
            None
        };
        let patience_tag = if ctx.patience > 0.65 {
            Some("[PATIENT]")
        } else if ctx.patience < 0.50 {
            Some("[IMPATIENT]")
        } else {
            None
        };
        for opt_tag in [verbosity_tag, humor_tag, patience_tag] {
            if let Some(tag) = opt_tag {
                if let Some(&id) = meta.ctrl.get(tag) {
                    ids.push(id);
                }
            }
        }

        // SP-encode the primary slot entity name so the model can reference it
        if let Some(slot_val) = pick_primary_slot(&ctx.slots) {
            if let Ok(sp_ids) = sp.encode_to_ids(slot_val) {
                for sp_id in sp_ids {
                    ids.push(sp_id as i64);
                }
            }
        }

        // [RESPONSE] delimiter — everything after this is generated
        if let Some(&id) = meta.ctrl.get("[RESPONSE]") {
            ids.push(id);
        }

        ids
    }

    // ── Top-k sampling ────────────────────────────────────────────────────────

    fn sample_top_k(logits: &[f32], temperature: f32, top_k: usize, rng: &mut StdRng) -> i64 {
        assert!(!logits.is_empty());

        let mut pairs: Vec<(usize, f32)> = logits
            .iter()
            .enumerate()
            .map(|(i, &l)| (i, l / temperature))
            .collect();

        pairs.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        pairs.truncate(top_k.max(1));

        let max_l = pairs[0].1;
        let exps: Vec<f32> = pairs.iter().map(|(_, l)| (*l - max_l).exp()).collect();
        let sum: f32 = exps.iter().sum();
        let probs: Vec<f32> = exps.iter().map(|e| e / sum).collect();

        let r: f32 = rng.gen();
        let mut cum = 0.0f32;
        for (prob, (tok_idx, _)) in probs.iter().zip(pairs.iter()) {
            cum += prob;
            if r <= cum {
                return *tok_idx as i64;
            }
        }
        pairs[0].0 as i64
    }

    // ── Tract model type alias ────────────────────────────────────────────────

    type TractPlan = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

    // ── Backend ───────────────────────────────────────────────────────────────

    /// ONNX-backed chat generation via tract-onnx (pure Rust, no AVX2 required).
    pub struct NeuralBackend {
        model: TractPlan,
        sp: SentencePieceProcessor,
        meta: ModelMeta,
        temperature: f32,
        top_k: usize,
        max_new: usize,
    }

    impl NeuralBackend {
        /// Load the backend from a directory containing:
        ///   - `model.onnx`
        ///   - `vocab.model`  (SentencePiece binary model)
        ///   - `model_meta.json`
        pub fn from_dir(dir: &Path) -> Result<Self, String> {
            let model_path = dir.join("model.onnx");
            let vocab_path = dir.join("vocab.model");
            let meta_path = dir.join("model_meta.json");

            let meta_text = std::fs::read_to_string(&meta_path)
                .map_err(|e| format!("cannot read {}: {e}", meta_path.display()))?;
            let meta = ModelMeta::from_json(&meta_text)?;

            let model = tract_onnx::onnx()
                .model_for_path(&model_path)
                .map_err(|e| format!("cannot load {}: {e}", model_path.display()))?
                .into_optimized()
                .map_err(|e| e.to_string())?
                .into_runnable()
                .map_err(|e| e.to_string())?;

            let sp = SentencePieceProcessor::open(&vocab_path)
                .map_err(|e| format!("cannot load {}: {e}", vocab_path.display()))?;

            Ok(Self {
                model,
                sp,
                meta,
                temperature: 0.9,
                top_k: 40,
                max_new: 64,
            })
        }

        /// Adjust generation hyper-parameters at runtime.
        pub fn set_params(&mut self, temperature: f32, top_k: usize, max_new: usize) {
            self.temperature = temperature;
            self.top_k = top_k;
            self.max_new = max_new;
        }

        fn run_forward(&mut self, token_ids: &[i64]) -> Result<Vec<f32>, String> {
            let seq_len = token_ids.len();

            // Build a [1, seq_len] i64 tensor via tract's bundled ndarray.
            let arr = tract_ndarray::Array2::from_shape_vec((1usize, seq_len), token_ids.to_vec())
                .map_err(|e| e.to_string())?;

            let tensor: Tensor = arr.into();
            let outputs = self
                .model
                .run(tvec![tensor.into()])
                .map_err(|e| e.to_string())?;

            // logits: [1, seq_len, vocab_size] — extract last position row.
            let view = outputs[0]
                .to_array_view::<f32>()
                .map_err(|e| e.to_string())?;

            let vocab_size = self.meta.vocab_size;
            let flat = view
                .as_slice_memory_order()
                .ok_or_else(|| "logits tensor is not contiguous".to_string())?;
            let offset = (seq_len - 1) * vocab_size;
            Ok(flat[offset..offset + vocab_size].to_vec())
        }
    }

    impl ChatBackend for NeuralBackend {
        fn generate(&mut self, ctx: &SituationContext, rng: &mut StdRng) -> Option<String> {
            let mut ids = build_prefix(&self.meta, &self.sp, ctx);
            tracing::debug!(
                trigger_kind  = ctx.trigger_kind,
                archetype     = archetype_tag(&ctx.archetype),
                register      = register_tag(&ctx.register),
                verbosity     = ctx.verbosity,
                humor         = ctx.humor,
                patience      = ctx.patience,
                primary_slot  = ?pick_primary_slot(&ctx.slots),
                prefix_len    = ids.len(),
                prefix_ids    = ?ids,
                temperature   = self.temperature,
                top_k         = self.top_k,
                max_new       = self.max_new,
                "NeuralBackend prefix built"
            );
            if ids.is_empty() {
                tracing::warn!("NeuralBackend: no control tokens matched — skipping");
                return None;
            }

            let eos = self.meta.eos_id;
            let pad = self.meta.pad_id;
            let max = self.meta.max_len.min(ids.len() + self.max_new);

            let mut response_ids: Vec<i64> = Vec::new();

            while ids.len() < max {
                let logits = match self.run_forward(&ids) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("NeuralBackend inference error: {e}");
                        return None;
                    }
                };

                let next = sample_top_k(&logits, self.temperature, self.top_k, rng);
                if next == eos || next == pad {
                    break;
                }
                ids.push(next);
                response_ids.push(next);
            }

            tracing::debug!(
                response_token_count = response_ids.len(),
                hit_eos = response_ids.last().copied() == Some(eos)
                    || response_ids.last().copied() == Some(self.meta.pad_id),
                "NeuralBackend generation complete"
            );
            if response_ids.is_empty() {
                return None;
            }

            let sp_ids: Vec<usize> = response_ids.iter().map(|&x| x as usize).collect();
            match self.sp.decode_ids(&sp_ids) {
                Ok(text) => {
                    let t = text.trim().to_owned();
                    tracing::debug!(decoded = %t, "NeuralBackend decoded output");
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                }
                Err(e) => {
                    tracing::warn!("NeuralBackend decode error: {e}");
                    None
                }
            }
        }
    }
}

// Re-export so callers can write `use froklog::chat::NeuralBackend` when
// the `neural` feature is active.
#[cfg(feature = "neural")]
pub use neural_impl::NeuralBackend;
