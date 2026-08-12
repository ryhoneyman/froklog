//! Audio triggers: the dev's trigger engine, with a Linux voice.
//!
//! The matching is entirely his — `froklog::triggers::engine` reads the same
//! `triggers.toml`, does the same regex/glob/variable work, and resolves sound
//! labels through the same sound packages, so a triggers file written for the
//! Windows client behaves identically here. What does not carry over is the
//! output: his overlay speaks through the Windows speech API and plays sound
//! through `PlaySoundW`. Those are the two things this module replaces.
//!
//! Everything is played through the desktop's own audio server, tagged as
//! froklog watch, so it shows up as its own stream in the volume mixer and can
//! be turned down without touching the game.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// Master volume 0–100, applied to every sound and voice (paplay --volume,
/// spd-say -i). Process-wide like the Windows client's audio atomics; the
/// window syncs it from settings each frame.
pub static VOLUME: AtomicU8 = AtomicU8::new(100);
/// Master mute. Auditions (the Sounds tab's ▶ buttons, "Say a phrase")
/// deliberately bypass it via the *_forced variants — you audition BECAUSE
/// things are muted — matching the Windows client's preview behavior.
pub static MUTED: AtomicBool = AtomicBool::new(false);

/// paplay volume scale: 65536 = 100%.
fn paplay_volume() -> String {
    let v = VOLUME.load(Ordering::Relaxed).min(100) as u32;
    format!("--volume={}", v * 65536 / 100)
}

use froklog::triggers::engine::{
    Action, Condition, OverlayEvent, TriggerConfig, TriggerDef, TriggerEngine, VoicePriority,
};

/// What a fired trigger looked like, for the window to show.
#[derive(Clone)]
pub struct Fired {
    pub message: String,
    pub spoke: bool,
    pub played: Option<String>,
}

/// An existing trigger taken back apart into the fields the builder edits.
pub struct Parts {
    pub name: String,
    pub pattern: String,
    pub sound: String,
    pub say: String,
    pub show: String,
}

pub struct Alerts {
    engine: TriggerEngine,
    queue: Arc<Mutex<Vec<OverlayEvent>>>,
    /// what fired recently, newest last, capped
    pub recent: Vec<Fired>,
    pub enabled: bool,
    /// the loaded triggers.toml, kept so the window can show and edit it
    cfg: TriggerConfig,
}

impl Alerts {
    pub fn load(enabled: bool) -> Self {
        let cfg = TriggerConfig::load();
        let queue = Arc::new(Mutex::new(Vec::new()));
        Self {
            engine: TriggerEngine::new(&cfg, Arc::clone(&queue)),
            queue,
            recent: Vec::new(),
            enabled,
            cfg,
        }
    }

    pub fn triggers(&self) -> &[TriggerDef] {
        &self.cfg.triggers
    }

    pub fn count(&self) -> usize {
        self.cfg.triggers.len()
    }

    /// Turn one trigger on or off and write it back. Enabling and disabling is
    /// most of what trigger editing actually is day to day — the rest is text.
    pub fn set_enabled(&mut self, index: usize, on: bool) {
        let Some(t) = self.cfg.triggers.get_mut(index) else {
            return;
        };
        t.enabled = on;
        self.cfg.save();
        self.engine.reload(&self.cfg);
    }

    /// The trigger file as text, for editing in place.
    pub fn read_file() -> String {
        std::fs::read_to_string(froklog::triggers::engine::triggers_path()).unwrap_or_default()
    }

    /// Validate, write, and start using it. Parsing first means a typo shows
    /// up here rather than silently emptying the file the engine reads.
    pub fn write_file(&mut self, text: &str) -> Result<usize, String> {
        let cfg: TriggerConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        std::fs::write(froklog::triggers::engine::triggers_path(), text)
            .map_err(|e| e.to_string())?;
        let n = cfg.triggers.len();
        self.cfg = cfg;
        self.engine.reload(&self.cfg);
        Ok(n)
    }

    /// Fire one trigger's own actions, right now, ignoring its conditions —
    /// the honest per-trigger test. (No fake line can match an arbitrary
    /// pattern, so the actions are executed directly; unfilled `{n}` capture
    /// placeholders in spoken text become the word "something".)
    pub fn fire_trigger_actions(
        &self,
        index: usize,
        package: &str,
        voice: &Voice,
    ) -> Vec<crate::messages::Msg> {
        let mut shown = Vec::new();
        let Some(t) = self.cfg.triggers.get(index) else {
            return shown;
        };
        for action in &t.actions {
            match action {
                Action::PlaySound { sound, .. } => {
                    if let Some(s) = sound {
                        play_forced(s, package);
                    }
                }
                Action::VoiceAlert { tts_text, priority } => {
                    let mut said = tts_text.clone();
                    while let Some(a) = said.find('{') {
                        match said[a..].find('}') {
                            Some(rel) => said.replace_range(a..a + rel + 1, "something"),
                            None => break,
                        }
                    }
                    speak_forced(&said, priority, voice);
                }
                Action::Overlay { message, .. } => {
                    // Same placeholder substitution as the spoken text: a
                    // test has no captures to fill in.
                    let mut text = message.clone();
                    while let Some(a) = text.find('{') {
                        match text[a..].find('}') {
                            Some(rel) => text.replace_range(a..a + rel + 1, "something"),
                            None => break,
                        }
                    }
                    let mut ev = action.clone();
                    if let Action::Overlay { message, .. } = &mut ev {
                        *message = text;
                    }
                    if let Action::Overlay {
                        icon,
                        color,
                        message,
                        message_color,
                        border_color,
                        treatment,
                        priority,
                        ..
                    } = ev
                    {
                        shown.push(crate::messages::Msg {
                            icon,
                            color,
                            text: message,
                            text_color: message_color,
                            border_color,
                            treatment,
                            priority,
                        });
                    }
                }
                // Nothing to prove for a variable write.
                Action::StoreVar { .. } => {}
            }
        }
        shown
    }

