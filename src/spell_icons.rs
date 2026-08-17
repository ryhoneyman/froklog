//! Extracts and deduplicates spell-gem icons from EverQuest's
//! `SpellsNN.tga` texture-atlas sheets. UI skins (including custom/modded
//! ones) each keep their own copy of these sheets under
//! `<EQ install dir>/uifiles/<skin>/`, so every subdirectory of `uifiles`
//! that contains at least one matching sheet is scanned, and the resulting
//! icons — deduplicated by near-identical pixel content *within* each skin
//! directory, but never across directories (see `NEAR_DUP_MAX_BLOB_SIZE`) —
//! are written as individual PNGs under the app's `icons/` directory, so
//! they show up in the trigger action icon picker.
//!
//! Dedup is scoped per skin directory, not global, because the spell-icon
//! numbering (see `ICONS_PER_SHEET`) is a single global convention shared by
//! every skin — the same icon id, and therefore the same spell name, can
//! legitimately appear in `default`, `dui`, and a dozen custom skins alike.
//! Deduping globally would let whichever skin happens to be scanned first
//! claim that spell's icon and silently drop it from every other skin's
//! picker entry; every skin needs its own copy of every spell it has art
//! for, even when that art is pixel-identical to another skin's.
//!
//! Dedup can't just average the pixel difference between two cells (plain
//! mean-absolute-difference): real `SpellsNN.tga` sheets contain pairs that
//! are deliberately different (e.g. a yellow- vs. green-tinted reagent icon
//! for two different spells) but whose difference is confined to a small
//! patch, so it averages away to *less* than some pairs that are genuinely
//! the same icon with harmless whole-image recompression noise. What does
//! separate the two cleanly: the size of the largest *contiguous* patch of
//! meaningfully-different pixels. Across every confirmed pair from all 5
//! stock UI skins, genuinely-identical icons never produced a contiguous
//! diff patch bigger than 2px, while every deliberately-different icon
//! produced one of at least 19px — see `largest_diff_blob` and
//! `NEAR_DUP_MAX_BLOB_SIZE`. Don't go back to a plain distance/average
//! threshold here; it was tried and silently merges real icons away.
//!
//! Grid geometry (40x40px cells, 6x6 per 256x256 sheet, no gutters/offset,
//! with a 16px unused margin along the right and bottom edges) was confirmed
//! against real sample sheets, not guessed.

