/// Queue/priority/TTS/phase-timing engine shared by every window that
/// presents trigger alerts — today the standalone alert window
/// (`overlay.rs`) and the merged alert+history window (`overlay_merged.rs`).
/// This is the half of the old `overlay.rs::OverlayState` that doesn't know
/// about window geometry: it decides *which* alert is active and *when* it
/// moves between phases, not how a phase renders on screen. Each caller
/// drives its own `slint::Timer`, calls `tick()` once per frame, and does
/// its own pixel math against the returned `ActiveAlert`.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod alert_engine {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::overlay_history::overlay_history::HistoryEntry;
    use crate::tray::tray::AppHandle;
    use crate::triggers::engine::{OverlayEvent, Treatment, VoicePriority};

    /// Ambient-priority events are dropped instead of queued once this many
    /// events are already waiting — stale low-priority info showing late is
    /// worse than not showing at all.
    const AMBIENT_DROP_QUEUE_LEN: usize = 2;
    /// Hold duration never shrinks below this when a backlog is draining it.
    const MIN_HOLD_SECS: f32 = 0.6;

    #[derive(Clone, Copy, PartialEq)]
    pub enum AlertPhase {
        FlyIn,
        Hold,
        ShrinkOut,
    }

    pub struct ActiveAlert {
        pub event: OverlayEvent,
        pub phase: AlertPhase,
        pub phase_started: Instant,
        /// True for the synthetic alert injected by the Settings dialog's
        /// "Show All Windows" button — held in `Hold` indefinitely (instead
        /// of aging out to `ShrinkOut` and landing in history) for as long
        /// as `force_show_windows` stays set.
        pub is_placeholder: bool,
    }

    pub struct AlertEngine {
        handle: Arc<AppHandle>,
        // Which config fields `fly_ms`/`hold_secs` are read from — the
        // standalone alert window's `overlay_fly_ms`/`overlay_hold_secs`, or
        // the Combined overlay's own independent `overlay_merged_fly_ms`/
        // `overlay_merged_hold_secs`. Set once at construction from which
        // window created this engine; never changes for the engine's
        // lifetime, unlike the timing values themselves.
        merged: bool,
        queue: Vec<OverlayEvent>,
        active: Option<ActiveAlert>,
        fly_ms: u32,
        hold_secs: f32,
        tts: Option<crate::tts::tts::PiperEngine>,
        tts_voice_name: String,
    }

    impl AlertEngine {
        /// `merged` — see the `AlertEngine::merged` field doc comment.
        pub fn new(handle: &Arc<AppHandle>, merged: bool) -> Self {
            let cfg = handle.config.lock().unwrap();
            let (fly_ms, hold_secs) = if merged {
                (cfg.overlay_merged_fly_ms, cfg.overlay_merged_hold_secs)
            } else {
                (cfg.overlay_fly_ms, cfg.overlay_hold_secs)
            };
            Self {
                handle: Arc::clone(handle),
                merged,
                queue: Vec::new(),
                active: None,
                fly_ms: fly_ms.max(16),
                hold_secs: hold_secs.max(0.0),
                tts: None,
                tts_voice_name: cfg.tts_voice.clone(),
            }
        }

        pub fn fly_ms(&self) -> u32 {
            self.fly_ms
        }

        /// Reload live-tunable settings from config.
        pub fn sync_config(&mut self) {
            let cfg = self.handle.config.lock().unwrap();
            let new_voice_name = cfg.tts_voice.clone();
            let (fly_ms, hold_secs) = if self.merged {
                (cfg.overlay_merged_fly_ms, cfg.overlay_merged_hold_secs)
            } else {
                (cfg.overlay_fly_ms, cfg.overlay_hold_secs)
            };
            self.fly_ms = fly_ms.max(16);
            self.hold_secs = hold_secs.max(0.0);
            drop(cfg);

            // If the selected voice changed, drop the stale engine so the
            // next try_speak() lazily reloads it under the new voice.
            // Unlike the old `tts` crate's set_voice() — a cheap property
            // set on an already-running OS engine — loading a piper model
            // takes ~1s, so that cost stays out of the tick loop and is
            // only paid right before the next utterance actually needs it.
            if new_voice_name != self.tts_voice_name {
                self.tts = None;
                self.tts_voice_name = new_voice_name;
            }
        }

        /// Advances the queue/phase state machine one tick and returns the
        /// currently active alert (if any) for the caller to render.
        pub fn tick(&mut self) -> Option<&ActiveAlert> {
            self.drain_queue();
            self.tick_state();
            self.active.as_ref()
        }

        fn drain_queue(&mut self) {
            let new_events: Vec<OverlayEvent> = {
                let mut q = self.handle.overlay_queue.lock().unwrap();
                q.drain(..).collect()
            };
            if !new_events.is_empty() {
                tracing::info!(
                    "alert engine: drained {} event(s) from trigger engine",
                    new_events.len()
                );
            }
            for ev in new_events {
                if let Some(ref text) = ev.tts_text {
                    self.try_speak(text, &ev.tts_priority, ev.is_test);
                }

                if !ev.message.is_empty() {
                    match ev.priority {
                        VoicePriority::Emergency => {
                            // Interrupt whatever's currently showing — it goes back to
                            // the front of the queue to resume (from scratch) right
                            // after this one, rather than being lost.
                            if let Some(active) = self.active.take() {
                                self.queue.insert(0, active.event);
                            }
                            self.queue.insert(0, ev);
                        }
                        VoicePriority::Ambient if self.queue.len() >= AMBIENT_DROP_QUEUE_LEN => {
                            // Backed up already — drop rather than pile on stale info.
                        }
                        VoicePriority::Ambient | VoicePriority::Operational => {
                            self.queue.push(ev);
                        }
                    }
                } else if let Some(s) = ev.sound.as_deref().filter(|s| !s.is_empty()) {
                    crate::overlay::overlay::play_sound_label(s);
                }
            }
        }

        /// Advance the single-message state machine one tick.
        fn tick_state(&mut self) {
            let now = Instant::now();
            let force_show = self.handle.force_show_windows.load(Ordering::Relaxed);

            // Placeholder injected for "Show All Windows": drop it the moment
            // the button is released.
            if let Some(active) = &self.active {
                if active.is_placeholder && !force_show {
                    self.active = None;
                }
            }

            if self.active.is_none() {
                // Show All Windows always wins the display slot immediately
                // when nothing's currently active, even with a real trigger
                // backlog waiting — it's a deliberate positioning pause, not
                // something that should have to wait its turn behind
                // whatever the trigger engine queued up in the meantime.
                // The backlog isn't lost — it resumes the moment force_show
                // goes false again.
                if force_show {
                    self.active = Some(ActiveAlert {
                        event: OverlayEvent {
                            icon: String::new(),
                            color: String::new(),
                            message: "Drag me".into(),
                            message_color: String::new(),
                            border_color: String::new(),
                            sound: None,
                            tts_text: None,
                            tts_priority: VoicePriority::default(),
                            treatment: Treatment::default(),
                            priority: VoicePriority::default(),
                            is_test: false,
                        },
                        phase: AlertPhase::Hold,
                        phase_started: now,
                        is_placeholder: true,
                    });
                } else if !self.queue.is_empty() {
                    let event = self.queue.remove(0);
                    self.active = Some(ActiveAlert {
                        event,
                        phase: AlertPhase::FlyIn,
                        phase_started: now,
                        is_placeholder: false,
                    });
                } else {
                    return;
                }
            }

            let fly_dur = Duration::from_millis(self.fly_ms as u64);
            let active = self.active.as_ref().unwrap();
            if active.is_placeholder {
                return;
            }
            let elapsed = now.duration_since(active.phase_started);
            let phase = active.phase;
            let dur = match phase {
                AlertPhase::FlyIn | AlertPhase::ShrinkOut => fly_dur,
                // Shorten the hold when a backlog is waiting, so queued messages
                // don't sit behind a full-length hold on the current one.
                AlertPhase::Hold => self.effective_hold_duration(),
            };
            if elapsed < dur {
                return;
            }

            match phase {
                AlertPhase::FlyIn => {
                    let active = self.active.as_mut().unwrap();
                    active.phase = AlertPhase::Hold;
                    active.phase_started = now;
                }
                AlertPhase::Hold => {
                    let active = self.active.as_mut().unwrap();
                    active.phase = AlertPhase::ShrinkOut;
                    active.phase_started = now;
                }
                AlertPhase::ShrinkOut => {
                    let done = self.active.take().unwrap();
                    let mut hist = self.handle.overlay_history.lock().unwrap();
                    hist.push(HistoryEntry::new(
                        done.event.icon,
                        done.event.color,
                        done.event.message,
                        done.event.message_color,
                        done.event.border_color,
                    ));
                }
            }
        }

        /// Hold duration for the current message, shortened when other
        /// messages are already waiting so the queue drains faster. Never
        /// drops below `MIN_HOLD_SECS` so a message is always readable.
        fn effective_hold_duration(&self) -> Duration {
            let backlog = self.queue.len();
            if backlog == 0 {
                return Duration::from_secs_f32(self.hold_secs);
            }
            let scaled = self.hold_secs / (1.0 + backlog as f32 * 0.5);
            let floor = MIN_HOLD_SECS.min(self.hold_secs);
            Duration::from_secs_f32(scaled.clamp(floor, self.hold_secs))
        }

        fn try_speak(&mut self, text: &str, priority: &VoicePriority, is_test: bool) {
            use crate::config::TtsAudioMode;

            let (enabled, speed, mode, re, ro, ra) = {
                let cfg = self.handle.config.lock().unwrap();
                (
                    cfg.tts_enabled,
                    cfg.tts_speed.clone(),
                    cfg.tts_audio_mode.clone(),
                    cfg.tts_read_emergency,
                    cfg.tts_read_operational,
                    cfg.tts_read_ambient,
                )
            };

            // Test-button events bypass the mute settings entirely — same
            // reasoning as `preview_sound_label` ignoring "Sound enabled": a
            // muted/disabled setting shouldn't make Test look silently broken.
            if !is_test {
                if !enabled {
                    return;
                }
                match priority {
                    VoicePriority::Emergency if !re => return,
                    VoicePriority::Operational if !ro => return,
                    VoicePriority::Ambient if !ra => return,
                    _ => {}
                }
            }

            if self.tts.is_none() {
                self.tts = crate::tts::tts::PiperEngine::load(&self.tts_voice_name);
            }
            let Some(ref mut engine) = self.tts else {
                return;
            };

            let interrupt = match mode {
                TtsAudioMode::InterruptConstantly => true,
                TtsAudioMode::QueueAll => false,
                TtsAudioMode::SmartPriority => match priority {
                    VoicePriority::Emergency => true,
                    VoicePriority::Ambient => {
                        if engine.is_speaking() {
                            return;
                        }
                        false
                    }
                    _ => false,
                },
            };

            engine.speak(text, interrupt, crate::tts::tts::length_scale_for(&speed));
        }
    }
}