    /// Remove a trigger permanently — written to triggers.toml and the
    /// engine reloaded, same as every other edit. (Silencing without losing
    /// it is what the enable tick is for.)
    pub fn delete_trigger(&mut self, index: usize) {
        if index < self.cfg.triggers.len() {
            self.cfg.triggers.remove(index);
            self.cfg.save();
            self.engine.reload(&self.cfg);
        }
    }

    /// Append a new trigger and write it to the same file the Windows client
    /// reads, so anything built here is portable back to it.
    pub fn add_trigger(&mut self, name: &str, pattern: &str, sound: &str, say: &str, show: &str) {
        let mut actions = Vec::new();
        if !sound.is_empty() {
            actions.push(Action::PlaySound {
                sound: Some(sound.to_string()),
                delay_secs: 0.0,
            });
        }
        if !say.is_empty() {
            actions.push(Action::VoiceAlert {
                tts_text: say.to_string(),
                priority: VoicePriority::default(),
            });
        }
        if !show.is_empty() {
            actions.push(overlay_action(show));
        }
        self.cfg.triggers.push(TriggerDef {
            name: name.to_string(),
            enabled: true,
            condition_logic: Default::default(),
            conditions: vec![Condition::Match {
                match_type: froklog::triggers::engine::MatchType::Regex,
                pattern: pattern.to_string(),
            }],
            actions,
        });
        self.cfg.save();
        self.engine.reload(&self.cfg);
    }
}

/// The overlay action the builder writes. Icon and colours are left at their
/// defaults — the message overlay picks an accent from the icon key, and a
/// hand-edited triggers.toml can set anything richer.
fn overlay_action(message: &str) -> Action {
    Action::Overlay {
        icon: String::new(),
        color: String::new(),
        message: message.to_string(),
        message_color: String::new(),
        border_color: String::new(),
        delay_secs: 0.0,
        treatment: Default::default(),
        priority: VoicePriority::default(),
    }
}

impl Alerts {
    /// Pull an existing trigger back apart into the four fields the builder
    /// edits — name, pattern, sound label, spoken text. A trigger written by
    /// hand can hold more than that (several conditions, overlay actions), so
    /// only the first match condition and the first sound/voice/overlay
    /// action come back; `parts` returning None means "too rich for the
    /// builder, use the text editor" rather than silently dropping the rest
    /// on save.
    pub fn parts(&self, index: usize) -> Option<Parts> {
        let t = self.cfg.triggers.get(index)?;
        if t.conditions.len() > 1 {
            return None;
        }
        let pattern = match t.conditions.first()? {
            Condition::Match {
                match_type: froklog::triggers::engine::MatchType::Regex,
                pattern,
            } => pattern.clone(),
            _ => return None,
        };
        let mut sound = String::new();
        let mut say = String::new();
        let mut show = String::new();
        for a in &t.actions {
            match a {
                Action::PlaySound { sound: Some(s), .. } if sound.is_empty() => sound = s.clone(),
                Action::VoiceAlert { tts_text, .. } if say.is_empty() => say = tts_text.clone(),
                Action::Overlay { message, .. } if show.is_empty() => show = message.clone(),
                _ => return None,
            }
        }
        Some(Parts {
            name: t.name.clone(),
            pattern,
            sound,
            say,
            show,
        })
    }

    /// Replace a trigger in place. Keeps its position in the list and its
    /// enabled state, so editing a live trigger does not silently re-enable
    /// one that was ticked off, and preserves the voice priority already set.
    pub fn update_trigger(
        &mut self,
        index: usize,
        name: &str,
        pattern: &str,
        sound: &str,
        say: &str,
        show: &str,
    ) {
        let Some(old) = self.cfg.triggers.get(index) else {
            return;
        };
        let priority = old
            .actions
            .iter()
            .find_map(|a| match a {
                Action::VoiceAlert { priority, .. } => Some(priority.clone()),
                _ => None,
            })
            .unwrap_or_default();
        // Keep whatever the overlay action already said beyond its text:
        // icon, colours and treatment are only settable by hand-editing, and
        // an edit through the builder must not quietly discard them.
        let kept_overlay = old.actions.iter().find_map(|a| match a {
            Action::Overlay { .. } => Some(a.clone()),
            _ => None,
        });
        let mut actions = Vec::new();
        if !sound.is_empty() {
            actions.push(Action::PlaySound {
                sound: Some(sound.to_string()),
                delay_secs: 0.0,
            });
        }
        if !say.is_empty() {
            actions.push(Action::VoiceAlert {
                tts_text: say.to_string(),
                priority,
            });
        }
        if !show.is_empty() {
            actions.push(match kept_overlay {
                Some(Action::Overlay {
                    icon,
                    color,
                    message_color,
                    border_color,
                    delay_secs,
                    treatment,
                    priority,
                    ..
                }) => Action::Overlay {
                    icon,
                    color,
                    message: show.to_string(),
                    message_color,
                    border_color,
                    delay_secs,
                    treatment,
                    priority,
                },
                _ => overlay_action(show),
            });
        }
        let t = &mut self.cfg.triggers[index];
        t.name = name.to_string();
        t.conditions = vec![Condition::Match {
            match_type: froklog::triggers::engine::MatchType::Regex,
            pattern: pattern.to_string(),
        }];
        t.actions = actions;
        self.cfg.save();
        self.engine.reload(&self.cfg);
    }

