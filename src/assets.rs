//! Paths to the icons/sounds directories, plus the legacy stock-icon
//! filename list.

use std::path::PathBuf;

pub fn icons_dir() -> PathBuf {
    data_dir().join("icons")
}

pub fn sounds_dir() -> PathBuf {
    data_dir().join("sounds")
}

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
        exe_dir()
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

#[cfg(target_os = "windows")]
fn exe_dir() -> PathBuf {
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
/// triggers may still reference one by filename, and the config UI's icon
/// picker (`overlay_config_win.rs`) filters these out of `build_icon_items`
/// so they aren't offered as new choices.
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
