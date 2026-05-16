//! Persisted user settings.
//!
//! Uses cosmic-config so settings end up under the standard
//! `~/.config/cosmic/com.system76.CosmicCapture/v1/` tree (or wherever
//! cosmic-config is rooted). The derive macro on [`UserSettings`] generates
//! `set_*` mutators that write to disk on change.

use cosmic_config::{Config, CosmicConfigEntry};
use cosmic_config::cosmic_config_derive::CosmicConfigEntry as CosmicConfigEntryDerive;

use super::app::{Mode, RecordFormat, RecordFps, SaveTarget, Source};
use super::widget::SelectionRect;

const APP_ID: &str = "com.system76.CosmicCapture";
// Version must match the `#[version = N]` attribute on `UserSettings` below.
// cosmic-config namespaces stored entries by version so future schema changes
// can land without colliding with what's on disk.
const CONFIG_VERSION: u64 = <UserSettings as CosmicConfigEntry>::VERSION;

#[derive(Clone, Debug, Default, PartialEq, Eq, CosmicConfigEntryDerive)]
#[version = 1]
pub struct UserSettings {
    pub mode: Mode,
    pub source: Source,
    pub save_target: SaveTarget,
    pub record_format: RecordFormat,
    pub record_fps: RecordFps,
    /// Last region the user dragged out. Persisted so re-launching after a
    /// quick capture restores the same rect rather than forcing them to redraw.
    pub last_region: Option<SelectionRect>,
}

impl UserSettings {
    /// Opens (or creates) the cosmic-config store and loads the saved entry.
    /// Falls back to defaults if cosmic-config init fails — that's not fatal,
    /// the user just doesn't get persistence this session.
    pub fn load() -> (Self, Option<Config>) {
        match Config::new(APP_ID, CONFIG_VERSION) {
            Ok(handler) => {
                let settings = Self::get_entry(&handler).unwrap_or_else(|(errs, fallback)| {
                    for e in errs {
                        tracing::warn!(error = %e, "cosmic-config load");
                    }
                    fallback
                });
                (settings, Some(handler))
            }
            Err(e) => {
                tracing::warn!(error = %e, "cosmic-config init failed; settings won't persist");
                (Self::default(), None)
            }
        }
    }
}