    /// Run a line as if the game had just logged it, so a trigger can be heard
    /// before the fight that needs it. Exercised by the #[ignore] round-trip
    /// tests; nothing in the UI calls it since Try-it began compiling the
    /// draft pattern directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn test_line(&self, line: &str) {
        self.engine.process_line(line);
    }

    /// Re-read triggers.toml. Editing that file is how triggers are authored,
    /// so there has to be a way to pick up the edit without a restart.
    pub fn reload(&mut self) {
        self.cfg = TriggerConfig::load();
        self.engine.reload(&self.cfg);
    }

    pub fn path() -> std::path::PathBuf {
        froklog::triggers::engine::triggers_path()
    }

    /// Feed one log line through the engine.
    pub fn process_line(&self, line: &str) {
        if self.enabled {
            // Live lines carry the "[Thu Jul 31 ...] " timestamp and the
            // engine matches raw text — without stripping it here, every
            // start-anchored (^) pattern the builder generates would test
            // fine and then never fire once in real play.
            self.engine.process_line(builder::strip_timestamp(line));
        }
    }

    /// Fire whatever the engine has queued. Called on the UI tick, which also
    /// drives the engine's own delayed actions.
    pub fn pump(&mut self, package: &str, voice: &Voice) -> Vec<crate::messages::Msg> {
        self.engine.tick();
        let events: Vec<OverlayEvent> = {
            let mut q = self.queue.lock().unwrap();
            if q.is_empty() {
                return Vec::new();
            }
            std::mem::take(&mut *q)
        };
        let mut announce = Vec::new();
        for ev in events {
            if let Some(m) = crate::messages::Msg::from_event(&ev) {
                announce.push(m);
            }
            let played = ev.sound.as_deref().and_then(|s| play(s, package));
            let spoke = ev
                .tts_text
                .as_deref()
                .is_some_and(|t| speak(t, &ev.tts_priority, voice));
            self.recent.push(Fired {
                message: if ev.message.is_empty() {
                    ev.tts_text.clone().unwrap_or_default()
                } else {
                    ev.message
                },
                spoke,
                played,
            });
        }
        // a log is long; the interesting alert is the last one
        let overflow = self.recent.len().saturating_sub(20);
        self.recent.drain(..overflow);
        announce
    }
}

/// Resolve a trigger's sound and play it. Returns what was played, if anything.
///
/// A trigger names a sound by label ("Ding"), which the active sound package
/// maps to a file — that is what lets one package swap every trigger's sounds
/// at once. An absolute path is taken literally, for a sound that is not in a
/// package at all.
pub fn play(sound: &str, package: &str) -> Option<String> {
    if MUTED.load(Ordering::Relaxed) {
        return None;
    }
    play_forced(sound, package)
}

/// Play regardless of the mute switch (auditions). Volume still applies.
pub fn play_forced(sound: &str, package: &str) -> Option<String> {
    if sound.is_empty() {
        return None;
    }
    let direct = std::path::Path::new(sound);
    let path = if direct.is_absolute() && direct.exists() {
        direct.to_path_buf()
    } else {
        froklog::sound_packages::sound_packages::resolve_label(package, sound)?
    };
    play_file(&path).then(|| path.to_string_lossy().into_owned())
}

/// paplay is the PipeWire/PulseAudio client every desktop ships; aplay is the
/// bare-ALSA fallback for a machine without a sound server. Naming the
/// application is what gives froklog watch its own slider in the mixer.
fn play_file(path: &std::path::Path) -> bool {
    let paplay = Command::new("paplay")
        .arg("--property=application.name=froklog watch")
        .arg("--property=media.role=event")
        .arg(paplay_volume())
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if paplay.is_ok() {
        return true;
    }
    Command::new("aplay")
        .arg("-q")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// How an alert gets spoken.
///
/// speech-dispatcher is always there, needs no setup and sounds like a robot,
/// because underneath it is espeak-ng — a formant synthesiser from an era when
/// that was the only thing that fit on the machine. piper is a neural voice:
/// it needs a model file on disk, and sounds like a person.
pub enum Voice<'a> {
    SpeechDispatcher,
    Piper { model: &'a str },
}

impl<'a> Voice<'a> {
    pub fn from_settings(engine: &'a str, model: &'a str) -> Self {
        match engine {
            "piper" if !model.is_empty() => Voice::Piper { model },
            _ => Voice::SpeechDispatcher,
        }
    }
}

/// How to say the words a speech engine has never seen.
///
/// Neither piper nor espeak has ever met Cazic-Thule, and both guess from
/// English spelling — usually badly, and always the same badly. Rather than
/// compile espeak pronunciation dictionaries (which are per-engine, need a
/// build step, and would not help speech-dispatcher) this rewrites the text
/// before it is spoken: "Cazic-Thule" in, "Kay-zick Thool" out. It is a
/// spelling hint for a machine, so it is written the way it sounds, not in
/// phonetic notation nobody wants to hand-write.
///
/// Lives at ~/.config/froklog-watch/pronounce.toml as plain key = "value".
pub fn pronounce_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("froklog-watch")
        .join("pronounce.toml")
}

/// The table as the window edits it: insertion-friendly pairs, not a map.
pub fn pronunciations() -> Vec<(String, String)> {
    load_pronunciations()
}

/// Write the table back. Speaking re-reads the file, so an edit takes effect
/// on the next alert without a reload button.
pub fn save_pronunciations(table: &[(String, String)]) -> std::io::Result<()> {
    let mut out = String::from(
        "# How to say what the voice gets wrong. Spelled the way it sounds,\n         # not in phonetic notation. Whole words only, case-insensitive.\n\n",
    );
    for (from, to) in table {
        if from.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("{:?} = {:?}\n", from.trim(), to.trim()));
    }
    let path = pronounce_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, out)
}

/// Apply the table to a line the way an alert would, so the window can show
/// what a voice will actually be handed.
pub fn preview(text: &str) -> String {
    respell(text, &load_pronunciations())
}

fn load_pronunciations() -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(pronounce_path()) else {
        return Vec::new();
    };
    let Ok(table) = toml::from_str::<std::collections::BTreeMap<String, String>>(&text) else {
        return Vec::new();
    };
    let mut v: Vec<(String, String)> = table.into_iter().collect();
    // longest first, so "Plane of Fear" wins over a "Fear" entry
    v.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    v
}

