//! Paths to the icons/sounds directories, plus the legacy stock-icon
//! filename list.

use std::path::PathBuf;

pub fn icons_dir() -> PathBuf {
    data_dir().join("icons")
}

pub fn sounds_dir() -> PathBuf {
    data_dir().join("sounds")
}

/// Where the bundled onnxruntime shared library
/// (`onnxruntime.dll`/`libonnxruntime.so`) lives — always beside the
/// executable, on every platform. Unlike `sounds_dir()`/`icons_dir()`, this
/// isn't XDG-style user data: it's installed, read-only, versioned-with-the-
/// binary content, same category as the app itself, so it doesn't follow
/// `data_dir()`'s Linux/Windows split.
pub fn runtime_dir() -> PathBuf {
    exe_dir_any_platform().join("runtime")
}

/// The one voice shipped with every install, so TTS works with no
/// first-run download. Same exe-relative reasoning as `runtime_dir()`.
pub fn bundled_voices_dir() -> PathBuf {
    exe_dir_any_platform().join("voices")
}

/// Voices the user downloaded from Settings' voice catalog — this *is* the
/// kind of mutable user data `sounds_dir()`/`icons_dir()` already model, so
/// it reuses `data_dir()`'s existing XDG-on-Linux/exe-relative-on-Windows
/// split rather than `runtime_dir()`/`bundled_voices_dir()`'s always-exe-
/// relative one.
pub fn downloaded_voices_dir() -> PathBuf {
    data_dir().join("voices")
}

/// Sidecar file `extract_spell_icons` (spell_icons.rs) writes into
/// `icons_dir()` alongside the extracted PNGs: one
/// `filename<TAB>source<TAB>spell_name<TAB>all_spell_names` line per icon,
/// where `source` is the exact (unsanitized) `uifiles` subdirectory name it
/// came from, `spell_name` (may be empty) is the first spell in
/// `spells_us.txt` found pointing at that icon's global icon id (used as
/// the display label), and `all_spell_names` is every distinct spell
/// pointing at that same icon id, `|`-joined (used so searching by any of
/// them finds the icon, not just the first). The PNG filenames themselves
/// only carry a *sanitized* version of the source name (safe characters
/// only), which isn't reversible if the real directory name had e.g.
/// spaces in it — this manifest is what lets the action editor's icon
/// picker filter by the real source name and label icons by spell name
/// instead of guessing either back out of a filename.
pub const SPELL_ICON_MANIFEST_FILE: &str = "spell_icons_manifest.txt";

/// Where icons and sound packages live.
///
/// On Windows that is beside the executable, which is what a portable app
/// wants and what every existing install already has. Elsewhere the binary
/// usually sits in a directory meant only for executables — ~/.local/bin —
/// so writing sound packages there would be wrong; XDG says data belongs in
/// ~/.local/share instead.
fn data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        exe_dir_any_platform()
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                    .join(".local")
                    .join("share")
            })
            .join("froklog")
    }
}

/// The executable's own directory, on any platform — unlike `data_dir()`,
/// which only uses this on Windows (see its doc comment), this is for
/// content that belongs beside the binary regardless of platform: bundled
/// runtime libraries and the default voice, not XDG-style user data.
fn exe_dir_any_platform() -> PathBuf {
    std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("."))
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf()
}

/// Returns a COLORREF (0x00BBGGRR) swatch colour for a named icon key.
/// Used by both the config-UI owner-draw combo and the overlay fallback renderer.
pub fn icon_swatch_color(key: &str) -> u32 {
    // COLORREF layout: R | (G << 8) | (B << 16)
    match key {
        "heal" => 0x0033CC33,          // green   R=51  G=204 B=51
        "damage" => 0x003333CC,        // red     R=204 G=51  B=51
        "warn" => 0x0000AAFF,          // orange  R=255 G=170 B=0
        "spell" => 0x00CC6600,         // blue    R=0   G=102 B=204
        "info" => 0x00888888,          // grey
        "colorbox" => 0x00AA44FF,      // magenta – indicates "pick a colour"
        "heart.png" => 0x003C3CDC,     // red     R=220 G=60  B=60
        "skull.png" => 0x00C8C8C8,     // light grey
        "sword.png" => 0x002864DC,     // orange  R=220 G=100 B=40
        "shield.png" => 0x00C85028,    // blue    R=40  G=80  B=200
        "lightning.png" => 0x001EDCFF, // yellow  R=255 G=220 B=30
        "star.png" => 0x0000C8FF,      // gold    R=255 G=200 B=0
        "info.png" => 0x00C8641E,      // blue    R=30  G=100 B=200
        "alert.png" => 0x000080FF,     // orange  R=255 G=128 B=0
        _ => 0x00666666,
    }
}

/// The app's old built-in stock icon set — no longer generated, but old
/// triggers may still reference one by filename, and the action editor's
/// icon picker (`settings_window.rs`'s `build_icon_items`) filters these
/// out so they aren't offered as new choices.
pub const STOCK_ICON_FILES: &[&str] = &[
    "heart.png",
    "skull.png",
    "sword.png",
    "shield.png",
    "lightning.png",
    "star.png",
    "info.png",
    "alert.png",
];
