/// Registry of overlay window kinds. Each kind owns its own
/// `create_*_window` function and `OverlayWindowConfig` chrome (visibility,
/// lock state, position) on `Config`; this module is the one place that
/// knows about all of them, so spawning and shared Settings-UI wiring don't
/// need to be hand-duplicated per window.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_registry {
    use std::sync::Arc;

    use crate::config::{Config, OverlayWindowConfig};
    use crate::tray::tray::AppHandle;

    /// One overlay window kind. `Alert` and `Merged` both present trigger
    /// alerts but are mutually exclusive — see `Config::alert_style` — so at
    /// most one of them is ever actively drawing from the trigger engine's
    /// event queue, even though (matching the other kinds) both windows are
    /// always created and only their visibility toggles.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum OverlayKind {
        Alert,
        History,
        Meter,
        Merged,
    }

    impl OverlayKind {
        pub const ALL: [OverlayKind; 4] = [
            OverlayKind::Alert,
            OverlayKind::History,
            OverlayKind::Meter,
            OverlayKind::Merged,
        ];

        pub fn label(&self) -> &'static str {
            match self {
                OverlayKind::Alert => "Alert",
                OverlayKind::History => "History",
                OverlayKind::Meter => "DPS Meter",
                OverlayKind::Merged => "Combined Alert + History",
            }
        }

        pub fn config<'a>(&self, cfg: &'a Config) -> &'a OverlayWindowConfig {
            match self {
                OverlayKind::Alert => &cfg.overlay_alert,
                OverlayKind::History => &cfg.overlay_history,
                OverlayKind::Meter => &cfg.overlay_meter,
                OverlayKind::Merged => &cfg.overlay_merged,
            }
        }

        pub fn config_mut<'a>(&self, cfg: &'a mut Config) -> &'a mut OverlayWindowConfig {
            match self {
                OverlayKind::Alert => &mut cfg.overlay_alert,
                OverlayKind::History => &mut cfg.overlay_history,
                OverlayKind::Meter => &mut cfg.overlay_meter,
                OverlayKind::Merged => &mut cfg.overlay_merged,
            }
        }
    }

    /// Creates every overlay window, unconditionally — matching the
    /// existing "always alive, gated by its own `enabled` flag" idiom, which
    /// is what lets Settings toggle a window's visibility live (no restart)
    /// and lets "Show All Windows" preview any of them on demand. Must run
    /// on the Slint UI thread — see `tray.rs`'s module doc.
    pub fn spawn_all(handle: &Arc<AppHandle>) {
        crate::overlay::overlay::create_alert_window(Arc::clone(handle));
        crate::overlay_history::overlay_history::create_history_window(Arc::clone(handle));
        crate::overlay_dps::overlay_dps::create_meter_window(Arc::clone(handle));
        crate::overlay_merged::overlay_merged::create_merged_window(Arc::clone(handle));
    }
}