/// Apply the table, matching whole words and ignoring case.
fn respell(text: &str, table: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (from, to) in table {
        let mut result = String::with_capacity(out.len());
        let lower = out.to_lowercase();
        let needle = from.to_lowercase();
        let mut at = 0;
        while let Some(hit) = lower[at..].find(&needle) {
            let start = at + hit;
            let end = start + needle.len();
            let before_ok = start == 0
                || !lower[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric());
            let after_ok = end == lower.len()
                || !lower[end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric());
            result.push_str(&out[at..start]);
            if before_ok && after_ok {
                result.push_str(to);
            } else {
                result.push_str(&out[start..end]);
            }
            at = end;
        }
        result.push_str(&out[at..]);
        out = result;
    }
    out
}

/// Every piper voice sitting on this machine, newest naming scheme or not.
///
/// A voice is just a pair of files, so "install a voice" means dropping an
/// .onnx and its .json into this directory — no code change, no rebuild. The
/// picker lists whatever is there.
pub fn installed_voices() -> Vec<(String, String)> {
    let dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("piper")
        .join("voices");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "onnx" {
                return None;
            }
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            Some((stem, path.to_string_lossy().into_owned()))
        })
        .collect();
    out.sort();
    out
}

/// Everything needed to turn a pasted log line into a working pattern.
pub mod builder {
    use std::collections::BTreeSet;

    /// Drop EverQuest's timestamp.
    ///
    /// Every logged line starts with "[Sun Jul 26 19:12:18 2026] ". Leaving it
    /// in a pattern would build a trigger that matches exactly one second of
    /// one day and never fires again, which is the first thing a hand-written
    /// trigger gets wrong.
    pub fn strip_timestamp(line: &str) -> &str {
        let t = line.trim();
        match (t.starts_with('['), t.find(']')) {
            (true, Some(end)) => t[end + 1..].trim_start(),
            _ => t,
        }
    }

    /// Split into runs of word characters and everything between them, so a
    /// name can be picked out without dragging the punctuation around it.
    ///
    /// An apostrophe counts as punctuation, not part of a word: EQ wraps
    /// speech in them ("says, 'Hail'") far more often than names contain
    /// them, and a quote swallowed into a capture would match the wrong text.
    pub fn tokenize(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_word = false;
        for c in line.chars() {
            let w = c.is_alphanumeric();
            if cur.is_empty() || w == in_word {
                cur.push(c);
            } else {
                out.push(std::mem::take(&mut cur));
                cur.push(c);
            }
            in_word = w;
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// True when text pasted into the "log line" box is plainly a regex
    /// rather than a line from the game. The builder escapes whatever it is
    /// given, so a regex pasted into the wrong box becomes a pattern that
    /// matches literal backslashes and can never fire — silently, because
    /// it still looks roughly right in the pattern field.
    pub fn looks_like_regex(line: &str) -> bool {
        let t = line.trim();
        if t.is_empty() {
            return false;
        }
        t.starts_with('^')
            || t.ends_with('$')
            || ["\\w", "\\d", "\\s", ".+?", ".*?", "(?:", "[^"]
                .iter()
                .any(|m| t.contains(m))
    }

    /// True for the tokens worth offering as a variable — the words, not the
    /// spaces and commas between them.
    pub fn is_word(tok: &str) -> bool {
        tok.chars().next().is_some_and(|c| c.is_alphanumeric())
    }

    /// What a chosen token most likely varies over. A number stays a number,
    /// because a digit capture is what makes a threshold testable later; a
    /// word stays a word so punctuation still has to match.
    fn capture_for(tok: &str) -> &'static str {
        if tok.chars().all(|c| c.is_ascii_digit()) {
            "(\\d+)"
        } else if tok.chars().all(|c| c.is_alphanumeric()) {
            "(\\w+)"
        } else {
            "(.+?)"
        }
    }

    /// Build the pattern: chosen tokens become capture groups, wild tokens
    /// become don't-care wildcards, everything else is matched literally and
    /// escaped, so a name containing a bracket or a full stop cannot
    /// silently turn into a wildcard.
    ///
    /// Adjacent wild words COLLAPSE into one `.+?` — including the spaces
    /// between them — because the thing that varies is usually a mob name
    /// and mob names change word count ("a rat" / "a greater skeleton").
    /// One trigger built on either matches both.
    pub fn regex(tokens: &[String], chosen: &BTreeSet<usize>, wild: &BTreeSet<usize>) -> String {
        let mut out = String::from("^");
        let mut i = 0;
        while i < tokens.len() {
            if chosen.contains(&i) {
                out.push_str(capture_for(&tokens[i]));
                i += 1;
            } else if wild.contains(&i) {
                // Swallow this wild run: further wild words and whatever
                // separators sit between them.
                out.push_str(".+?");
                i += 1;
                loop {
                    // Separators directly before another wild word belong
                    // to the run; a separator before a literal does not.
                    let mut j = i;
                    while j < tokens.len() && !is_word(&tokens[j]) {
                        j += 1;
                    }
                    if j < tokens.len() && wild.contains(&j) {
                        i = j + 1;
                    } else {
                        break;
                    }
                }
            } else {
                out.push_str(&escape(&tokens[i]));
                i += 1;
            }
        }
        out
    }

    /// The capture number a chosen token will have, for showing "{1}" beside
    /// the word it came from.
    pub fn group_of(chosen: &BTreeSet<usize>, index: usize) -> Option<usize> {
        chosen.iter().position(|i| *i == index).map(|n| n + 1)
    }

    fn escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if "\\.+*?()|[]{}^$".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }
}

/// One line describing what a trigger watches for and what it does, so the
/// list is readable without opening the file.
pub fn describe(def: &TriggerDef) -> String {
    let watches: Vec<String> = def
        .conditions
        .iter()
        .map(|c| match c {
            Condition::Match { pattern, .. } => pattern.clone(),
            Condition::Var { var_name, .. } => format!("${var_name}"),
        })
        .collect();
    let does: Vec<&str> = def
        .actions
        .iter()
        .map(|a| match a {
            Action::Overlay { .. } => "message",
            Action::VoiceAlert { .. } => "voice",
            Action::PlaySound { .. } => "sound",
            Action::StoreVar { .. } => "variable",
        })
        .collect();
    format!("{}  →  {}", watches.join(" & "), does.join(", "))
}

