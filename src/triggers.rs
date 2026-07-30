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
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use once_cell::sync::Lazy;
    use regex::Regex;
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
            /// None/empty = none, otherwise the name of a sound label (see
            /// `sound_packages`) resolved through whichever package is
            /// currently active.
            #[serde(default)]
            sound: Option<String>,
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
    /// package if needed. Returns whether anything changed, so the caller
    /// knows to persist the migrated file. Idempotent: migrated values are
    /// bare labels with no `/`/`\`, so a second pass is always a no-op.
    fn migrate_legacy_sounds(cfg: &mut TriggerConfig) -> bool {
        let mut changed = false;
        for def in &mut cfg.triggers {
            for action in &mut def.actions {
                if let Action::PlaySound { sound: Some(s), .. } = action {
                    if s.contains('/') || s.contains('\\') {
                        if let Some(label) =
                            crate::sound_packages::sound_packages::migrate_legacy_sound_value(s)
                        {
                            *s = label;
                            changed = true;
                        }
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
        logic: ConditionLogic,
        conditions: Vec<CompiledCondition>,
        actions: Vec<Action>,
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
                triggers.push(CompiledTrigger {
                    logic: def.condition_logic.clone(),
                    conditions,
                    actions: def.actions.clone(),
                });
            }
            Self {
                triggers,
                vars: HashMap::new(),
                pending: Vec::new(),
                output,
            }
        }

        fn process_line(&mut self, line: &str) {
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

                // Execute actions in order.
                for action in &trigger.actions {
                    match action {
                        Action::StoreVar { var_name, value } => {
                            if !var_name.is_empty() {
                                let resolved = resolve_template(value, &caps, &self.vars);
                                self.vars.insert(var_name.clone(), resolved);
                            }
                        }
                        Action::VoiceAlert { tts_text, priority } => {
                            let text = resolve_template(tts_text, &caps, &self.vars);
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
                                message: resolve_template(message, &caps, &self.vars),
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
                        Action::PlaySound { sound, delay_secs } => {
                            if let Some(s) = sound.clone().filter(|s| !s.is_empty()) {
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
            }

            self.pending.extend(new_pending);
            if !new_events.is_empty() {
                let mut q = self.output.lock().unwrap();
                q.extend(new_events);
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

    // ── Condition evaluation ──────────────────────────────────────────────────

    fn compile_condition(cond: &Condition) -> CompiledCondition {
        let compiled = match cond {
            Condition::Match {
                match_type,
                pattern,
            } => match match_type {
                MatchType::Exact => Some(CompiledMatch::Exact(pattern.clone())),
                MatchType::Regex => Regex::new(pattern).ok().map(CompiledMatch::Regex),
                MatchType::Glob => {
                    let re_str = glob_to_regex(pattern);
                    Regex::new(&re_str).ok().map(CompiledMatch::Regex)
                }
            },
            Condition::Var { .. } => None,
        };
        CompiledCondition {
            original: cond.clone(),
            compiled,
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
                match compiled {
                    CompiledMatch::Exact(s) => {
                        if line.contains(s.as_str()) {
                            (true, Some(CaptureMap::default()))
                        } else {
                            (false, None)
                        }
                    }
                    CompiledMatch::Regex(re) => {
                        if let Some(caps) = re.captures(line) {
                            let positional = (0..caps.len())
                                .map(|i| caps.get(i).map(|m| m.as_str().to_owned()))
                                .collect();
                            let mut named = HashMap::new();
                            for name in re.capture_names().flatten() {
                                if let Some(m) = caps.name(name) {
                                    named.insert(name.to_owned(), m.as_str().to_owned());
                                }
                            }
                            (true, Some(CaptureMap { positional, named }))
                        } else {
                            (false, None)
                        }
                    }
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
}
