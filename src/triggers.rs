/// Data-driven trigger engine for the overlay window.
///
/// Triggers are defined in `%APPDATA%\froklog\triggers.toml` (Windows) or
/// `~/.config/froklog/triggers.toml` (other).  The engine is reloaded whenever
/// the config dialog saves changes.
///
/// Each trigger has:
///   - Zero or more Conditions evaluated with ALL or ANY logic
///   - One or more Actions executed when the trigger fires
///
/// Condition types:
///   - match : compare the log line with an exact string, regex, or glob pattern
///   - var   : test a value with isset/equals/gt/gte/lt/lte/matches. `var_name`
///     resolves against a capture group from an earlier Match condition *in the
///     same trigger* first, then falls back to a persisted `store_var` variable —
///     so a Regex condition can capture a number and a following Var condition
///     can Gt/Lt-compare it immediately, with no `store_var` action needed.
///
/// Action types:
///   - overlay   : emit a message to the overlay window (icon, color, delay)
///   - store_var : write a variable (value may reference capture groups)
#[cfg(feature = "triggers")]
pub mod engine {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use once_cell::sync::Lazy;
    use regex::Regex;
    use regex_syntax::ast::{Ast, GroupKind};
    use serde::{Deserialize, Serialize};

    // ── TOML schema ───────────────────────────────────────────────────────────

    fn default_true() -> bool {
        true
    }