#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod spell_icons {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// Confirmed against real Spells0N.tga sample sheets: 256x256px images,
    /// packed edge-to-edge in a 6x6 grid of 40x40 icons with no gutter (the
    /// remaining 16px strip along the right/bottom is unused and correctly
    /// dropped by the integer-division cell count below).
    pub const DEFAULT_CELL_SIZE: u32 = 40;

    /// EQ's global spell-icon numbering (`spells_us.txt` field 76) is
    /// 0-indexed: ids 0..=35 fall on `SpellsNN.tga`'s first sheet, 36..=71
    /// on the second, and so on — confirmed against real sample sheets
    /// (icon id 0 and id 36 are both row 0 / col 0 of their respective
    /// sheets — e.g. id 0 is the raised-hand icon shared by "Stalwart
    /// Regeneration" and hundreds of other real spells, not a "no icon"
    /// sentinel — and the in-between ids fill each 6x6 sheet row-major).
    /// This is the game's fixed data convention, not a property of any one
    /// skin's actual texture dimensions, so it's a constant rather than
    /// derived from a given sheet's measured width/height.
    const ICONS_PER_SHEET: u32 = 6 * 6;

    /// Filename this app looks for in the EQ install root (sibling of
    /// `uifiles/` and `Logs/`) to name extracted icons after a real spell.
    const SPELLS_FILE_NAME: &str = "spells_us.txt";

    /// Per-channel (R/G/B, alpha ignored) absolute difference above which a
    /// pixel counts as "changed" when comparing two icon cells for
    /// near-duplicate detection. Feeds `largest_diff_blob`, not a
    /// standalone dedup signal by itself.
    const NEAR_DUP_PIXEL_DIFF_THRESHOLD: i32 = 25;

    /// Two icon cells are treated as the same icon when the largest
    /// 4-connected blob of "changed" pixels between them (see
    /// `largest_diff_blob`) is at most this many pixels. Validated against
    /// every candidate pair (same perceptual hash) across all 5 stock EQ UI
    /// skins: genuinely-identical pairs (re-export/rounding noise) topped
    /// out at a 2px blob, while every deliberately-different pair (e.g. a
    /// differently-colored reagent variant) started at 19px. 5 sits in that
    /// gap with margin on both sides — re-validate against real sheets
    /// before raising it.
    const NEAR_DUP_MAX_BLOB_SIZE: usize = 5;

    /// 8x8 average-threshold perceptual hash (aHash) of an icon cell, used
    /// to cheaply bucket visually-similar cells before the costlier
    /// `largest_diff_blob` comparison below — comparing every new cell
    /// against every previously-kept cell (thousands, across a full
    /// `uifiles` scan) would be quadratic otherwise.
    fn perceptual_hash(cell: &image::RgbaImage) -> u64 {
        let gray = image::imageops::grayscale(cell);
        let gray = image::imageops::resize(&gray, 8, 8, image::imageops::FilterType::Lanczos3);
        let avg: u32 = gray.pixels().map(|p| p.0[0] as u32).sum::<u32>() / 64;
        let mut hash: u64 = 0;
        for (i, p) in gray.pixels().enumerate() {
            if p.0[0] as u32 > avg {
                hash |= 1 << i;
            }
        }
        hash
    }

    /// XOR deltas for every perceptual hash 1 or 2 bits away from a given
    /// hash — checked alongside the exact hash bucket when looking for
    /// near-duplicate candidates. `perceptual_hash`'s per-image average
    /// threshold means two genuinely near-identical cells can still land a
    /// bit or two apart (a downsampled pixel sitting right at that
    /// particular image's own brightness average, tipped either way by the
    /// same few units of noise being deduped against) — confirmed directly
    /// against real sheets, where a pair with *zero* pixels differing by
    /// more than the dup threshold still hashed 1 bit apart. Widening the
    /// candidate search can only ever improve recall, never cause a false
    /// merge, since `largest_diff_blob` still gates the actual decision.
    fn hash_neighbor_masks() -> Vec<u64> {
        let mut masks = Vec::with_capacity(64 + 64 * 63 / 2);
        for i in 0..64u32 {
            masks.push(1u64 << i);
            for j in (i + 1)..64u32 {
                masks.push((1u64 << i) | (1u64 << j));
            }
        }
        masks
    }

    /// True if `cell_raw` (a raw RGBA buffer, `cell_size`x`cell_size`)
    /// matches any previously-kept icon closely enough — per
    /// `largest_diff_blob`/`NEAR_DUP_MAX_BLOB_SIZE` — among the candidates
    /// found in `hash_buckets` under `phash` or any of `neighbor_masks`
    /// away from it.
    #[allow(clippy::too_many_arguments)]
    fn is_near_duplicate(
        cell_raw: &[u8],
        phash: u64,
        cell_size: u32,
        hash_buckets: &HashMap<u64, Vec<usize>>,
        kept_pixels: &[Vec<u8>],
        neighbor_masks: &[u64],
    ) -> bool {
        let matches_bucket = |h: u64| {
            hash_buckets.get(&h).is_some_and(|idxs| {
                idxs.iter().any(|&idx| {
                    largest_diff_blob(cell_raw, &kept_pixels[idx], cell_size, cell_size)
                        <= NEAR_DUP_MAX_BLOB_SIZE
                })
            })
        };
        matches_bucket(phash)
            || neighbor_masks
                .iter()
                .any(|&mask| matches_bucket(phash ^ mask))
    }

    /// Size (in pixels) of the largest 4-connected group of pixels where
    /// `a` and `b` — equal-length raw RGBA buffers of `width`x`height` —
    /// differ by at least `NEAR_DUP_PIXEL_DIFF_THRESHOLD` in R, G, or B.
    /// This is the signal that actually separates real duplicates from
    /// deliberately different icons; see the module doc comment for why
    /// a plain averaged difference doesn't.
    fn largest_diff_blob(a: &[u8], b: &[u8], width: u32, height: u32) -> usize {
        let width = width as usize;
        let height = height as usize;
        let mut changed = vec![false; width * height];
        for (i, is_changed) in changed.iter_mut().enumerate() {
            let px = i * 4;
            let d = (0..3)
                .map(|c| (a[px + c] as i32 - b[px + c] as i32).abs())
                .max()
                .unwrap_or(0);
            *is_changed = d >= NEAR_DUP_PIXEL_DIFF_THRESHOLD;
        }
        let mut visited = vec![false; width * height];
        let mut best = 0usize;
        let mut stack: Vec<usize> = Vec::new();
        for start in 0..width * height {
            if !changed[start] || visited[start] {
                continue;
            }
            visited[start] = true;
            stack.push(start);
            let mut size = 0usize;
            while let Some(idx) = stack.pop() {
                size += 1;
                let x = idx % width;
                let y = idx / width;
                let neighbors = [
                    (x > 0).then(|| idx - 1),
                    (x + 1 < width).then(|| idx + 1),
                    (y > 0).then(|| idx - width),
                    (y + 1 < height).then(|| idx + width),
                ];
                for n in neighbors.into_iter().flatten() {
                    if changed[n] && !visited[n] {
                        visited[n] = true;
                        stack.push(n);
                    }
                }
            }
            best = best.max(size);
        }
        best
    }

    #[derive(Default)]
    pub struct ExtractResult {
        /// The `uifiles` directory itself, so a "found nothing" result is
        /// immediately diagnosable instead of a black box.
        pub searched_dir: PathBuf,
        /// Whether `searched_dir` itself exists — false points at a wrong
        /// `eq_dir` derivation; true-but-no-sheets points at a naming or
        /// layout mismatch inside its subdirectories.
        pub searched_dir_exists: bool,
        /// Subdirectories of `uifiles` that contained at least one matching
        /// sheet and were scanned (e.g. `["default", "SOF"]`).
        pub dirs_scanned: Vec<String>,
        /// `<dir>/<file>` for every sheet actually opened.
        pub sheets_found: Vec<String>,
        /// Immediate subdirectories of `uifiles` when none of them contained
        /// a matching sheet — surfaces a different naming convention
        /// immediately instead of leaving it a mystery.
        pub dir_listing: Vec<String>,
        pub cells_scanned: usize,
        pub blank_skipped: usize,
        pub duplicates_skipped: usize,
        pub extracted: usize,
        /// Whether `<eq_dir>/spells_us.txt` was found and read — false
        /// means icons were named from their filename position instead of
        /// a real spell name.
        pub spells_file_found: bool,
        /// How many extracted icons were matched to a spell name via
        /// `spells_us.txt`.
        pub named: usize,
        pub errors: Vec<String>,
    }

    /// Derives the EverQuest installation directory (parent of `Logs/`) from
    /// a configured log file path like `DIR\Logs\eqlog_Name_Server.txt`.
    pub fn eq_dir_from_log_path(log_path: &str) -> Option<PathBuf> {
        let logs_dir = Path::new(log_path).parent()?;
        logs_dir.parent().map(|p| p.to_path_buf())
    }

    /// True for filenames matching EverQuest's spell-sheet convention:
    /// `Spells` followed by digits, with a `.tga` extension — both matched
    /// case-insensitively since UI mod authors are inconsistent about
    /// casing.
    fn is_spell_sheet_filename(name: &str) -> bool {
        sheet_number_from_filename(name).is_some()
    }

    /// Parses the `NN` sheet number out of a `SpellsNN.tga`-style filename
    /// (case-insensitive), or `None` if it doesn't match that convention.
    fn sheet_number_from_filename(name: &str) -> Option<u32> {
        let lower = name.to_ascii_lowercase();
        let stem = lower.strip_suffix(".tga")?;
        let digits = stem.strip_prefix("spells")?;
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }

    /// Reads `<eq_dir>/spells_us.txt` (EQ's caret-delimited client spell
    /// database) and returns a map from global icon id (field 76,
    /// 0-indexed — see `ICONS_PER_SHEET`) to every distinct spell name that
    /// points at it, in file order (ascending spell id). Many spells share
    /// one icon, and a search for *any* of them should find it, so every
    /// name is kept rather than just the first encountered. Icon id 0 is a
    /// real, commonly-used slot (not a "no icon" sentinel), so it's kept
    /// rather than filtered out.
    fn load_spell_icon_names(eq_dir: &Path) -> HashMap<u32, Vec<String>> {
        let mut names: HashMap<u32, Vec<String>> = HashMap::new();
        let Ok(text) = std::fs::read_to_string(eq_dir.join(SPELLS_FILE_NAME)) else {
            return names;
        };
        for line in text.lines() {
            let fields: Vec<&str> = line.split('^').collect();
            if fields.len() < 76 {
                continue;
            }
            let name = fields[1];
            if name.is_empty() {
                continue;
            }
            let Ok(icon_id) = fields[75].parse::<u32>() else {
                continue;
            };
            let entry = names.entry(icon_id).or_default();
            if !entry.iter().any(|n| n == name) {
                entry.push(name.to_string());
            }
        }
        names
    }

    /// Scans every subdirectory of `eq_dir/uifiles/` for `SpellsNN.tga`
    /// sheets, slices every one found into `cell_size`-px cells, and writes
    /// each visually-unique, non-blank cell as a PNG into `icons_dir`
    /// (created if missing). Cells that are the same icon as one already
    /// written *from the same skin directory* — see
    /// `largest_diff_blob`/`NEAR_DUP_MAX_BLOB_SIZE` — are skipped, keeping
    /// the first occurrence within that directory; the same icon reappearing
    /// under a different skin directory is always kept, since each skin gets
    /// its own independent, complete icon set (see the module doc comment).
    pub fn extract_spell_icons(eq_dir: &Path, icons_dir: &Path, cell_size: u32) -> ExtractResult {
        let mut result = ExtractResult::default();
        let uifiles_dir = eq_dir.join("uifiles");
        result.searched_dir = uifiles_dir.clone();
        result.searched_dir_exists = uifiles_dir.is_dir();

        if let Err(e) = std::fs::create_dir_all(icons_dir) {
            result
                .errors
                .push(format!("Could not create {}: {e}", icons_dir.display()));
            return result;
        }

        // Remove PNGs from any previous extraction run before writing new
        // ones. Without this, changing `cell_size` (or re-extracting after
        // the game patches its UI sheets) leaves the old, differently-cut
        // cells behind forever — they don't collide with the new filenames
        // (row/col indices shift with the grid), so they'd otherwise pile up
        // as permanent, wrongly-cropped duplicates in the icon picker.
        // Only files matching our own naming scheme are touched; a user's
        // own custom icons dropped into the same folder are left alone.
        if let Ok(dir) = std::fs::read_dir(icons_dir) {
            for entry in dir.filter_map(|e| e.ok()) {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if is_generated_icon_name(&name) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        // Find every uifiles subdirectory that contains at least one
        // matching sheet, rather than assuming `default` — custom/modded UI
        // skins each ship their own copy under their own subdirectory name.
        let mut skin_dirs: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&uifiles_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let has_sheet = std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .any(|e| is_spell_sheet_filename(&e.file_name().to_string_lossy()));
                if has_sheet {
                    skin_dirs.push(path);
                }
            }
        }
        skin_dirs.sort();

        let spell_names = load_spell_icon_names(eq_dir);
        result.spells_file_found = eq_dir.join(SPELLS_FILE_NAME).is_file();

        let neighbor_masks = hash_neighbor_masks();
        // (filename, exact source directory name, primary spell name for
        // display, every spell name pointing at this icon id for search)
        // for every icon actually written this run — the filename only
        // carries a *sanitized* form of the source name, so this is what
        // lets the picker filter by any real name instead of guessing one
        // back out of a filename.
        let mut manifest: Vec<(String, String, String, Vec<String>)> = Vec::new();

        for skin_dir in &skin_dirs {
            let dir_name = skin_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            result.dirs_scanned.push(dir_name.clone());

            // Reset per skin directory, not shared across the outer loop —
            // dedup only ever collapses a skin's own repeated art (e.g. the
            // same icon appearing on two of its sheets), never a different
            // skin's otherwise-identical icon. See the module doc comment.
            let mut hash_buckets: HashMap<u64, Vec<usize>> = HashMap::new();
            let mut kept_pixels: Vec<Vec<u8>> = Vec::new();

            let mut sheet_files: Vec<String> = std::fs::read_dir(skin_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| is_spell_sheet_filename(n))
                .collect();
            sheet_files.sort();

            for filename in sheet_files {
                let path = skin_dir.join(&filename);
                let img = match image::open(&path) {
                    Ok(img) => img.to_rgba8(),
                    Err(e) => {
                        result.errors.push(format!("{dir_name}/{filename}: {e}"));
                        continue;
                    }
                };
                result.sheets_found.push(format!("{dir_name}/{filename}"));

                let sheet_num = sheet_number_from_filename(&filename);
                let (w, h) = img.dimensions();
                let cols = w / cell_size;
                let rows = h / cell_size;
                let sheet_stem = filename
                    .rsplit_once('.')
                    .map(|(stem, _)| stem)
                    .unwrap_or(&filename);
                let safe_dir = sanitize_for_filename(&dir_name);
                let safe_sheet = sanitize_for_filename(sheet_stem);

                for row in 0..rows {
                    for col in 0..cols {
                        result.cells_scanned += 1;
                        let cell = image::imageops::crop_imm(
                            &img,
                            col * cell_size,
                            row * cell_size,
                            cell_size,
                            cell_size,
                        )
                        .to_image();

                        if is_blank(&cell) {
                            result.blank_skipped += 1;
                            continue;
                        }
                        let phash = perceptual_hash(&cell);
                        let is_duplicate = is_near_duplicate(
                            cell.as_raw(),
                            phash,
                            cell_size,
                            &hash_buckets,
                            &kept_pixels,
                            &neighbor_masks,
                        );
                        if is_duplicate {
                            result.duplicates_skipped += 1;
                            continue;
                        }
                        let kept_idx = kept_pixels.len();
                        kept_pixels.push(cell.as_raw().clone());
                        hash_buckets.entry(phash).or_default().push(kept_idx);

                        let out_name =
                            format!("spell_{safe_dir}_{safe_sheet}_r{row:02}_c{col:02}.png");
                        let out_path = icons_dir.join(&out_name);
                        // Global icon id per the fixed 36-per-sheet, row-major
                        // convention documented on `ICONS_PER_SHEET` — not
                        // derived from this sheet's own `cols`, since the id
                        // space is a game-data constant, independent of any
                        // one skin's actual texture size.
                        let icon_id = sheet_num.map(|n| (n - 1) * ICONS_PER_SHEET + row * 6 + col);
                        let all_names: Vec<String> = icon_id
                            .and_then(|id| spell_names.get(&id))
                            .cloned()
                            .unwrap_or_default();
                        let primary_name = all_names.first().cloned().unwrap_or_default();
                        match image::DynamicImage::ImageRgba8(cell).save(&out_path) {
                            Ok(()) => {
                                result.extracted += 1;
                                if !all_names.is_empty() {
                                    result.named += 1;
                                }
                                manifest.push((
                                    out_name,
                                    dir_name.clone(),
                                    primary_name,
                                    all_names,
                                ));
                            }
                            Err(e) => result.errors.push(format!("{out_name}: {e}")),
                        }
                    }
                }
            }
        }

        // Overwrite (not append) so a re-extraction never leaves stale
        // entries for icons that were just deleted above.
        // `|` is also stripped from names, alongside tab/newline, since it's
        // the delimiter joining the all-names column below.
        let clean = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c == '\t' || c == '\n' || c == '\r' || c == '|' {
                        ' '
                    } else {
                        c
                    }
                })
                .collect()
        };
        let manifest_text: String = manifest
            .iter()
            .map(|(name, source, spell_name, all_names)| {
                let joined = all_names
                    .iter()
                    .map(|n| clean(n))
                    .collect::<Vec<_>>()
                    .join("|");
                format!(
                    "{name}\t{}\t{}\t{joined}\n",
                    clean(source),
                    clean(spell_name)
                )
            })
            .collect();
        let _ = std::fs::write(
            icons_dir.join(crate::assets::SPELL_ICON_MANIFEST_FILE),
            manifest_text,
        );

        if result.sheets_found.is_empty() {
            if let Ok(entries) = std::fs::read_dir(&uifiles_dir) {
                result.dir_listing = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .take(20)
                    .collect();
            }
        }

        result
    }

    /// Replaces anything that isn't a filename-safe ASCII alphanumeric,
    /// `-`, or `_` with `_`, so arbitrary UI-skin directory/sheet names
    /// can't produce path separators or otherwise break the output name.
    fn sanitize_for_filename(s: &str) -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// True for filenames this module itself generates
    /// (`spell_<dir>_<sheet>_rNN_cNN.png`), so a re-extraction can safely
    /// clear out only its own prior output.
    fn is_generated_icon_name(name: &str) -> bool {
        let Some(rest) = name
            .strip_prefix("spell_")
            .and_then(|r| r.strip_suffix(".png"))
        else {
            return false;
        };
        let Some((_prefix, row_col)) = rest.rsplit_once("_r") else {
            return false;
        };
        let Some((row, col)) = row_col.split_once("_c") else {
            return false;
        };
        !row.is_empty()
            && row.chars().all(|c| c.is_ascii_digit())
            && !col.is_empty()
            && col.chars().all(|c| c.is_ascii_digit())
    }

    /// True if every pixel is fully transparent, or every non-transparent
    /// pixel is the exact same color — both mean an unused grid slot rather
    /// than real icon art.
    fn is_blank(img: &image::RgbaImage) -> bool {
        let mut first: Option<[u8; 4]> = None;
        for px in img.pixels() {
            if px.0[3] == 0 {
                continue;
            }
            match first {
                None => first = Some(px.0),
                Some(f) if f == px.0 => {}
                Some(_) => return false,
            }
        }
        true
    }
}
