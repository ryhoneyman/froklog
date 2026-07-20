//! Sound "packages": named subdirectories under `assets::sounds_dir()`, each
//! holding a set of sound files plus a `package.toml` manifest mapping a
//! stable label name (e.g. "Ding") to a filename within that package's
//! folder. Triggers reference sounds by label, not by file, so switching the
//! globally "active" package (`Config.sound_package`) re-themes every
//! trigger's sounds at once without editing any of them.

#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod sound_packages {
    use std::fs;
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Serialize};

    pub const DEFAULT_PACKAGE: &str = "default";

    /// Built-in labels seeded into the `default` package by
    /// `assets::ensure_sounds()`, and recognized here so legacy trigger
    /// values (raw `"sounds/ding.wav"`-style keys from before packages
    /// existed) migrate to a label with no file copy needed.
    pub const BUILTIN_LABELS: &[(&str, &str, &str)] = &[
        // (label, filename, legacy key)
        ("Ding", "ding.wav", "sounds/ding.wav"),
        ("Alert", "alert.wav", "sounds/alert.wav"),
        ("Chime", "chime.wav", "sounds/chime.wav"),
        ("Notify", "notify.wav", "sounds/notify.wav"),
        ("Warning", "warning.wav", "sounds/warning.wav"),
    ];

    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    pub struct PackageManifest {
        #[serde(default, rename = "label")]
        pub labels: Vec<LabelEntry>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct LabelEntry {
        pub name: String,
        pub file: String,
    }

    pub fn package_dir(name: &str) -> PathBuf {
        crate::assets::sounds_dir().join(name)
    }

    pub fn manifest_path(pkg: &str) -> PathBuf {
        package_dir(pkg).join("package.toml")
    }

    /// All package names that have a `package.toml`, `"default"` first,
    /// remaining ones sorted alphabetically.
    pub fn list_packages() -> Vec<String> {
        let dir = crate::assets::sounds_dir();
        let mut names: Vec<String> = fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .filter(|name| manifest_path(name).is_file())
            .collect();
        names.retain(|n| n != DEFAULT_PACKAGE);
        names.sort();
        if manifest_path(DEFAULT_PACKAGE).is_file() || package_dir(DEFAULT_PACKAGE).is_dir() {
            names.insert(0, DEFAULT_PACKAGE.to_string());
        }
        names
    }

    pub fn load_manifest(pkg: &str) -> PackageManifest {
        let Ok(text) = fs::read_to_string(manifest_path(pkg)) else {
            return PackageManifest::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    pub fn save_manifest(pkg: &str, m: &PackageManifest) {
        let dir = package_dir(pkg);
        let _ = fs::create_dir_all(&dir);
        if let Ok(text) = toml::to_string_pretty(m) {
            let _ = fs::write(manifest_path(pkg), text);
        }
    }

    /// `("", "(none)")` plus the sorted union of every label name defined in
    /// any package, for populating the PlaySound action's dropdown.
    pub fn all_label_options() -> Vec<(String, String)> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for pkg in list_packages() {
            for entry in load_manifest(&pkg).labels {
                names.insert(entry.name);
            }
        }
        let mut opts = vec![(String::new(), "(none)".to_string())];
        opts.extend(names.into_iter().map(|n| (n.clone(), n)));
        opts
    }

    /// Resolves a label to a playable file path: the active package's
    /// mapping first, falling back to the `default` package's mapping, else
    /// `None` (caller should no-op, never treat this as an error).
    pub fn resolve_label(active_pkg: &str, label: &str) -> Option<PathBuf> {
        if label.is_empty() {
            return None;
        }
        if let Some(entry) = load_manifest(active_pkg)
            .labels
            .into_iter()
            .find(|e| e.name == label)
        {
            return Some(package_dir(active_pkg).join(entry.file));
        }
        if active_pkg != DEFAULT_PACKAGE {
            if let Some(entry) = load_manifest(DEFAULT_PACKAGE)
                .labels
                .into_iter()
                .find(|e| e.name == label)
            {
                return Some(package_dir(DEFAULT_PACKAGE).join(entry.file));
            }
        }
        None
    }

    /// Display label for a picked/legacy sound file: the file stem, falling
    /// back to the full string if it has none.
    pub fn label_from_stem(path_or_name: &str) -> String {
        Path::new(path_or_name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_or_name.to_string())
    }

    /// Filesystem-safe filename built from a label name plus a source
    /// file's extension (defaults to `wav` if the source has none).
    fn label_filename(name: &str, source: &Path) -> String {
        let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("wav");
        let sanitized: String = name
            .chars()
            .map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
            .collect();
        let sanitized = sanitized.trim();
        let sanitized = if sanitized.is_empty() {
            "sound"
        } else {
            sanitized
        };
        format!("{sanitized}.{ext}")
    }

    /// Copies `source` into `pkg`'s folder under a filename derived from
    /// `name`, then upserts `name -> filename` in that package's manifest.
    /// If `name` already existed pointing at a *different* filename (e.g.
    /// the source's extension changed on edit), the stale file is removed —
    /// safe because this is the one label being replaced, not a shared file.
    pub fn add_or_replace_label(pkg: &str, name: &str, source: &Path) -> io::Result<()> {
        let dir = package_dir(pkg);
        fs::create_dir_all(&dir)?;
        let filename = label_filename(name, source);
        let dest = dir.join(&filename);
        if dest != source {
            fs::copy(source, &dest)?;
        }

        let mut manifest = load_manifest(pkg);
        let stale_file = manifest
            .labels
            .iter()
            .find(|e| e.name == name)
            .filter(|e| e.file != filename)
            .map(|e| e.file.clone());
        manifest.labels.retain(|e| e.name != name);
        manifest.labels.push(LabelEntry {
            name: name.to_string(),
            file: filename,
        });
        save_manifest(pkg, &manifest);

        if let Some(stale) = stale_file {
            let still_used = manifest.labels.iter().any(|e| e.file == stale);
            if !still_used {
                let _ = fs::remove_file(dir.join(stale));
            }
        }
        Ok(())
    }

    /// Renames a label in place (the underlying file is untouched).
    pub fn rename_label(pkg: &str, old: &str, new: &str) -> io::Result<()> {
        let mut manifest = load_manifest(pkg);
        for entry in &mut manifest.labels {
            if entry.name == old {
                entry.name = new.to_string();
            }
        }
        save_manifest(pkg, &manifest);
        Ok(())
    }

    /// Removes a label from the manifest only. The file is left in place —
    /// another label (in this package or another) may still reference it.
    pub fn delete_label(pkg: &str, name: &str) {
        let mut manifest = load_manifest(pkg);
        manifest.labels.retain(|e| e.name != name);
        save_manifest(pkg, &manifest);
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let dest_path = dst.join(entry.file_name());
            if entry.path().is_dir() {
                copy_dir_all(&entry.path(), &dest_path)?;
            } else {
                fs::copy(entry.path(), dest_path)?;
            }
        }
        Ok(())
    }

    pub fn clone_package(src: &str, dst_name: &str) -> io::Result<()> {
        copy_dir_all(&package_dir(src), &package_dir(dst_name))
    }

    pub fn rename_package(old: &str, new: &str) -> io::Result<()> {
        if old == DEFAULT_PACKAGE {
            return Err(io::Error::other("the default package cannot be renamed"));
        }
        fs::rename(package_dir(old), package_dir(new))
    }

    pub fn delete_package(name: &str) -> io::Result<()> {
        if name == DEFAULT_PACKAGE {
            return Err(io::Error::other("the default package cannot be deleted"));
        }
        fs::remove_dir_all(package_dir(name))
    }

    /// `base`, or `"{base} (2)"`, `"{base} (3)"`, ... — first name with no
    /// existing package folder.
    pub fn unique_package_name(base: &str) -> String {
        if !package_dir(base).exists() {
            return base.to_string();
        }
        let mut n = 2u32;
        loop {
            let candidate = format!("{base} ({n})");
            if !package_dir(&candidate).exists() {
                return candidate;
            }
            n += 1;
        }
    }

    // ── Export / import ──────────────────────────────────────────────────

    pub fn export_package_zip(pkg: &str, dest_zip: &Path) -> Result<(), String> {
        let dir = package_dir(pkg);
        let entries = fs::read_dir(&dir).map_err(|e| e.to_string())?;
        let file = fs::File::create(dest_zip).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let data = fs::read(&path).map_err(|e| e.to_string())?;
            zip.start_file(name, options).map_err(|e| e.to_string())?;
            zip.write_all(&data).map_err(|e| e.to_string())?;
        }
        zip.finish().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Extracts `zip_path` into a new package folder (name derived from the
    /// zip's own filename, de-duplicated against existing packages) and
    /// returns that package's name.
    pub fn import_package_zip(zip_path: &Path) -> Result<String, String> {
        let file = fs::File::open(zip_path).map_err(|e| e.to_string())?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

        let base = zip_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("imported");
        let name = unique_package_name(base);
        let dir = package_dir(&name);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
            if entry.is_dir() {
                continue;
            }
            let Some(entry_name) = Path::new(entry.name())
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            fs::write(dir.join(entry_name), &buf).map_err(|e| e.to_string())?;
        }
        Ok(name)
    }

    // ── Legacy migration ─────────────────────────────────────────────────

    /// Converts a raw path-shaped trigger sound value (a pre-packages
    /// `Action::PlaySound.sound`) into a label, registering it in the
    /// `default` package if needed. Returns `None` (leave the value
    /// untouched) if the referenced file can't be found — best-effort, not
    /// fatal, since it just means that sound won't resolve at play-time.
    pub fn migrate_legacy_sound_value(raw: &str) -> Option<String> {
        let normalized = raw.replace('\\', "/");
        for (label, _, legacy_key) in BUILTIN_LABELS {
            if normalized == *legacy_key {
                // Already seeded into `default` by `ensure_sounds()` — no file copy needed.
                return Some((*label).to_string());
            }
        }

        let source = if let Some(rest) = normalized.strip_prefix("sounds/") {
            crate::assets::sounds_dir().join(rest)
        } else {
            PathBuf::from(raw)
        };
        if !source.is_file() {
            return None;
        }
        let label = label_from_stem(&normalized);
        add_or_replace_label(DEFAULT_PACKAGE, &label, &source).ok()?;
        Some(label)
    }
}