    /// How a log line is matched in a Match condition.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum MatchType {
        /// Literal substring — line must contain the pattern string verbatim.
        Exact,
        /// Standard Rust regex.  Capture groups `(…)` give `{1}`, `{2}` …
        #[default]
        Regex,
        /// Shell-style glob.  `*` = any chars, `?` = one char,
        /// `{name}` = named capture (usable as `{name}` in action templates).
        Glob,
    }

    /// Comparison operator for a Var condition.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum VarOp {
        /// Variable has been stored (value field ignored).
        #[default]
        Isset,
        /// String equality (case-insensitive).
        Equals,
        /// Numeric greater-than.
        Gt,
        /// Numeric greater-than-or-equal.
        Gte,
        /// Numeric less-than.
        Lt,
        /// Numeric less-than-or-equal.
        Lte,
        /// Variable value matches a regex pattern.
        Matches,
    }

    /// Whether ALL or ANY conditions must pass for a trigger to fire.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum ConditionLogic {
        /// Every condition must pass (AND).
        #[default]
        All,
        /// At least one condition must pass (OR).
        Any,
    }

    /// Which chat channel a `Condition::Chat` should match.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum ChatChannel {
        /// Any chat line, on any channel.
        #[default]
        Any,
        Say,
        /// Both directions: a tell you received ("X tells you, ...") and one
        /// you sent ("You told X, ...").
        Tell,
        /// Out-of-character chat ("says out of character").
        Ooc,
        Shout,
        Guild,
        /// Group/party chat — EQ Legends and classic EQ both use "party" or
        /// "group" here depending on version, so both are accepted.
        Group,
        Raid,
        Auction,
        /// A channel not covered above (a numbered custom channel like
        /// "General:1", or a server-specific one). Matched by substring
        /// against `Condition::Chat`'s `custom_channel` field.
        Custom,
    }

    /// A single trigger condition stored in triggers.toml.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Condition {
        /// Test the incoming log line.
        Match {
            #[serde(default)]
            match_type: MatchType,
            #[serde(default)]
            pattern: String,
        },
        /// Test a stored variable.
        Var {
            #[serde(default)]
            var_name: String,
            #[serde(default)]
            op: VarOp,
            /// Comparison value (ignored for `isset`).
            #[serde(default)]
            value: String,
        },
        /// Test a chat line: which channel carried it, and (optionally) a
        /// pattern matched against just the spoken message — never the raw
        /// log line. Scoping the pattern to the extracted message, and the
        /// channel to the line's own verb structure (see
        /// `crate::patterns::RE_CHAT`), means a pattern configured for one
        /// channel can't be satisfied by text someone else quoted inside a
        /// *different* channel's message — the classic risk with a raw
        /// substring/regex `Match` condition on chat-shaped lines.
        Chat {
            #[serde(default)]
            channel: ChatChannel,
            /// Only consulted when `channel == Custom`: a substring to look
            /// for in the channel's own log text (e.g. "General:1"). Empty
            /// never matches.
            #[serde(default)]
            custom_channel: String,
            #[serde(default)]
            match_type: MatchType,
            /// Matched against the message only. An empty pattern with
            /// `match_type: Exact` matches any message on the channel.
            #[serde(default)]
            pattern: String,
        },
    }

    /// Priority level for a VoiceAlert action, controlling queue behaviour.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum VoicePriority {
        /// Cuts any currently playing audio immediately to speak this alert.
        Emergency,
        /// Queues speech after the current audio finishes.
        #[default]
        Operational,
        /// Suppressed entirely if any audio is currently playing.
        Ambient,
    }

    /// How to pick among multiple sounds on a `PlaySound` action that fires more
    /// than once (only meaningful when `sounds` has 2+ entries).
    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum SoundMode {
        /// Pick a random sound from the list on every firing.
        #[default]
        Random,
        /// Cycle through the list in order, wrapping back to the start.
        Sequential,
    }

    /// Visual treatment applied to an overlay message while it's held at max size.
    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum Treatment {
        /// No special effect.
        #[default]
        None,
        /// Soft expanding halo behind the text.
        Glow,
        /// Small jittering x/y offset.
        Vibrate,
        /// Gentle scale oscillation around the peak size.
        Pulse,
    }

    /// A single action executed when a trigger fires.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Action {
        /// Show a message in the overlay.
        Overlay {
            /// Built-in icon key: "heal", "damage", "warn", "spell", "info" or "".
            #[serde(default)]
            icon: String,
            /// Optional hex colour override for the icon swatch, e.g. "#FF4400".
            #[serde(default)]
            color: String,
            /// Message text; supports `{1}`, `{name}` placeholders.
            #[serde(default)]
            message: String,
            /// Optional hex colour for the message text, e.g. "#FFDD44".  Empty = default white.
            #[serde(default)]
            message_color: String,
            /// Optional hex colour for the text's stroke/outline, e.g. "#000000".
            /// Empty = default black.
            #[serde(default)]
            border_color: String,
            /// Seconds to wait before showing (0 = immediate).
            #[serde(default)]
            delay_secs: f64,
            /// Visual treatment applied while the message is held at max size.
            #[serde(default)]
            treatment: Treatment,
            /// Queue priority: `emergency` interrupts whatever's currently showing and
            /// jumps to the front; `operational` (default) queues normally; `ambient`
            /// queues normally too but is dropped if the queue is already backed up.
            #[serde(default)]
            priority: VoicePriority,
        },
        /// Speak a message via the system TTS engine.
        VoiceAlert {
            /// Text to speak; supports `{1}`, `{name}` placeholders.
            #[serde(default)]
            tts_text: String,
            /// Playback priority: emergency cuts current audio, operational queues,
            /// ambient is suppressed if audio is already playing.
            #[serde(default)]
            priority: VoicePriority,
        },
        /// Play a sound file only — no visual overlay card, no TTS.
        PlaySound {
            /// Deprecated single-sound field, superseded by `sounds`. Only read
            /// by `migrate_legacy_sounds` to upgrade old configs on load; the
            /// editor never writes it and it's dropped from newly-saved files.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            sound: Option<String>,
            /// One or more sound label names (see `sound_packages`), resolved
            /// through whichever package is currently active. When there's more
            /// than one, `mode` decides how one is picked per firing.
            #[serde(default)]
            sounds: Vec<String>,
            /// How to choose among `sounds` when there's more than one.
            #[serde(default)]
            mode: SoundMode,
            /// Seconds to wait before playing (0 = immediate).
            #[serde(default)]
            delay_secs: f64,
        },
        /// Write a value to a named variable.
        StoreVar {
            #[serde(default)]
            var_name: String,
            /// Value template; supports `{1}`, `{name}` placeholders.
            #[serde(default)]
            value: String,
        },
    }

    /// One trigger definition as stored in triggers.toml.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TriggerDef {
        pub name: String,
        #[serde(default = "default_true")]
        pub enabled: bool,
        #[serde(default)]
        pub condition_logic: ConditionLogic,
        #[serde(default, rename = "condition")]
        pub conditions: Vec<Condition>,
        #[serde(default, rename = "action")]
        pub actions: Vec<Action>,
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
            let mut cfg: Self = toml::from_str(&text).unwrap_or_default();
            if migrate_legacy_sounds(&mut cfg) {
                cfg.save();
            }
            cfg
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

    /// Converts any `Action::PlaySound` still storing a raw path (from before
    /// sound packages existed) into a label, registering it in the `default`
    /// package if needed, and upgrades the old single-sound `sound` field into
    /// the new multi-sound `sounds` list. Returns whether anything changed, so
    /// the caller knows to persist the migrated file. Idempotent: migrated
    /// values are bare labels with no `/`/`\`, and `sound` is always cleared
    /// once folded into `sounds`, so a second pass is always a no-op.
    fn migrate_legacy_sounds(cfg: &mut TriggerConfig) -> bool {
        let mut changed = false;
        for def in &mut cfg.triggers {
            for action in &mut def.actions {
                if let Action::PlaySound { sound, sounds, .. } = action {
                    if let Some(s) = sound {
                        if s.contains('/') || s.contains('\\') {
                            if let Some(label) =
                                crate::sound_packages::sound_packages::migrate_legacy_sound_value(s)
                            {
                                *s = label;
                            }
                        }
                        if !s.is_empty() {
                            sounds.push(s.clone());
                        }
                        *sound = None;
                        changed = true;
                    }
                }
            }
        }
        changed
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

    // ── Runtime event type ────────────────────────────────────────────────────

    /// An event emitted by the trigger engine to be displayed in the overlay.
    #[derive(Debug, Clone)]
    pub struct OverlayEvent {
        pub icon: String,
        /// Optional hex colour override for the icon swatch (e.g. "#FF4400"), or "".
        pub color: String,
        pub message: String,
        /// Optional hex colour for the message text (e.g. "#FFDD44"), or "".
        pub message_color: String,
        /// Optional hex colour for the text's stroke/outline (e.g. "#000000"), or "".
        pub border_color: String,
        pub sound: Option<String>,
        /// Text to speak via TTS, or None for visual-only events.
        pub tts_text: Option<String>,
        /// Priority for TTS playback (controls interrupt / queue / suppress behaviour).
        pub tts_priority: VoicePriority,
        /// Visual treatment applied while the message is held at max size.
        pub treatment: Treatment,
        /// Visual queue priority — see `Action::Overlay`'s `priority` field.
        pub priority: VoicePriority,
    }

    // ── Compiled runtime types ────────────────────────────────────────────────

    /// The compiled form of a match-type condition.
    enum CompiledMatch {
        Exact(String),
        Regex(Regex),
    }

    struct CompiledCondition {
        original: Condition,
        /// Compiled pattern — only Some for Match conditions.
        compiled: Option<CompiledMatch>,
    }

    struct CompiledTrigger {
        name: String,
        logic: ConditionLogic,
        conditions: Vec<CompiledCondition>,
        actions: Vec<Action>,
        /// Sequential-mode cursor per action index, meaningful only for
        /// `Action::PlaySound` actions using `SoundMode::Sequential`. `Cell`
        /// lets `process_line` advance it while only holding `&self.triggers`.
        /// Reset to 0 whenever the engine reloads (matching the answer that
        /// sequential position doesn't need to survive a config reload).
        sound_seq: Vec<Cell<usize>>,
    }

    /// Resolved capture groups from a matched condition, owned strings.
    #[derive(Default)]
    struct CaptureMap {
        /// Positional groups: index 0 = full match, 1+ = capture groups.
        positional: Vec<Option<String>>,
        /// Named capture groups.
        named: HashMap<String, String>,
    }

    struct PendingAction {
        fire_at: Instant,
        event: OverlayEvent,
    }

    struct EngineInner {
        triggers: Vec<CompiledTrigger>,
        vars: HashMap<String, String>,
        pending: Vec<PendingAction>,
        output: Arc<Mutex<Vec<OverlayEvent>>>,
        lines_processed: u64,
    }

    // ── Template placeholder regex ────────────────────────────────────────────

    /// Matches `{something}` in action templates.
    static PLACEHOLDER: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{([^}]+)\}").unwrap());

    // ── Public engine handle ──────────────────────────────────────────────────

    #[derive(Clone)]
    pub struct TriggerEngine {
        inner: Arc<Mutex<EngineInner>>,
    }

    impl TriggerEngine {
        pub fn new(cfg: &TriggerConfig, output: Arc<Mutex<Vec<OverlayEvent>>>) -> Self {
            Self {
                inner: Arc::new(Mutex::new(EngineInner::from_config(cfg, output))),
            }
        }

        /// Reload with a fresh config (called when the user saves trigger settings).
        pub fn reload(&self, cfg: &TriggerConfig) {
            let mut g = self.inner.lock().unwrap();
            let output = Arc::clone(&g.output);
            *g = EngineInner::from_config(cfg, output);
        }

        /// Feed a log line into the engine.
        pub fn process_line(&self, line: &str) {
            self.inner.lock().unwrap().process_line(line);
        }

        /// Advance timer-based actions.  Call on a regular tick (e.g. every 100 ms).
        pub fn tick(&self) {
            self.inner.lock().unwrap().tick();
        }

        /// Fires `actions` immediately, bypassing condition evaluation — used
        /// by the Triggers tab's Test button to preview a trigger's actions.
        pub fn fire_actions_for_test(&self, actions: &[Action]) {
            self.inner.lock().unwrap().fire_actions_for_test(actions);
        }
    }

    // ── Engine inner ──────────────────────────────────────────────────────────

    impl EngineInner {
        fn from_config(cfg: &TriggerConfig, output: Arc<Mutex<Vec<OverlayEvent>>>) -> Self {
            let mut triggers = Vec::new();
            for def in &cfg.triggers {
                if !def.enabled {
                    continue;
                }
                let conditions = def.conditions.iter().map(compile_condition).collect();
                let sound_seq = def.actions.iter().map(|_| Cell::new(0)).collect();
                triggers.push(CompiledTrigger {
                    name: def.name.clone(),
                    logic: def.condition_logic.clone(),
                    conditions,
                    actions: def.actions.clone(),
                    sound_seq,
                });
            }
            tracing::info!(
                "trigger engine: loaded {} enabled trigger(s) ({} total in triggers.toml)",
                triggers.len(),
                cfg.triggers.len()
            );
            Self {
                triggers,
                vars: HashMap::new(),
                pending: Vec::new(),
                output,
                lines_processed: 0,
            }
        }

        fn process_line(&mut self, line: &str) {
            self.lines_processed += 1;
            if self.lines_processed.is_multiple_of(500) {
                tracing::info!(
                    "trigger engine: {} lines processed so far, {} trigger(s) loaded (last line: {:?})",
                    self.lines_processed,
                    self.triggers.len(),
                    line
                );
            }

            let now = Instant::now();
            let mut new_events: Vec<OverlayEvent> = Vec::new();
            let mut new_pending: Vec<PendingAction> = Vec::new();

            for trigger in &self.triggers {
                // Evaluate all conditions and collect captures from Match ones.
                let mut results: Vec<bool> = Vec::with_capacity(trigger.conditions.len());
                let mut caps = CaptureMap::default();
                let mut caps_set = false;

                for cond in &trigger.conditions {
                    // `caps` reflects only conditions evaluated *so far* in this
                    // trigger, so a Var condition can reference a capture group
                    // from an earlier Match/Regex condition in the same list
                    // (e.g. regex-capture a kick's damage, then Gt-compare it)
                    // without needing an intermediate `store_var` action.
                    let (passed, maybe_caps) = eval_condition(cond, line, &self.vars, &caps);
                    results.push(passed);
                    if passed {
                        if let Some(c) = maybe_caps {
                            if !caps_set {
                                // First match condition's positional groups win.
                                caps.positional = c.positional;
                                caps_set = true;
                            }
                            // Named captures from all matching conditions are merged.
                            caps.named.extend(c.named);
                        }
                    }
                }

                // Apply ALL / ANY logic.  An empty condition list always fires.
                let fired = match trigger.logic {
                    ConditionLogic::All => results.iter().all(|&b| b),
                    ConditionLogic::Any => results.is_empty() || results.iter().any(|&b| b),
                };
                if !fired {
                    continue;
                }
                tracing::info!(
                    "trigger engine: \"{}\" fired on line: {:?}",
                    trigger.name,
                    line
                );

                let (events, pending) = execute_actions(
                    &trigger.actions,
                    &trigger.sound_seq,
                    &caps,
                    &mut self.vars,
                    now,
                );
                new_events.extend(events);
                new_pending.extend(pending);
            }

            self.pending.extend(new_pending);
            if !new_events.is_empty() {
                let mut q = self.output.lock().unwrap();
                q.extend(new_events);
            }
        }

        /// Fires `actions` immediately, bypassing condition evaluation entirely.
        /// Backs the Triggers tab's Test button, so it works even for a
        /// currently-disabled trigger being edited.
        fn fire_actions_for_test(&mut self, actions: &[Action]) {
            let now = Instant::now();
            let caps = CaptureMap::default();
            let sound_seq: Vec<Cell<usize>> = actions.iter().map(|_| Cell::new(0)).collect();
            let (events, pending) =
                execute_actions(actions, &sound_seq, &caps, &mut self.vars, now);
            tracing::info!(
                "test trigger: produced {} immediate event(s), {} pending (delayed) action(s)",
                events.len(),
                pending.len()
            );
            self.pending.extend(pending);
            if !events.is_empty() {
                self.output.lock().unwrap().extend(events);
            }
        }

        fn tick(&mut self) {
            let now = Instant::now();
            let mut fired: Vec<OverlayEvent> = Vec::new();
            self.pending.retain(|p| {
                if now >= p.fire_at {
                    fired.push(p.event.clone());
                    false
                } else {
                    true
                }
            });
            if !fired.is_empty() {
                let mut q = self.output.lock().unwrap();
                q.extend(fired);
            }
        }
    }

    /// Executes `actions` in order, resolving `{...}` templates against `caps`/
    /// `vars` and honouring each action's `delay_secs`. Shared by `process_line`
    /// (actions gated on a trigger's conditions matching) and
    /// `fire_actions_for_test` (actions fired unconditionally by the Triggers
    /// tab's Test button), so both stay in sync.
    fn execute_actions(
        actions: &[Action],
        sound_seq: &[Cell<usize>],
        caps: &CaptureMap,
        vars: &mut HashMap<String, String>,
        now: Instant,
    ) -> (Vec<OverlayEvent>, Vec<PendingAction>) {
        let mut new_events = Vec::new();
        let mut new_pending = Vec::new();
        for (action_idx, action) in actions.iter().enumerate() {
            match action {
                Action::StoreVar { var_name, value } => {
                    if !var_name.is_empty() {
                        let resolved = resolve_template(value, caps, vars);
                        vars.insert(var_name.clone(), resolved);
                    }
                }
                Action::VoiceAlert { tts_text, priority } => {
                    let text = resolve_template(tts_text, caps, vars);
                    if !text.is_empty() {
                        new_events.push(OverlayEvent {
                            icon: String::new(),
                            color: String::new(),
                            message: String::new(),
                            message_color: String::new(),
                            border_color: String::new(),
                            sound: None,
                            tts_text: Some(text),
                            tts_priority: priority.clone(),
                            treatment: Treatment::default(),
                            priority: VoicePriority::default(),
                        });
                    }
                }
                Action::Overlay {
                    icon,
                    color,
                    message,
                    message_color,
                    border_color,
                    delay_secs,
                    treatment,
                    priority,
                } => {
                    let event = OverlayEvent {
                        icon: icon.clone(),
                        color: color.clone(),
                        message: resolve_template(message, caps, vars),
                        message_color: message_color.clone(),
                        border_color: border_color.clone(),
                        sound: None,
                        tts_text: None,
                        tts_priority: VoicePriority::default(),
                        treatment: *treatment,
                        priority: priority.clone(),
                    };
                    if *delay_secs <= 0.0 {
                        new_events.push(event);
                    } else {
                        new_pending.push(PendingAction {
                            fire_at: now + Duration::from_secs_f64(*delay_secs),
                            event,
                        });
                    }
                }
                Action::PlaySound {
                    sounds,
                    mode,
                    delay_secs,
                    ..
                } => {
                    let picked = pick_sound(sounds, *mode, &sound_seq[action_idx]);
                    if let Some(s) = picked {
                        let event = OverlayEvent {
                            icon: String::new(),
                            color: String::new(),
                            message: String::new(),
                            message_color: String::new(),
                            border_color: String::new(),
                            sound: Some(s),
                            tts_text: None,
                            tts_priority: VoicePriority::default(),
                            treatment: Treatment::default(),
                            priority: VoicePriority::default(),
                        };
                        if *delay_secs <= 0.0 {
                            new_events.push(event);
                        } else {
                            new_pending.push(PendingAction {
                                fire_at: now + Duration::from_secs_f64(*delay_secs),
                                event,
                            });
                        }
                    }
                }
            }
        }
        (new_events, new_pending)
    }

    /// Picks one label out of a `PlaySound` action's `sounds` list for a single
    /// firing. `seq` is that action's per-trigger sequential cursor, advanced
    /// only when `mode` is `Sequential` and there's more than one sound.
    fn pick_sound(sounds: &[String], mode: SoundMode, seq: &Cell<usize>) -> Option<String> {
        match sounds.len() {
            0 => None,
            1 => Some(sounds[0].clone()),
            n => match mode {
                SoundMode::Random => {
                    use rand::Rng;
                    let i = rand::thread_rng().gen_range(0..n);
                    Some(sounds[i].clone())
                }
                SoundMode::Sequential => {
                    let i = seq.get() % n;
                    seq.set(i + 1);
                    Some(sounds[i].clone())
                }
            },
        }
    }

    // ── Condition evaluation ──────────────────────────────────────────────────

    /// Shared by `Match` and `Chat` conditions: both compile a `match_type` +
    /// `pattern` pair the same way, they just apply it to different text.
    fn compile_pattern(match_type: &MatchType, pattern: &str) -> Option<CompiledMatch> {
        match match_type {
            MatchType::Exact => Some(CompiledMatch::Exact(pattern.to_owned())),
            MatchType::Regex => {
                let escaped = auto_escape_literal_groups(pattern);
                Regex::new(&escaped).ok().map(CompiledMatch::Regex)
            }
            MatchType::Glob => {
                let re_str = glob_to_regex(pattern);
                Regex::new(&re_str).ok().map(CompiledMatch::Regex)
            }
        }
    }

    /// True if `ast` contains nothing but literal text (and/or groups that
    /// are themselves nothing but literal text) — no wildcard, character
    /// class, repetition, alternation or assertion anywhere inside.
    ///
    /// A bare capturing group whose content is "boring" by this definition
    /// (e.g. `(Critical)`) can never capture anything a human would want
    /// back out via `{1}`/`{2}` — it always captures the exact same fixed
    /// text that's already sitting right there in the pattern. The only
    /// reason such a group exists is almost always that the trigger author
    /// pasted literal text straight out of the log (which legitimately
    /// contains parentheses, e.g. `damage. (Critical)`) into a regex field
    /// without knowing `(` `)` are regex syntax.
    fn is_boring(ast: &Ast) -> bool {
        match ast {
            Ast::Empty(_) | Ast::Literal(_) => true,
            Ast::Concat(c) => c.asts.iter().all(is_boring),
            Ast::Group(g) => is_boring(&g.ast),
            // Dot, Assertion, the various character classes, Repetition,
            // Alternation and Flags all mean "this is doing real matching
            // work" — never auto-escape around one of these.
            _ => false,
        }
    }

    /// Recursively collects the byte offsets of the `(` and `)` characters
    /// bounding every bare, unnamed, "boring" capturing group in `ast` —
    /// the ones `auto_escape_literal_groups` should turn into literal
    /// parentheses. Named groups (`(?<name>...)`) and non-capturing groups
    /// (`(?:...)`) are never touched, however boring their content —
    /// they're unambiguous, deliberate regex syntax either way.
    fn collect_literal_group_positions(ast: &Ast, out: &mut Vec<usize>) {
        if let Ast::Group(g) = ast {
            if matches!(g.kind, GroupKind::CaptureIndex(_)) && is_boring(&g.ast) {
                out.push(g.span.start.offset);
                out.push(g.span.end.offset - 1);
            }
        }
        match ast {
            Ast::Group(g) => collect_literal_group_positions(&g.ast, out),
            Ast::Concat(c) => {
                for a in &c.asts {
                    collect_literal_group_positions(a, out);
                }
            }
            Ast::Alternation(a) => {
                for a in &a.asts {
                    collect_literal_group_positions(a, out);
                }
            }
            Ast::Repetition(r) => collect_literal_group_positions(&r.ast, out),
            _ => {}
        }
    }

    /// Rewrites every "boring" bare capturing group in `pattern` — one whose
    /// content is pure literal text, like `(Critical)` — into escaped
    /// literal parentheses (`\(Critical\)`), so pasting real log text
    /// (which often contains parentheses) into a Regex-mode pattern just
    /// works without the trigger author needing to know `(` `)` are regex
    /// syntax. Genuine captures — anything containing a wildcard, character
    /// class, repetition or alternation, plus all named/non-capturing
    /// groups regardless of content — are left completely untouched, so
    /// `{1}`/`{2}`/`{name}` template placeholders keep working exactly as
    /// before. Invalid patterns are passed through unchanged and left for
    /// `Regex::new` to reject with its own error.
    fn auto_escape_literal_groups(pattern: &str) -> String {
        let Ok(ast) = regex_syntax::ast::parse::Parser::new().parse(pattern) else {
            return pattern.to_owned();
        };
        let mut positions = Vec::new();
        collect_literal_group_positions(&ast, &mut positions);
        if positions.is_empty() {
            return pattern.to_owned();
        }
        positions.sort_unstable();
        let mut out = String::with_capacity(pattern.len() + positions.len());
        let mut last = 0;
        for pos in positions {
            out.push_str(&pattern[last..pos]);
            out.push('\\');
            last = pos;
        }
        out.push_str(&pattern[last..]);
        out
    }

    fn compile_condition(cond: &Condition) -> CompiledCondition {
        let compiled = match cond {
            Condition::Match {
                match_type,
                pattern,
            }
            | Condition::Chat {
                match_type,
                pattern,
                ..
            } => compile_pattern(match_type, pattern),
            Condition::Var { .. } => None,
        };
        CompiledCondition {
            original: cond.clone(),
            compiled,
        }
    }

    /// Runs a compiled Exact/Regex/Glob pattern against `text`, returning the
    /// captures on a match. Shared by `Match` (against the raw line) and
    /// `Chat` (against just the extracted message).
    fn match_text(compiled: &CompiledMatch, text: &str) -> Option<CaptureMap> {
        match compiled {
            CompiledMatch::Exact(s) => text.contains(s.as_str()).then(CaptureMap::default),
            CompiledMatch::Regex(re) => re.captures(text).map(|caps| {
                let positional = (0..caps.len())
                    .map(|i| caps.get(i).map(|m| m.as_str().to_owned()))
                    .collect();
                let mut named = HashMap::new();
                for name in re.capture_names().flatten() {
                    if let Some(m) = caps.name(name) {
                        named.insert(name.to_owned(), m.as_str().to_owned());
                    }
                }
                CaptureMap { positional, named }
            }),
        }
    }

    /// Whether a chat line's structural verb segment (e.g. "tells the
    /// guild", "shouts", "says out of character" — see `RE_CHAT`) belongs to
    /// `channel`. Deliberately reads only `verb`, never the quoted message
    /// text, so a channel selection can't be spoofed by message content.
    fn chat_channel_matches(channel: &ChatChannel, custom_channel: &str, verb: &str) -> bool {
        let is_ooc = verb.contains("out of character");
        let is_guild = verb.contains("guild");
        let is_raid = verb.contains("raid");
        let is_group = verb.contains("party") || verb.contains("group");
        let is_auction = verb.contains("auction");
        match channel {
            ChatChannel::Any => true,
            ChatChannel::Ooc => is_ooc,
            ChatChannel::Guild => is_guild,
            ChatChannel::Raid => is_raid,
            ChatChannel::Group => is_group,
            ChatChannel::Auction => is_auction,
            ChatChannel::Shout => verb.starts_with("shout"),
            // A tell is the only channel EQ ever logs for a third party's
            // words, and always as exactly "tells you" (received) or "told
            // <name>" (sent) — "tells the guild"/"tells General:1" share the
            // same base verb but are excluded by the checks above already
            // having claimed guild/raid/group, and by the "told <bare name>"
            // shape check below for anything else.
            ChatChannel::Tell => {
                verb == "tells you"
                    || verb.strip_prefix("told ").is_some_and(|rest| {
                        !rest.is_empty()
                            && rest
                                .chars()
                                .all(|c| c.is_alphabetic() || c == '\'' || c == '`')
                    })
            }
            ChatChannel::Say => {
                verb.starts_with("say") && !is_ooc && !is_guild && !is_raid && !is_group
            }
            ChatChannel::Custom => {
                !custom_channel.is_empty()
                    && verb
                        .to_ascii_lowercase()
                        .contains(&custom_channel.to_ascii_lowercase())
            }
        }
    }

    fn eval_condition(
        cond: &CompiledCondition,
        line: &str,
        vars: &HashMap<String, String>,
        caps: &CaptureMap,
    ) -> (bool, Option<CaptureMap>) {
        match &cond.original {
            Condition::Match { .. } => {
                let Some(compiled) = &cond.compiled else {
                    return (false, None);
                };
                match match_text(compiled, line) {
                    Some(caps) => (true, Some(caps)),
                    None => (false, None),
                }
            }
            Condition::Chat {
                channel,
                custom_channel,
                ..
            } => {
                let Some(chat_caps) = crate::patterns::RE_CHAT.captures(line) else {
                    return (false, None);
                };
                if !chat_channel_matches(channel, custom_channel, &chat_caps["verb"]) {
                    return (false, None);
                }
                let Some(compiled) = &cond.compiled else {
                    return (false, None);
                };
                match match_text(compiled, &chat_caps["msg"]) {
                    Some(mut caps) => {
                        caps.named
                            .insert("speaker".to_owned(), chat_caps["speaker"].to_owned());
                        (true, Some(caps))
                    }
                    None => (false, None),
                }
            }
            Condition::Var {
                var_name,
                op,
                value,
            } => {
                let stored = resolve_var(var_name, caps, vars);
                let stored = stored.as_deref();
                let result = match op {
                    VarOp::Isset => stored.is_some(),
                    VarOp::Equals => stored
                        .map(|v| v.eq_ignore_ascii_case(value))
                        .unwrap_or(false),
                    VarOp::Gt => cmp_numeric(stored, value, |a, b| a > b),
                    VarOp::Gte => cmp_numeric(stored, value, |a, b| a >= b),
                    VarOp::Lt => cmp_numeric(stored, value, |a, b| a < b),
                    VarOp::Lte => cmp_numeric(stored, value, |a, b| a <= b),
                    VarOp::Matches => {
                        if let Some(v) = stored {
                            Regex::new(value).map(|re| re.is_match(v)).unwrap_or(false)
                        } else {
                            false
                        }
                    }
                };
                (result, None)
            }
        }
    }

    fn cmp_numeric(stored: Option<&str>, rhs: &str, op: impl Fn(f64, f64) -> bool) -> bool {
        let Some(v) = stored else { return false };
        let Ok(a) = v.parse::<f64>() else {
            return false;
        };
        let Ok(b) = rhs.parse::<f64>() else {
            return false;
        };
        op(a, b)
    }

    // ── Variable / capture resolution ─────────────────────────────────────────

    /// Resolves `name` against, in order: a positional capture index, a named
    /// capture group from conditions evaluated so far in the current trigger,
    /// then a persisted `store_var` variable. Shared by Var-condition
    /// comparisons and action template placeholders so both can refer to
    /// either a same-trigger regex capture or a stored variable interchangeably.
    fn resolve_var(
        name: &str,
        caps: &CaptureMap,
        vars: &HashMap<String, String>,
    ) -> Option<String> {
        if let Ok(i) = name.parse::<usize>() {
            if let Some(Some(s)) = caps.positional.get(i) {
                return Some(s.clone());
            }
        }
        if let Some(s) = caps.named.get(name) {
            return Some(s.clone());
        }
        vars.get(name).cloned()
    }

    // ── Template resolution ───────────────────────────────────────────────────

    fn resolve_template(
        template: &str,
        caps: &CaptureMap,
        vars: &HashMap<String, String>,
    ) -> String {
        PLACEHOLDER
            .replace_all(template, |m: &regex::Captures<'_>| {
                resolve_var(&m[1], caps, vars).unwrap_or_else(|| m[0].to_owned())
            })
            .into_owned()
    }

    // ── Glob → regex compiler ─────────────────────────────────────────────────

    /// Convert a glob pattern to a regex string.
    ///
    /// Supported glob syntax:
    ///   `*`      — matches any sequence of characters
    ///   `?`      — matches exactly one character
    ///   `{name}` — named capture group (referenced as `{name}` in action templates)
    ///
    /// All other regex metacharacters are escaped.
    pub fn glob_to_regex(glob: &str) -> String {
        let chars: Vec<char> = glob.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '{' => {
                    // Look for a valid identifier followed by '}'.
                    let start = i + 1;
                    let rel = chars[start..].iter().position(|&c| c == '}');
                    if let Some(rel) = rel {
                        let name: String = chars[start..start + rel].iter().collect();
                        let valid = !name.is_empty()
                            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                        if valid {
                            out.push_str(&format!("(?P<{name}>.+?)"));
                            i = start + rel + 1;
                            continue;
                        }
                    }
                    out.push_str("\\{");
                    i += 1;
                }
                '}' => {
                    out.push_str("\\}");
                    i += 1;
                }
                '*' => {
                    out.push_str(".*");
                    i += 1;
                }
                '?' => {
                    out.push('.');
                    i += 1;
                }
                c => {
                    if ".+^$|\\[]()".contains(c) {
                        out.push('\\');
                    }
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn migrate_legacy_sound_folds_into_sounds() {
            let mut cfg = TriggerConfig {
                triggers: vec![TriggerDef {
                    name: "t".into(),
                    enabled: true,
                    condition_logic: ConditionLogic::default(),
                    conditions: Vec::new(),
                    actions: vec![Action::PlaySound {
                        sound: Some("aggro".into()),
                        sounds: Vec::new(),
                        mode: SoundMode::default(),
                        delay_secs: 0.0,
                    }],
                }],
            };
            assert!(migrate_legacy_sounds(&mut cfg));
            let Action::PlaySound { sound, sounds, .. } = &cfg.triggers[0].actions[0] else {
                panic!("expected PlaySound");
            };
            assert_eq!(sound, &None);
            assert_eq!(sounds, &vec!["aggro".to_string()]);
            // Idempotent: a second pass over already-migrated data is a no-op.
            assert!(!migrate_legacy_sounds(&mut cfg));
        }

        #[test]
        fn pick_sound_sequential_cycles_in_order_and_wraps() {
            let sounds = vec!["a".to_string(), "b".to_string(), "c".to_string()];
            let seq = Cell::new(0);
            let picks: Vec<_> = (0..5)
                .map(|_| pick_sound(&sounds, SoundMode::Sequential, &seq).unwrap())
                .collect();
            assert_eq!(picks, vec!["a", "b", "c", "a", "b"]);
        }

        #[test]
        fn pick_sound_random_stays_within_the_list() {
            let sounds = vec!["a".to_string(), "b".to_string()];
            let seq = Cell::new(0);
            for _ in 0..20 {
                let picked = pick_sound(&sounds, SoundMode::Random, &seq).unwrap();
                assert!(sounds.contains(&picked));
            }
        }

        #[test]
        fn pick_sound_empty_list_is_none() {
            let seq = Cell::new(0);
            assert_eq!(pick_sound(&[], SoundMode::Random, &seq), None);
        }

        #[test]
        fn auto_escape_turns_literal_paren_group_into_literal_parens() {
            // The real bug that prompted this: a trigger author pasted actual
            // log text into a Regex pattern, not realizing "(Critical)"
            // reads as a (pointless) capture group rather than literal text.
            let line = "You slash a greater skeleton for 96 points of damage. (Critical)";
            let pattern = "You .*damage. (Critical)";
            let escaped = auto_escape_literal_groups(pattern);
            assert_eq!(escaped, "You .*damage. \\(Critical\\)");
            assert!(Regex::new(&escaped).unwrap().is_match(line));
            // Unescaped, this must NOT match — confirms the test is actually
            // exercising the fix rather than a pattern that matched anyway.
            assert!(!Regex::new(pattern).unwrap().is_match(line));
        }

        #[test]
        fn auto_escape_leaves_real_captures_alone() {
            // Anything with a wildcard, digit class, or alternation inside
            // is doing real capturing work and must be untouched, including
            // its group numbering (`{1}` etc. depend on it).
            for pattern in [
                r"You hit (.*) for (\d+) points",
                r"(a|b) start",
                r"(Critical|Crushing Blow)",
            ] {
                assert_eq!(
                    auto_escape_literal_groups(pattern),
                    pattern,
                    "should not touch: {pattern}"
                );
            }
        }

        #[test]
        fn auto_escape_leaves_named_and_non_capturing_groups_alone() {
            for pattern in ["(?<mob>Critical)", "(?P<mob>Critical)", "(?:Critical)"] {
                assert_eq!(
                    auto_escape_literal_groups(pattern),
                    pattern,
                    "should not touch: {pattern}"
                );
            }
        }

        #[test]
        fn auto_escape_handles_multiple_and_nested_literal_groups() {
            assert_eq!(
                auto_escape_literal_groups("(Critical) and (Crippling Blow)"),
                "\\(Critical\\) and \\(Crippling Blow\\)"
            );
            // The outer group contains a real capture, so IT stays a group —
            // but the inner literal "(bar)" still gets escaped independently.
            assert_eq!(
                auto_escape_literal_groups(r"(foo (bar) (\d+))"),
                r"(foo \(bar\) (\d+))"
            );
        }

        #[test]
        fn auto_escape_passes_through_invalid_regex_unchanged() {
            let bad = "(unclosed";
            assert_eq!(auto_escape_literal_groups(bad), bad);
        }

        #[test]
        fn chat_channel_classifies_known_verb_shapes() {
            let cases: &[(&str, ChatChannel)] = &[
                ("says", ChatChannel::Say),
                ("say to your guild", ChatChannel::Guild),
                ("tells the guild", ChatChannel::Guild),
                ("say to your raid", ChatChannel::Raid),
                ("tells the raid", ChatChannel::Raid),
                ("tell your party", ChatChannel::Group),
                ("tells the group", ChatChannel::Group),
                ("shout", ChatChannel::Shout),
                ("shouts", ChatChannel::Shout),
                ("says out of character", ChatChannel::Ooc),
                ("say out of character", ChatChannel::Ooc),
                ("tells you", ChatChannel::Tell),
                ("told Zyro", ChatChannel::Tell),
                ("auctions", ChatChannel::Auction),
            ];
            for (verb, channel) in cases {
                assert!(
                    chat_channel_matches(channel, "", verb),
                    "{verb:?} should classify as {channel:?}"
                );
            }
        }

        #[test]
        fn chat_channel_say_excludes_other_say_shaped_verbs() {
            for verb in [
                "say to your guild",
                "say to your raid",
                "say out of character",
            ] {
                assert!(
                    !chat_channel_matches(&ChatChannel::Say, "", verb),
                    "{verb:?} should not classify as plain Say"
                );
            }
        }

        /// A tell is the only channel EQ ever shows a third party's exact
        /// words on ("tells you" / "told <name>") — anything else sharing
        /// the "tell(s)" base verb is a named channel, not a private tell.
        #[test]
        fn chat_channel_tell_excludes_channel_tells_and_custom_channels() {
            for verb in [
                "tells the guild",
                "tells the raid",
                "tells the group",
                "tells General:1",
            ] {
                assert!(
                    !chat_channel_matches(&ChatChannel::Tell, "", verb),
                    "{verb:?} should not classify as Tell"
                );
            }
        }

        #[test]
        fn chat_channel_custom_matches_by_substring() {
            assert!(chat_channel_matches(
                &ChatChannel::Custom,
                "General:1",
                "tells General:1"
            ));
            assert!(!chat_channel_matches(
                &ChatChannel::Custom,
                "General:1",
                "tells the guild"
            ));
            assert!(!chat_channel_matches(
                &ChatChannel::Custom,
                "",
                "tells General:1"
            ));
        }

        #[test]
        fn chat_condition_matches_message_not_raw_line() {
            let cond = Condition::Chat {
                channel: ChatChannel::Tell,
                custom_channel: String::new(),
                match_type: MatchType::Exact,
                pattern: "help".into(),
            };
            let compiled = compile_condition(&cond);
            let vars = HashMap::new();
            let caps = CaptureMap::default();

            let (fired, out) = eval_condition(&compiled, "Rysk tells you, 'help'", &vars, &caps);
            assert!(fired);
            assert_eq!(
                out.unwrap().named.get("speaker").map(String::as_str),
                Some("Rysk")
            );
        }

        /// The concrete injection this design blocks: another channel
        /// quoting text that happens to satisfy a Tell-only condition's
        /// pattern must not fire it — channel classification comes from the
        /// line's own verb structure, never from inside the quoted message.
        #[test]
        fn chat_condition_is_not_spoofed_by_another_channel_quoting_the_pattern() {
            let cond = Condition::Chat {
                channel: ChatChannel::Tell,
                custom_channel: String::new(),
                match_type: MatchType::Exact,
                pattern: "help".into(),
            };
            let compiled = compile_condition(&cond);
            let vars = HashMap::new();
            let caps = CaptureMap::default();

            for line in [
                "Rysk shouts, 'help'",
                "Rysk says, 'help'",
                "Rysk tells the guild, 'help'",
            ] {
                let (fired, _) = eval_condition(&compiled, line, &vars, &caps);
                assert!(!fired, "{line} should not satisfy a Tell-only condition");
            }
        }
    }
}
