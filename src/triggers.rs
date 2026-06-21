/// Data-driven trigger engine for the overlay window.
///
/// Triggers are defined in `%APPDATA%\froklog\triggers.toml` (Windows) or
/// `~/.config/froklog/triggers.toml` (other).  The engine is reloaded whenever
/// the config dialog saves changes.
///
/// Two trigger shapes are supported:
///   - Simple  : single pattern → instant overlay event
///   - Chained : multi-step state machine (delay / completion / cancel)
#[cfg(feature = "tray")]
pub mod engine {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use regex::Regex;
    use serde::{Deserialize, Serialize};

    // ── TOML schema ───────────────────────────────────────────────────────────

    fn default_true() -> bool {
        true
    }

    /// A single step inside a chained trigger.
    /// Exactly one of `match_pattern`, `delay_secs`, `complete`, or `cancel` must
    /// be set; serde's untagged discriminant resolves which variant to decode.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum ChainStepDef {
        /// Primary trigger pattern (always the first step in a chain).
        Match {
            #[serde(rename = "match")]
            pattern: String,
            #[serde(default)]
            icon: String,
            #[serde(default)]
            message: String,
            #[serde(default)]
            sound: Option<String>,
        },
        /// Fire after a fixed delay with no log event required.
        Delay {
            delay_secs: f64,
            #[serde(default)]
            icon: String,
            #[serde(default)]
            message: String,
            #[serde(default)]
            sound: Option<String>,
        },
        /// Fire when this pattern is seen, completing the chain.
        Complete {
            complete: String,
            #[serde(default)]
            icon: String,
            #[serde(default)]
            message: String,
            #[serde(default)]
            sound: Option<String>,
        },
        /// Silently cancel the active chain when this pattern is seen.
        Cancel { cancel: String },
    }

    /// Top-level trigger definition stored in triggers.toml.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum TriggerDef {
        Simple {
            name: String,
            #[serde(default = "default_true")]
            enabled: bool,
            #[serde(rename = "match")]
            pattern: String,
            #[serde(default)]
            icon: String,
            #[serde(default)]
            message: String,
            #[serde(default)]
            sound: Option<String>,
        },
        Chained {
            name: String,
            #[serde(default = "default_true")]
            enabled: bool,
            steps: Vec<ChainStepDef>,
        },
    }

    impl TriggerDef {
        pub fn name(&self) -> &str {
            match self {
                TriggerDef::Simple { name, .. } | TriggerDef::Chained { name, .. } => name,
            }
        }

        pub fn enabled(&self) -> bool {
            match self {
                TriggerDef::Simple { enabled, .. } | TriggerDef::Chained { enabled, .. } => {
                    *enabled
                }
            }
        }

        pub fn set_enabled(&mut self, v: bool) {
            match self {
                TriggerDef::Simple { enabled, .. } | TriggerDef::Chained { enabled, .. } => {
                    *enabled = v;
                }
            }
        }
    }

    /// Root document in triggers.toml.
    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    pub struct TriggerConfig {
        #[serde(default, rename = "trigger")]
        pub triggers: Vec<TriggerDef>,
    }

    impl TriggerConfig {
        pub fn load() -> Self {
            let path = triggers_path();
            let Ok(text) = std::fs::read_to_string(&path) else {
                return Self::default();
            };
            toml::from_str(&text).unwrap_or_default()
        }

        pub fn save(&self) {
            let path = triggers_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(text) = toml::to_string_pretty(self) {
                let _ = std::fs::write(path, text);
            }
        }
    }

    pub fn triggers_path() -> PathBuf {
        #[cfg(target_os = "windows")]
        {
            let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
            PathBuf::from(appdata).join("froklog").join("triggers.toml")
        }
        #[cfg(not(target_os = "windows"))]
        {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home)
                .join(".config")
                .join("froklog")
                .join("triggers.toml")
        }
    }

    // ── Runtime types ─────────────────────────────────────────────────────────

    /// An event emitted by the trigger engine to be displayed in the overlay.
    #[derive(Debug, Clone)]
    pub struct OverlayEvent {
        pub icon: String,
        pub message: String,
        pub sound: Option<String>,
    }

    /// Compiled form of a `TriggerDef::Simple`.
    struct CompiledSimple {
        pattern: Regex,
        icon: String,
        message: String,
        sound: Option<String>,
    }

    /// One pending step inside a running chain instance.
    enum PendingStep {
        Delay {
            fire_at: Instant,
            icon: String,
            message: String,
            sound: Option<String>,
        },
        Pattern {
            regex: Regex,
            is_cancel: bool,
            icon: String,
            message: String,
            sound: Option<String>,
        },
    }

    /// A running instance of a chained trigger.
    struct ActiveChain {
        /// Index of the next step to process (everything before this index is done).
        pending: Vec<PendingStep>,
    }

    /// Compiled form of a `TriggerDef::Chained` — just the start pattern + the
    /// full step list for spawning new instances.
    struct CompiledChain {
        start_pattern: Regex,
        start_icon: String,
        start_message: String,
        start_sound: Option<String>,
        steps: Vec<ChainStepDef>,
    }

    // ── Engine ────────────────────────────────────────────────────────────────

    /// The trigger engine.  Cloneable handle backed by a shared inner state.
    #[derive(Clone)]
    pub struct TriggerEngine {
        inner: Arc<Mutex<EngineInner>>,
    }

    struct EngineInner {
        simples: Vec<CompiledSimple>,
        chains: Vec<CompiledChain>,
        active: Vec<ActiveChain>,
        output: Arc<Mutex<Vec<OverlayEvent>>>,
    }

    impl TriggerEngine {
        /// Build a new engine from a loaded `TriggerConfig`.
        pub fn new(cfg: &TriggerConfig, output: Arc<Mutex<Vec<OverlayEvent>>>) -> Self {
            let inner = EngineInner::from_config(cfg, output);
            Self {
                inner: Arc::new(Mutex::new(inner)),
            }
        }

        /// Reload with a fresh config (called when the user saves trigger settings).
        pub fn reload(&self, cfg: &TriggerConfig) {
            let mut g = self.inner.lock().unwrap();
            let output = Arc::clone(&g.output);
            *g = EngineInner::from_config(cfg, output);
        }

        /// Feed a log line into the engine.  Any resulting events are appended to
        /// the shared output queue.
        pub fn process_line(&self, line: &str) {
            self.inner.lock().unwrap().process_line(line);
        }

        /// Advance timer-based steps.  Call on a regular tick (e.g. every 100 ms).
        pub fn tick(&self) {
            self.inner.lock().unwrap().tick();
        }
    }

    impl EngineInner {
        fn from_config(cfg: &TriggerConfig, output: Arc<Mutex<Vec<OverlayEvent>>>) -> Self {
            let mut simples = Vec::new();
            let mut chains = Vec::new();

            for def in &cfg.triggers {
                if !def.enabled() {
                    continue;
                }
                match def {
                    TriggerDef::Simple {
                        pattern,
                        icon,
                        message,
                        sound,
                        ..
                    } => {
                        if let Ok(re) = Regex::new(pattern) {
                            simples.push(CompiledSimple {
                                pattern: re,
                                icon: icon.clone(),
                                message: message.clone(),
                                sound: sound.clone(),
                            });
                        }
                    }
                    TriggerDef::Chained { steps, .. } => {
                        // The first step must be a Match.
                        if let Some(ChainStepDef::Match {
                            pattern,
                            icon,
                            message,
                            sound,
                        }) = steps.first()
                        {
                            if let Ok(re) = Regex::new(pattern) {
                                chains.push(CompiledChain {
                                    start_pattern: re,
                                    start_icon: icon.clone(),
                                    start_message: message.clone(),
                                    start_sound: sound.clone(),
                                    steps: steps[1..].to_vec(),
                                });
                            }
                        }
                    }
                }
            }

            Self {
                simples,
                chains,
                active: Vec::new(),
                output,
            }
        }

        fn emit(&self, icon: &str, message: &str, sound: Option<&str>) {
            let mut q = self.output.lock().unwrap();
            q.push(OverlayEvent {
                icon: icon.to_string(),
                message: message.to_string(),
                sound: sound.map(|s| s.to_string()),
            });
        }

        fn apply_captures(template: &str, caps: &regex::Captures<'_>) -> String {
            let mut out = template.to_string();
            for i in 0..caps.len() {
                let placeholder = format!("{{{i}}}");
                if let Some(m) = caps.get(i) {
                    out = out.replace(&placeholder, m.as_str());
                }
            }
            out
        }

        fn process_line(&mut self, line: &str) {
            // Simple triggers.
            for s in &self.simples {
                if let Some(caps) = s.pattern.captures(line) {
                    let msg = Self::apply_captures(&s.message, &caps);
                    self.emit(&s.icon, &msg, s.sound.as_deref());
                }
            }

            // Chain start triggers — spawn new ActiveChain instances.
            let mut new_chains: Vec<ActiveChain> = Vec::new();
            for c in &self.chains {
                if let Some(caps) = c.start_pattern.captures(line) {
                    let msg = Self::apply_captures(&c.start_message, &caps);
                    self.emit(&c.start_icon, &msg, c.start_sound.as_deref());

                    // Build pending steps for this chain instance.
                    let pending = compile_pending_steps(&c.steps);
                    new_chains.push(ActiveChain { pending });
                }
            }
            self.active.extend(new_chains);

            // Check active chain pending patterns.
            let mut emit_queue: Vec<OverlayEvent> = Vec::new();
            let mut keep = Vec::with_capacity(self.active.len());

            'outer: for mut chain in self.active.drain(..) {
                // Scan the pending list for pattern steps that could fire on this line.
                let mut fired_idx = None;
                for (i, step) in chain.pending.iter().enumerate() {
                    if let PendingStep::Pattern {
                        regex,
                        is_cancel,
                        icon,
                        message,
                        sound,
                    } = step
                    {
                        if regex.is_match(line) {
                            if *is_cancel {
                                // Discard chain.
                                continue 'outer;
                            }
                            emit_queue.push(OverlayEvent {
                                icon: icon.clone(),
                                message: message.clone(),
                                sound: sound.clone(),
                            });
                            fired_idx = Some(i);
                            break;
                        }
                    }
                }

                if let Some(idx) = fired_idx {
                    // Remove everything up to and including the fired step.
                    chain.pending.drain(..=idx);
                    if !chain.pending.is_empty() {
                        keep.push(chain);
                    }
                } else {
                    keep.push(chain);
                }
            }

            self.active = keep;

            for ev in emit_queue {
                let mut q = self.output.lock().unwrap();
                q.push(ev);
            }
        }

        fn tick(&mut self) {
            let now = Instant::now();
            let mut emit_queue: Vec<OverlayEvent> = Vec::new();
            let mut keep = Vec::with_capacity(self.active.len());

            for mut chain in self.active.drain(..) {
                // Fire any delay steps whose time has come (may be multiple if stacked).
                loop {
                    match chain.pending.first() {
                        Some(PendingStep::Delay {
                            fire_at,
                            icon,
                            message,
                            sound,
                        }) => {
                            if now >= *fire_at {
                                emit_queue.push(OverlayEvent {
                                    icon: icon.clone(),
                                    message: message.clone(),
                                    sound: sound.clone(),
                                });
                                chain.pending.remove(0);
                            } else {
                                break;
                            }
                        }
                        _ => break,
                    }
                }

                if !chain.pending.is_empty() {
                    keep.push(chain);
                }
            }

            self.active = keep;

            for ev in emit_queue {
                let mut q = self.output.lock().unwrap();
                q.push(ev);
            }
        }
    }

    fn compile_pending_steps(steps: &[ChainStepDef]) -> Vec<PendingStep> {
        let mut out = Vec::new();
        let now = Instant::now();

        // We accumulate delay offsets so that stacked delays work correctly.
        let mut delay_offset = Duration::ZERO;

        for step in steps {
            match step {
                ChainStepDef::Delay {
                    delay_secs,
                    icon,
                    message,
                    sound,
                } => {
                    delay_offset += Duration::from_secs_f64(*delay_secs);
                    out.push(PendingStep::Delay {
                        fire_at: now + delay_offset,
                        icon: icon.clone(),
                        message: message.clone(),
                        sound: sound.clone(),
                    });
                }
                ChainStepDef::Complete {
                    complete,
                    icon,
                    message,
                    sound,
                } => {
                    if let Ok(re) = Regex::new(complete) {
                        out.push(PendingStep::Pattern {
                            regex: re,
                            is_cancel: false,
                            icon: icon.clone(),
                            message: message.clone(),
                            sound: sound.clone(),
                        });
                    }
                }
                ChainStepDef::Cancel { cancel } => {
                    if let Ok(re) = Regex::new(cancel) {
                        out.push(PendingStep::Pattern {
                            regex: re,
                            is_cancel: true,
                            icon: String::new(),
                            message: String::new(),
                            sound: None,
                        });
                    }
                }
                ChainStepDef::Match { .. } => {
                    // Match steps only valid as the first step; ignore here.
                }
            }
        }

        out
    }
}
