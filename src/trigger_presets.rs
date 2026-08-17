/// Built-in regex patterns offered by the Condition editor's "Presets"
/// picker (see ui/common/preset-picker.slint), for the common EQ log lines
/// people write triggers against. Plain data only — no dependency on the
/// Slint-generated `PatternPreset` struct, which lives in settings_window's
/// generated module and gets mapped from this in settings_window.rs (same
/// split `build_icon_items()`/`IconOption` already uses for the icon
/// picker).
///
/// These are deliberately *not* the same regex strings as `patterns.rs`:
/// those are `^`/`$`-anchored against the timestamp-stripped line the
/// parser sees, while trigger conditions match unanchored against the raw
/// tailed line (timestamp prefix and all) — see `triggers::engine::eval_condition`.
pub struct PatternPresetDef {
    pub category: &'static str,
    pub label: &'static str,
    pub pattern: &'static str,
}

pub const PATTERN_PRESETS: &[PatternPresetDef] = &[
    PatternPresetDef {
        category: "Death",
        label: "You slay a mob",
        pattern: r"You have slain (?P<tgt>.+)!",
    },
    PatternPresetDef {
        category: "Death",
        label: "Someone else slays a mob",
        pattern: r"(?P<killer>.+?) has slain (?P<tgt>.+)!",
    },
    PatternPresetDef {
        category: "Death",
        label: "A mob is slain (by anyone)",
        pattern: r"(?P<tgt>.+?) (?:was|has been) slain by (?P<killer>.+)!",
    },
    PatternPresetDef {
        category: "Death",
        label: "You are slain",
        pattern: r"You have been slain by (?P<killer>.+)!",
    },
    PatternPresetDef {
        category: "Death",
        label: "A mob simply died",
        pattern: r"(?P<tgt>.+?) died\.",
    },
    PatternPresetDef {
        category: "Combat",
        label: "You melee a target",
        pattern: r"You (?:hit|slash|crush|pierce|bash|kick|punch|backstab)s? (?P<tgt>.+?) for (?P<dmg>\d+) point",
    },
    PatternPresetDef {
        category: "Combat",
        label: "Something melees you",
        pattern: r"(?P<src>.+?) (?:hits?|slashes?|crushes?|pierces?|bites?|claws?) YOU for (?P<dmg>\d+) point",
    },
    PatternPresetDef {
        category: "Combat",
        label: "Spell damage lands",
        pattern: r"(?P<src>.+?)'s (?P<spell>.+?) (?:hit|has taken effect on) (?P<tgt>.+?)(?: for (?P<dmg>\d+) point)?",
    },
    PatternPresetDef {
        category: "Combat",
        label: "You begin casting",
        pattern: r"You begin casting (?P<spell>.+)\.",
    },
    PatternPresetDef {
        category: "Combat",
        label: "Someone heals a target",
        pattern: r"(?P<src>.+?) healed? (?P<tgt>.+?) for (?P<amt>\d+) hit points?",
    },
    PatternPresetDef {
        category: "Chat",
        label: "Someone tells you",
        pattern: r"(?P<src>.+?) tells? you, '(?P<msg>.+)'",
    },
    PatternPresetDef {
        category: "Chat",
        label: "Someone says something",
        pattern: r"(?P<src>.+?) says?, '(?P<msg>.+)'",
    },
    PatternPresetDef {
        category: "Chat",
        label: "Someone tells the group",
        pattern: r"(?P<src>.+?) tells? the group, '(?P<msg>.+)'",
    },
    PatternPresetDef {
        category: "Loot",
        label: "You loot an item",
        pattern: r"You (?:have )?looted (?P<item>.+?) from (?P<mob>.+?)'s corpse",
    },
    PatternPresetDef {
        category: "Loot",
        label: "You receive currency from a corpse",
        pattern: r"You receive (?P<amounts>.+?) from the corpse\.",
    },
    PatternPresetDef {
        category: "System",
        label: "You enter a zone",
        pattern: r"You have entered (?P<zone>.+)\.",
    },
];

/// A preset as offered to the UI — same three fields as `PatternPresetDef`
/// but owned, since it may come from a downloaded JSON file rather than a
/// `&'static str` compiled into the binary.
#[derive(Clone)]
pub struct EffectivePreset {
    pub category: String,
    pub label: String,
    pub pattern: String,
}

/// Raw shape of `dynamic-config.json`, maintained in the froklog repo and
/// fetched fresh by `download_dynamic_config()` — see that fn's doc comment
/// for why presets live there instead of only in `PATTERN_PRESETS`.
#[derive(serde::Deserialize)]
struct DynamicConfigFile {
    #[serde(default)]
    presets: Vec<DynamicPreset>,
}

#[derive(serde::Deserialize)]
struct DynamicPreset {
    category: String,
    label: String,
    pattern: String,
}

/// Where the maintained preset list lives — plain data in the froklog repo
/// itself, read straight off `main` so a merge there is live for every
/// client immediately, with no new release. See dynamic-config.json's own
/// header comment and ci.yml's paths-ignore entry for the same filename.
pub const DYNAMIC_CONFIG_URL: &str =
    "https://raw.githubusercontent.com/ryhoneyman/froklog/main/dynamic-config.json";

/// Presets to actually show in the Condition editor's picker. If the user
/// has ever successfully used General's "Download Latest Configuration"
/// button, that cached copy is used *instead of* `PATTERN_PRESETS` — this
/// is a full replace, not a merge, so the repo-hosted file can also edit or
/// drop presets that shipped in the binary, not just add to them. Falls
/// back to `PATTERN_PRESETS` when nothing has been downloaded yet, or the
/// cached file is missing/corrupt.
pub fn effective_presets() -> Vec<EffectivePreset> {
    let cached = std::fs::read(crate::assets::dynamic_config_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<DynamicConfigFile>(&bytes).ok())
        .filter(|cfg| !cfg.presets.is_empty());

    match cached {
        Some(cfg) => cfg
            .presets
            .into_iter()
            .map(|p| EffectivePreset {
                category: p.category,
                label: p.label,
                pattern: p.pattern,
            })
            .collect(),
        None => PATTERN_PRESETS
            .iter()
            .map(|p| EffectivePreset {
                category: p.category.to_string(),
                label: p.label.to_string(),
                pattern: p.pattern.to_string(),
            })
            .collect(),
    }
}

/// Downloads `dynamic-config.json` from the froklog repo and caches it to
/// `assets::dynamic_config_path()`, returning the number of presets it
/// contained. Parses the response before writing anything, so a bad or
/// truncated download can never clobber a previously-working cache.
pub fn download_dynamic_config() -> Result<usize, String> {
    let bytes = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?
        .get(DYNAMIC_CONFIG_URL)
        .send()
        .and_then(|resp| resp.error_for_status())
        .map_err(|e| e.to_string())?
        .bytes()
        .map_err(|e| e.to_string())?;

    let parsed: DynamicConfigFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid JSON: {e}"))?;

    let path = crate::assets::dynamic_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;

    Ok(parsed.presets.len())
}
