/// Neural text-to-speech via piper-rs (onnxruntime), replacing the old
/// `tts` crate (SAPI on Windows, speech-dispatcher/espeak-ng on Linux) with
/// one bundled voice plus a small downloadable catalog — see
/// `assets::bundled_voices_dir()`/`downloaded_voices_dir()` for where voice
/// files live, and `main.rs` for the `ORT_DYLIB_PATH` startup wiring this
/// depends on.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod tts {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use rodio::{OutputStream, Sink};

    use crate::config::{Config, TtsSpeed};

    /// Piper's `length_scale` is a duration multiplier (lower = faster),
    /// the opposite sense of the old `tts` crate's `normal_rate()`..`
    /// max_rate()` interpolation this replaces (see the old
    /// `overlay::apply_tts_speed` in git history) — `Normal` sits at
    /// piper's own default (1.0) same as before, `Fastest` is a hand-picked
    /// value rather than an engine-reported max, since piper has no
    /// equivalent concept.
    pub fn length_scale_for(speed: &TtsSpeed) -> f32 {
        match speed {
            TtsSpeed::Normal => 1.0,
            TtsSpeed::Fast => 0.85,
            TtsSpeed::Faster => 0.7,
            TtsSpeed::Fastest => 0.55,
        }
    }

    /// A loaded voice plus a persistent playback sink. Model load is the
    /// expensive part (order of a second) — kept alive across calls rather
    /// than reloaded per utterance, same lazy-init-once shape as the old
    /// `tts::Tts` field it replaces in `alert_engine.rs`. The sink is
    /// likewise long-lived so `speak()`'s "queue" mode can just
    /// `Sink::append` (rodio plays queued sources back-to-back
    /// automatically) instead of needing to manage a queue by hand.
    pub struct PiperEngine {
        voice_name: String,
        piper: piper_rs::Piper,
        _stream: OutputStream,
        sink: Sink,
    }

    impl PiperEngine {
        pub fn load(voice_name: &str) -> Option<Self> {
            tracing::warn!("TTS: locating voice files for {voice_name:?}");
            let Some((onnx_path, config_path)) = voice_paths(voice_name) else {
                tracing::warn!(
                    "TTS: no voice files found for {voice_name:?} (checked {:?} and {:?})",
                    crate::assets::bundled_voices_dir(),
                    crate::assets::downloaded_voices_dir()
                );
                return None;
            };
            tracing::warn!(
                "TTS: found voice files ({onnx_path:?}, {config_path:?}), calling Piper::new \
                (ORT_DYLIB_PATH={:?})",
                std::env::var("ORT_DYLIB_PATH")
            );
            let piper = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                piper_rs::Piper::new(&onnx_path, &config_path)
            })) {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => {
                    tracing::warn!(
                        "TTS: Piper::new failed for {voice_name:?} from {onnx_path:?}: {e:?}"
                    );
                    return None;
                }
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "<non-string panic payload>".to_string());
                    tracing::error!("TTS: Piper::new PANICKED for {voice_name:?}: {msg}");
                    return None;
                }
            };
            tracing::warn!("TTS: Piper::new returned, opening default audio output device");
            let (stream, handle) = match OutputStream::try_default() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("TTS: failed to open default audio output device: {e:?}");
                    return None;
                }
            };
            tracing::warn!("TTS: audio output opened, creating sink");
            let sink = match Sink::try_new(&handle) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("TTS: failed to create audio sink: {e:?}");
                    return None;
                }
            };
            tracing::warn!("TTS: engine fully loaded for {voice_name:?}");
            Some(Self {
                voice_name: voice_name.to_string(),
                piper,
                _stream: stream,
                sink,
            })
        }

        pub fn voice_name(&self) -> &str {
            &self.voice_name
        }

        /// Whether audio is still queued/playing — the piper-side analogue
        /// of `tts::Tts::is_speaking()`, used by `SmartPriority` mode to
        /// decide whether an Ambient alert should be dropped rather than
        /// interrupt or queue behind what's already playing.
        pub fn is_speaking(&self) -> bool {
            !self.sink.empty()
        }

        /// Synthesizes `text` and plays it. `interrupt: true` stops
        /// whatever's currently playing first (Emergency/InterruptConstantly
        /// callers); `false` queues behind it (QueueAll, or SmartPriority's
        /// Operational/Emergency-while-idle cases) via the sink's own
        /// playback queue. `volume` is a 0.0-1.0 multiplier from
        /// `Config.tts_volume`, independent of `Config.sound_volume` — the
        /// sink is long-lived (see the struct doc comment), so this is
        /// re-applied on every call rather than once at load time, letting a
        /// live Settings change take effect on the next utterance.
        pub fn speak(&mut self, text: &str, interrupt: bool, length_scale: f32, volume: f32) {
            let (samples, sample_rate) =
                match self
                    .piper
                    .create(text, false, None, Some(length_scale), None, None)
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(
                            "TTS: synthesis failed for voice {:?}: {e:?}",
                            self.voice_name
                        );
                        return;
                    }
                };
            if interrupt {
                self.sink.stop();
            }
            self.sink.set_volume(volume);
            tracing::warn!(
                "TTS: synthesized {} samples @ {sample_rate}Hz for voice {:?}, handing to audio sink",
                samples.len(),
                self.voice_name
            );
            self.sink
                .append(rodio::buffer::SamplesBuffer::new(1, sample_rate, samples));
        }
    }

    /// Locates a voice's `.onnx`/`.onnx.json` pair — the bundled voice
    /// directory first, then user-downloaded voices.
    fn voice_paths(voice_name: &str) -> Option<(PathBuf, PathBuf)> {
        for dir in [
            crate::assets::bundled_voices_dir(),
            crate::assets::downloaded_voices_dir(),
        ] {
            let onnx = dir.join(format!("{voice_name}.onnx"));
            let config = dir.join(format!("{voice_name}.onnx.json"));
            if onnx.is_file() && config.is_file() {
                return Some((onnx, config));
            }
        }
        None
    }

    /// Enumerate installed voices — bundled + downloaded — as
    /// `(display_name, voice_name)` pairs, sorted and deduplicated (a voice
    /// downloaded to `downloaded_voices_dir()` that happens to also be the
    /// bundled one only appears once).
    ///
    /// Replaces the old `enumerate_tts_voices()`, which shelled out to
    /// speech-dispatcher over IPC and could hang forever if that daemon was
    /// wedged (see the 10s-watchdog machinery this obsoletes in
    /// `settings_window.rs`) — this is a synchronous local directory scan,
    /// so it can't wedge and doesn't need a background thread or timeout.
    pub fn enumerate_voices() -> Vec<(String, String)> {
        let mut voices: Vec<(String, String)> = [
            crate::assets::bundled_voices_dir(),
            crate::assets::downloaded_voices_dir(),
        ]
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let voice_name = name.strip_suffix(".onnx.json")?;
            path.with_file_name(format!("{voice_name}.onnx"))
                .is_file()
                .then(|| (display_name(voice_name), voice_name.to_string()))
        })
        .collect();
        voices.sort();
        voices.dedup_by(|a, b| a.1 == b.1);
        voices
    }

    /// "en_US-amy-low" -> "Amy (en_US, low)" — friendlier than the raw
    /// piper voice id for the Settings dropdown, and reused as-is for the
    /// voice-download catalog panel's row labels (see
    /// `settings_window.rs`'s `voice_catalog_rows`). Falls back to the raw
    /// id for anything that doesn't match piper's
    /// `<lang>-<name>-<quality>` naming convention rather than hiding it
    /// from the list.
    pub fn display_name(voice_name: &str) -> String {
        let parts: Vec<&str> = voice_name.splitn(3, '-').collect();
        match parts.as_slice() {
            [lang, name, quality] => {
                let mut c = name.chars();
                let name = match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => name.to_string(),
                };
                format!("{name} ({lang}, {quality})")
            }
            _ => voice_name.to_string(),
        }
    }

    /// Speaks `text` via a throw-away engine using the currently saved
    /// voice/speed, bypassing the "TTS enabled" and per-priority read-aloud
    /// filters `AlertEngine::try_speak` applies — used by the action
    /// editor's Voice Alert ▶ preview button, so users can audition an
    /// alert even while TTS or that priority's read-aloud is disabled.
    pub fn preview_speak(text: &str) {
        let cfg = Config::load();
        let text = text.to_string();
        tracing::warn!("TTS: preview triggered for voice {:?}", cfg.tts_voice);
        std::thread::spawn(move || {
            let Some(mut engine) = PiperEngine::load(&cfg.tts_voice) else {
                return;
            };
            engine.speak(
                &text,
                true,
                length_scale_for(&cfg.tts_speed),
                cfg.tts_volume as f32 / 100.0,
            );

            // Block until playback finishes so this throw-away engine (and
            // its OutputStream) isn't dropped mid-utterance.
            let deadline = Instant::now() + Duration::from_secs(30);
            while engine.is_speaking() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }
}