/// Sound packages installed on this machine.
pub fn packages() -> Vec<String> {
    froklog::sound_packages::sound_packages::list_packages()
}

/// The labels a package offers. Triggers reference these names, so this is
/// also the list of sounds worth auditioning before wiring one to a trigger.
pub fn labels(package: &str) -> Vec<String> {
    froklog::sound_packages::sound_packages::load_manifest(package)
        .labels
        .into_iter()
        .map(|l| l.name)
        .collect()
}

/// What is currently being said, so a later alert can interrupt it.
static SPEAKING: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// How long an identical spoken text is suppressed after one starts. Two
/// crits in one flurry say "Critical" once, not a chorus.
const DUP_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
/// Breathing room between DIFFERENT utterances, so "Critical" and
/// "Finishing blow" arrive as two things, not a collision.
const UTTER_GAP: std::time::Duration = std::time::Duration::from_millis(500);
/// A voice queue longer than this is a story about the past; drop new
/// operational chatter instead of narrating history.
const QUEUE_CAP: usize = 5;

/// Owned form of `Voice` so a request can cross into the speaker thread.
enum OwnedVoice {
    SpeechDispatcher,
    Piper(String),
}

impl OwnedVoice {
    fn of(v: &Voice) -> Self {
        match v {
            Voice::SpeechDispatcher => Self::SpeechDispatcher,
            Voice::Piper { model } => Self::Piper(model.to_string()),
        }
    }
    fn as_voice(&self) -> Voice<'_> {
        match self {
            Self::SpeechDispatcher => Voice::SpeechDispatcher,
            Self::Piper(m) => Voice::Piper { model: m },
        }
    }
}

struct SpeakReq {
    text: String,
    priority: VoicePriority,
    voice: OwnedVoice,
}

/// Admission policy for the voice queue — pure, so it is testable.
///
/// `queued` are texts waiting to be spoken; `last_started` is when an
/// identical text last began. Returns whether this request should join.
fn admit(
    text: &str,
    priority: &VoicePriority,
    queued: &std::collections::VecDeque<SpeakReq>,
    last_started: Option<std::time::Instant>,
    speaking_now: bool,
) -> bool {
    // Identical text recently started or already waiting: one is enough.
    if last_started.is_some_and(|t| t.elapsed() < DUP_WINDOW) {
        return false;
    }
    if queued.iter().any(|r| r.text == text) {
        return false;
    }
    match priority {
        // Trivia never queues and never talks over anything.
        VoicePriority::Ambient => !speaking_now && queued.is_empty(),
        VoicePriority::Emergency => true,
        VoicePriority::Operational => queued.len() < QUEUE_CAP,
    }
}

/// The speaker thread: one utterance at a time, a breath between them.
///
/// Piper is a per-utterance process with no queue of its own, so before this
/// existed every Operational alert spawned a fresh pipeline immediately —
/// two crits 200 ms apart talked straight over each other.
fn speaker() -> &'static std::sync::mpsc::Sender<SpeakReq> {
    static TX: std::sync::OnceLock<std::sync::mpsc::Sender<SpeakReq>> = std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<SpeakReq>();
        std::thread::Builder::new()
            .name("voice-queue".into())
            .spawn(move || {
                let mut queue: std::collections::VecDeque<SpeakReq> =
                    std::collections::VecDeque::new();
                let mut last_started: std::collections::HashMap<String, std::time::Instant> =
                    std::collections::HashMap::new();
                loop {
                    // Pull everything pending; block only when idle.
                    if queue.is_empty() {
                        match rx.recv() {
                            Ok(r) => queue.push_back(r),
                            Err(_) => return,
                        }
                    }
                    while let Ok(r) = rx.try_recv() {
                        let speaking = {
                            let mut cur = SPEAKING.lock().unwrap();
                            cur.as_mut()
                                .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
                        };
                        if admit(
                            &r.text,
                            &r.priority,
                            &queue,
                            last_started.get(&r.text).copied(),
                            speaking,
                        ) {
                            if matches!(r.priority, VoicePriority::Emergency) {
                                queue.push_front(r);
                            } else {
                                queue.push_back(r);
                            }
                        }
                    }
                    let Some(req) = queue.pop_front() else {
                        continue;
                    };
                    last_started.insert(req.text.clone(), std::time::Instant::now());
                    last_started.retain(|_, t| t.elapsed() < DUP_WINDOW * 4);
                    speak_now(&req.text, &req.priority, &req.voice.as_voice());
                    // Piper playback is awaited inside speak_now via SPEAKING;
                    // wait for it to finish, then leave the gap.
                    loop {
                        let done = {
                            let mut cur = SPEAKING.lock().unwrap();
                            !cur.as_mut()
                                .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
                        };
                        if done {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    if !queue.is_empty() {
                        std::thread::sleep(UTTER_GAP);
                    }
                }
            })
            .expect("voice-queue thread");
        tx
    })
}

/// Speak an alert, honouring his three priorities.
///
/// Emergency cuts off whatever is talking — an alert you need now is worth
/// losing the last one for. Ambient gives way instead: it is the trivia, and
/// it should never talk over something that mattered. Operational queues,
/// one utterance at a time, duplicates within 2 s collapsed to one.
pub fn speak(text: &str, priority: &VoicePriority, voice: &Voice) -> bool {
    if MUTED.load(Ordering::Relaxed) {
        return false;
    }
    if matches!(priority, VoicePriority::Emergency) {
        // Interrupt NOW, from the caller's thread — the speaker thread may be
        // mid-wait on the current utterance.
        let mut cur = SPEAKING.lock().unwrap();
        if let Some(c) = cur.as_mut() {
            let _ = c.kill();
        }
        *cur = None;
    }
    speaker()
        .send(SpeakReq {
            text: text.to_string(),
            priority: priority.clone(),
            voice: OwnedVoice::of(voice),
        })
        .is_ok()
}

/// Speak regardless of the mute switch (auditions) — direct, unqueued: an
/// audition should play the instant the button is clicked.
pub fn speak_forced(text: &str, priority: &VoicePriority, voice: &Voice) -> bool {
    speak_now(text, priority, voice)
}

/// Actually synthesize and play, immediately, on the calling thread.
fn speak_now(text: &str, priority: &VoicePriority, voice: &Voice) -> bool {
    if text.is_empty() {
        return false;
    }
    let table = load_pronunciations();
    let said = respell(text, &table);
    let text = said.as_str();
    match voice {
        Voice::SpeechDispatcher => {
            // speech-dispatcher implements the same three-way policy itself
            let prio = match priority {
                VoicePriority::Emergency => "important",
                VoicePriority::Operational => "message",
                VoicePriority::Ambient => "notification",
            };
            // spd-say volume runs -100 (silent) to +100; the 0-100 slider
            // maps onto the attenuation half so 100 stays the voice default.
            let vol = (VOLUME.load(Ordering::Relaxed).min(100) as i32 - 100).to_string();
            Command::new("spd-say")
                .args([
                    "--priority",
                    prio,
                    "-i",
                    &vol,
                    "--application-name",
                    "froklog-watch",
                    "--",
                    text,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok()
        }
        Voice::Piper { model } => speak_piper(text, priority, model),
    }
}

/// piper reads text on stdin and writes raw samples on stdout, so it pipes
/// straight into the same audio server the trigger sounds use.
fn speak_piper(text: &str, priority: &VoicePriority, model: &str) -> bool {
    use std::io::Write;

    {
        let mut cur = SPEAKING.lock().unwrap();
        let busy = cur
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        match (busy, priority) {
            (true, VoicePriority::Emergency) => {
                if let Some(c) = cur.as_mut() {
                    let _ = c.kill();
                }
                *cur = None;
            }
            (true, VoicePriority::Ambient) => return false, // never talk over a real alert
            _ => {}
        }
    }

    let rate = piper_sample_rate(model);
    let Ok(mut piper) = Command::new("piper")
        .args(["--model", model, "--output_raw"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = piper.stdin.take() {
        let _ = writeln!(stdin, "{text}");
    }
    let Some(audio) = piper.stdout.take() else {
        return false;
    };
    let played = Command::new("paplay")
        .args([
            "--property=application.name=froklog watch",
            "--raw",
            "--format=s16le",
            "--channels=1",
            &format!("--rate={rate}"),
            &paplay_volume(),
        ])
        .stdin(audio)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match played {
        Ok(child) => {
            *SPEAKING.lock().unwrap() = Some(child);
            true
        }
        Err(_) => false,
    }
}

/// Each piper voice ships a JSON sidecar naming its sample rate; playing at
/// the wrong rate is what makes a voice sound chipmunked or drunk.
fn piper_sample_rate(model: &str) -> u32 {
    let sidecar = format!("{model}.json");
    let Ok(text) = std::fs::read_to_string(sidecar) else {
        return 22050;
    };
    text.split("\"sample_rate\"")
        .nth(1)
        .and_then(|rest| {
            rest.trim_start_matches([':', ' '])
                .split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(22050)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_crit_trigger_covers_all_verbs_and_mobs() {
        // verb = capture, mob words + damage = wildcards, "(Critical)" literal.
        let line = "You strike a greater skeleton for 175 points of damage. (Critical)";
        let tokens = builder::tokenize(line);
        let chosen: std::collections::BTreeSet<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.as_str() == "strike")
            .map(|(i, _)| i)
            .collect();
        let wild: std::collections::BTreeSet<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| ["a", "greater", "skeleton", "175"].contains(&t.as_str()))
            .map(|(i, _)| i)
            .collect();
        let pattern = builder::regex(&tokens, &chosen, &wild);
        let re = regex::Regex::new(&pattern).unwrap();
        // the built-from line matches, with the verb captured
        assert_eq!(&re.captures(line).unwrap()[1], "strike");
        // and so do other verbs, mobs of DIFFERENT word counts, other damage
        let kick = "You kick a rat for 3 points of damage. (Critical)";
        assert_eq!(&re.captures(kick).unwrap()[1], "kick");
        // but a non-crit line does not
        assert!(re
            .captures("You kick a rat for 3 points of damage.")
            .is_none());
    }

    #[test]
    fn builds_a_pattern_from_a_pasted_line() {
        use builder::*;
        let raw = "[Sun Jul 26 19:12:18 2026] Zarri says, 'Hail, Icestorm'";
        // the timestamp must go, or the trigger matches one second of one day
        let line = strip_timestamp(raw);
        assert_eq!(line, "Zarri says, 'Hail, Icestorm'");

        let tokens = tokenize(line);
        // pick the two names
        let chosen: std::collections::BTreeSet<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.as_str() == "Zarri" || t.as_str() == "Icestorm")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(chosen.len(), 2);

        let pattern = regex(&tokens, &chosen, &Default::default());
        // captures where we pointed, literal everywhere else
        assert!(pattern.starts_with(r"^(\w+) says, "), "{pattern}");
        assert!(pattern.ends_with(r"(\w+)'"), "{pattern}");

        // and it has to actually match the line it was built from
        let re = ::regex::Regex::new(&pattern).expect("valid regex");
        let caps = re.captures(line).expect("matches its own line");
        assert_eq!(&caps[1], "Zarri");
        assert_eq!(&caps[2], "Icestorm");
    }

    /// Adding a trigger writes it and it starts matching — the path the
    /// window's Create button takes.
    #[test]
    #[ignore] // rewrites HOME; run alone: cargo test adds_a_trigger -- --ignored
    fn adds_a_trigger_and_it_fires() {
        let tmp = std::env::temp_dir().join("froklog-watch-addtrigger");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);

        let line =
            builder::strip_timestamp("[Sun Jul 26 19:12:18 2026] Zarri has merged an item to +4");
        let tokens = builder::tokenize(line);
        let chosen: std::collections::BTreeSet<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.as_str() == "Zarri" || t.as_str() == "4")
            .map(|(i, _)| i)
            .collect();
        let pattern = builder::regex(&tokens, &chosen, &Default::default());

        let mut alerts = Alerts::load(true);
        alerts.add_trigger("merge", &pattern, "Ding", "{1} merged to plus {2}", "");
        assert_eq!(alerts.count(), 1, "trigger should be in the config");

        // it must survive the round trip through the file
        let reread = TriggerConfig::load();
        assert_eq!(reread.triggers.len(), 1, "trigger should be on disk");

        alerts.test_line(line);
        alerts.pump("default", &Voice::SpeechDispatcher);
        assert!(!alerts.recent.is_empty(), "the trigger should have fired");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The builder only edits an overlay action's TEXT. Icon, colours and
    /// treatment can only be set by hand-editing triggers.toml, so saving an
    /// edit through the builder must carry them through untouched rather
    /// than resetting a carefully styled message to plain white.
    #[test]
    #[ignore] // update_trigger WRITES triggers.toml; run alone: cargo test editing_a_message -- --ignored
    fn editing_a_message_keeps_styling_the_builder_cannot_set() {
        // Every test that reaches a `save()` has to move HOME first, or it
        // overwrites the triggers file of whoever is running the suite.
        let tmp = std::env::temp_dir().join("froklog-watch-msgedit");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);

        // r##: the hex colours below contain `"#`, which would end an r#".
        let cfg: TriggerConfig = toml::from_str(
            r##"
            [[trigger]]
            name = "Train"
            [[trigger.condition]]
            type = "match"
            match_type = "regex"
            pattern = "^TRAIN"
            [[trigger.action]]
            type = "overlay"
            icon = "warn"
            color = "#FF4400"
            message = "TRAIN TO ZONE"
            message_color = "#FFDD44"
            treatment = "vibrate"
            priority = "emergency"
            "##,
        )
        .expect("valid config");
        let mut alerts = Alerts {
            engine: TriggerEngine::new(&cfg, Arc::new(Mutex::new(Vec::new()))),
            queue: Arc::new(Mutex::new(Vec::new())),
            recent: Vec::new(),
            enabled: true,
            cfg,
        };

        let p = alerts.parts(0).expect("one condition, one action");
        assert_eq!(p.show, "TRAIN TO ZONE");
        assert!(p.say.is_empty());

        // Save with only the text changed, as the builder would.
        alerts.update_trigger(0, "Train", "^TRAIN", "", "", "TRAIN INCOMING");
        match &alerts.triggers()[0].actions[0] {
            Action::Overlay {
                icon,
                color,
                message,
                message_color,
                treatment,
                priority,
                ..
            } => {
                assert_eq!(message, "TRAIN INCOMING", "the text is the edit");
                assert_eq!(icon, "warn", "icon survived");
                assert_eq!(color, "#FF4400", "accent survived");
                assert_eq!(message_color, "#FFDD44", "text colour survived");
                assert_eq!(
                    *treatment,
                    froklog::triggers::engine::Treatment::Vibrate,
                    "treatment survived"
                );
                assert_eq!(*priority, VoicePriority::Emergency, "priority survived");
            }
            other => panic!("expected an overlay action, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A regex pasted into the log-line box has to be caught: the builder
    /// escapes it into a pattern that matches literal backslashes, looks
    /// plausible in the field, and never fires. Real log lines must not
    /// trip the check.
    #[test]
    fn a_pasted_regex_is_not_mistaken_for_a_log_line() {
        assert!(builder::looks_like_regex(
            r"^You (\w+) .+? for \d+ points of damage\. \(Critical\)"
        ));
        assert!(builder::looks_like_regex(r"(\w+) says, '.+?'"));
        assert!(!builder::looks_like_regex(
            "You slash a greater skeleton for 96 points of damage. (Critical)"
        ));
        assert!(!builder::looks_like_regex(
            "[Fri Jul 31 08:48:45 2026] Zarri has merged an item to +4"
        ));
        assert!(!builder::looks_like_regex(""));
    }

    /// Editing an existing trigger: it comes back apart into the builder's
    /// four fields, a broadened pattern saves over it in place, and the new
    /// pattern is what fires. This is the over-literal-pattern repair the
    /// builder exists for — the mob name and damage number were baked in.
    #[test]
    #[ignore] // rewrites HOME; run alone: cargo test edits_a_trigger -- --ignored
    fn edits_a_trigger_in_place() {
        let tmp = std::env::temp_dir().join("froklog-watch-edittrigger");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);

        let mut alerts = Alerts::load(true);
        alerts.add_trigger(
            "Critical",
            r"^You (\w+) Emperor Crush for 37 points of damage\. \((\w+)\)",
            "",
            "Critical",
            "",
        );
        alerts.set_enabled(0, false);

        // it comes back apart the way the builder needs it
        let p = alerts.parts(0).expect("builder can hold it");
        assert_eq!(p.name, "Critical");
        assert_eq!(p.say, "Critical");
        assert!(p.sound.is_empty());
        assert!(p.show.is_empty());
        assert!(p.pattern.contains("Emperor Crush"), "{}", p.pattern);

        // broaden it the way the word picker's * state would
        alerts.update_trigger(
            0,
            "Critical",
            r"^You (\w+) .+? for \d+ points of damage\. \(Critical\)",
            "",
            "{1}",
            "CRIT {1}",
        );
        assert_eq!(alerts.count(), 1, "edit replaces, it does not append");
        assert!(
            !alerts.triggers()[0].enabled,
            "editing must not silently re-enable a trigger that was ticked off"
        );

        // and the edit is what is on disk and what matches now
        let reread = TriggerConfig::load();
        assert_eq!(reread.triggers.len(), 1);
        alerts.set_enabled(0, true);
        alerts.test_line(builder::strip_timestamp(
            "[Fri Jul 31 08:50:05 2026] You smite a greater skeleton for 47 points of damage. (Critical)",
        ));
        alerts.pump("default", &Voice::SpeechDispatcher);
        assert!(
            !alerts.recent.is_empty(),
            "the broadened trigger should fire on a different mob and damage"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The full path a user walks: paste a line, click two words, write a
    /// spoken template referring to them, and hear the words come back.
    #[test]
    fn captures_come_back_as_numbered_placeholders() {
        let line =
            builder::strip_timestamp("[Sun Jul 26 19:12:18 2026] Zarri has merged an item to +4");
        let tokens = builder::tokenize(line);
        let chosen: std::collections::BTreeSet<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.as_str() == "Zarri" || t.as_str() == "4")
            .map(|(i, _)| i)
            .collect();
        let pattern = builder::regex(&tokens, &chosen, &Default::default());

        let cfg: TriggerConfig = toml::from_str(&format!(
            r#"
            [[trigger]]
            name = "merge"
            [[trigger.condition]]
            type = "match"
            match_type = "regex"
            pattern = {pattern:?}
            [[trigger.action]]
            type = "voice_alert"
            tts_text = "{{1}} merged an item to plus {{2}}"
            "#
        ))
        .expect("config parses");

        let queue = Arc::new(Mutex::new(Vec::new()));
        let engine = TriggerEngine::new(&cfg, Arc::clone(&queue));
        engine.process_line(line);
        engine.tick();

        let fired = queue.lock().unwrap();
        assert_eq!(
            fired.first().and_then(|e| e.tts_text.as_deref()),
            Some("Zarri merged an item to plus 4")
        );
    }

    #[test]
    fn respelling_matches_whole_words_only() {
        let table = vec![
            ("Cazic-Thule".to_string(), "Kay-zick Thool".to_string()),
            ("Erudin".to_string(), "Air-oo-din".to_string()),
        ];
        assert_eq!(
            respell("Cazic-Thule hits you", &table),
            "Kay-zick Thool hits you"
        );
        // case-insensitive
        assert_eq!(respell("erudin guard", &table), "Air-oo-din guard");
        // but not inside a longer word, or every name containing it breaks
        assert_eq!(respell("Erudinite", &table), "Erudinite");
    }

    /// Makes a noise, so it only runs when asked for:
    ///   cargo test -- --ignored
    #[test]
    #[ignore]
    fn plays_the_ding_label_from_the_default_package() {
        let played = play("Ding", "default");
        assert!(played.is_some(), "no sound package installed?");
        assert!(played.unwrap().ends_with("ding.wav"));
        assert!(speak(
            "trigger test",
            &VoicePriority::Operational,
            &Voice::SpeechDispatcher
        ));
    }

    /// The whole point of reusing his engine is that a triggers.toml written
    /// for the Windows client behaves the same here — matching, captures and
    /// templates included. This asserts that end of the contract; the audio
    /// side is a process spawn and is verified by hand.
    #[test]
    fn a_matching_line_produces_a_message_and_speech() {
        let cfg: TriggerConfig = toml::from_str(
            r#"
            [[trigger]]
            name = "hail"
            [[trigger.condition]]
            type = "match"
            match_type = "regex"
            pattern = "^(\\w+) says, 'Hail, (\\w+)'"
            [[trigger.action]]
            type = "overlay"
            message = "{1} hailed {2}"
            [[trigger.action]]
            type = "voice_alert"
            tts_text = "{1} is hailing you"
            "#,
        )
        .expect("test config parses");

        let queue = Arc::new(Mutex::new(Vec::new()));
        let engine = TriggerEngine::new(&cfg, Arc::clone(&queue));
        engine.process_line("Zarri says, 'Hail, Icestorm'");
        engine.tick();

        let fired = queue.lock().unwrap();
        assert_eq!(fired.len(), 2, "one overlay action and one voice action");
        assert_eq!(fired[0].message, "Zarri hailed Icestorm");
        assert_eq!(fired[1].tts_text.as_deref(), Some("Zarri is hailing you"));
    }
}

#[cfg(test)]
mod voice_queue_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    fn req(text: &str) -> SpeakReq {
        SpeakReq {
            text: text.into(),
            priority: VoicePriority::Operational,
            voice: OwnedVoice::SpeechDispatcher,
        }
    }

    /// "critical | critical" collapses to one: an identical text that started
    /// within the window, or is already waiting, is not said again.
    #[test]
    fn duplicates_within_the_window_collapse() {
        let q = VecDeque::new();
        let just_started = Some(Instant::now());
        assert!(!admit(
            "Critical",
            &VoicePriority::Operational,
            &q,
            just_started,
            true
        ));

        let long_ago = Some(Instant::now() - Duration::from_secs(3));
        assert!(admit(
            "Critical",
            &VoicePriority::Operational,
            &q,
            long_ago,
            false
        ));

        let mut q2 = VecDeque::new();
        q2.push_back(req("Critical"));
        assert!(
            !admit("Critical", &VoicePriority::Operational, &q2, None, true),
            "already queued: one is enough"
        );
    }

    /// "critical | finishing blow" both get through — distinct texts queue,
    /// the scheduler paces them with the gap.
    #[test]
    fn distinct_texts_both_queue() {
        let mut q = VecDeque::new();
        assert!(admit(
            "Critical",
            &VoicePriority::Operational,
            &q,
            None,
            false
        ));
        q.push_back(req("Critical"));
        assert!(admit(
            "Finishing blow, 441",
            &VoicePriority::Operational,
            &q,
            None,
            true
        ));
    }

    /// Ambient never talks over anything and never queues; emergency always
    /// gets in; operational stops joining once the queue is a backlog.
    #[test]
    fn priorities_keep_their_meaning() {
        let mut q = VecDeque::new();
        assert!(!admit("trivia", &VoicePriority::Ambient, &q, None, true));
        assert!(admit("trivia", &VoicePriority::Ambient, &q, None, false));

        for i in 0..QUEUE_CAP {
            q.push_back(req(&format!("msg {i}")));
        }
        assert!(!admit("late", &VoicePriority::Operational, &q, None, true));
        assert!(admit("TRAIN", &VoicePriority::Emergency, &q, None, true));
    }
}
