//! Extracts and deduplicates spell-gem icons from EverQuest's
//! `Spells00.tga`..`Spells07.tga` texture-atlas sheets (found in
//! `<EQ install dir>/uifiles/default/`) into individual PNGs under the app's
//! `icons/` directory, so they show up in the trigger action icon picker.
//!
//! Grid geometry (40x40px cells, 6x6 per 256x256 sheet, no gutters/offset,
//! with a 16px unused margin along the right and bottom edges) was confirmed
//! against real sample sheets, not guessed.

#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod spell_icons {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    /// Confirmed against real Spells0N.tga sample sheets: 256x256px images,
    /// packed edge-to-edge in a 6x6 grid of 40x40 icons with no gutter (the
    /// remaining 16px strip along the right/bottom is unused and correctly
    /// dropped by the integer-division cell count below).
    pub const DEFAULT_CELL_SIZE: u32 = 40;

    /// `Spells00.tga` through `Spells07.tga` — the conventional set.
    const SHEET_COUNT: u32 = 8;

    #[derive(Default)]
    pub struct ExtractResult {
        /// The exact directory scanned for `Spells0N.tga` files, so a "found
        /// nothing" result is immediately diagnosable instead of a black box.
        pub searched_dir: PathBuf,
        /// Whether `searched_dir` itself exists — false points at a wrong
        /// `eq_dir` derivation; true-but-no-sheets points at a naming or
        /// layout mismatch inside an otherwise-correct directory.
        pub searched_dir_exists: bool,
        pub sheets_found: Vec<String>,
        pub sheets_missing: Vec<String>,
        /// First ~20 entries actually found in `searched_dir` when no
        /// `Spells0N.tga` sheets matched — surfaces a different naming
        /// convention (extension, casing, numbering) immediately instead of
        /// leaving it a mystery.
        pub dir_listing: Vec<String>,
        pub cells_scanned: usize,
        pub blank_skipped: usize,
        pub duplicates_skipped: usize,
        pub extracted: usize,
        pub errors: Vec<String>,
    }

    /// Derives the EverQuest installation directory (parent of `Logs/`) from
    /// a configured log file path like `DIR\Logs\eqlog_Name_Server.txt`.
    pub fn eq_dir_from_log_path(log_path: &str) -> Option<PathBuf> {
        let logs_dir = Path::new(log_path).parent()?;
        logs_dir.parent().map(|p| p.to_path_buf())
    }

    /// Slices every found `Spells0N.tga` sheet under `eq_dir/uifiles/default/`
    /// into `cell_size`-px cells and writes each visually-unique, non-blank
    /// cell as a PNG into `icons_dir` (created if missing). Cells identical
    /// to one already written (exact pixel match, including across sheets)
    /// are skipped, keeping the first occurrence.
    pub fn extract_spell_icons(eq_dir: &Path, icons_dir: &Path, cell_size: u32) -> ExtractResult {
        let mut result = ExtractResult::default();
        let uifiles_dir = eq_dir.join("uifiles").join("default");
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

        let mut seen: HashSet<Vec<u8>> = HashSet::new();

        for sheet in 0..SHEET_COUNT {
            let filename = format!("Spells{sheet:02}.tga");
            let path = uifiles_dir.join(&filename);
            if !path.exists() {
                result.sheets_missing.push(filename);
                continue;
            }

            let img = match image::open(&path) {
                Ok(img) => img.to_rgba8(),
                Err(e) => {
                    result.errors.push(format!("{filename}: {e}"));
                    continue;
                }
            };
            result.sheets_found.push(filename.clone());

            let (w, h) = img.dimensions();
            let cols = w / cell_size;
            let rows = h / cell_size;

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
                    if !seen.insert(cell.as_raw().clone()) {
                        result.duplicates_skipped += 1;
                        continue;
                    }

                    let out_name = format!("spell_s{sheet:02}_r{row:02}_c{col:02}.png");
                    let out_path = icons_dir.join(&out_name);
                    match image::DynamicImage::ImageRgba8(cell).save(&out_path) {
                        Ok(()) => result.extracted += 1,
                        Err(e) => result.errors.push(format!("{out_name}: {e}")),
                    }
                }
            }
        }

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

    /// True for filenames this module itself generates (`spell_sNN_rNN_cNN.png`),
    /// so a re-extraction can safely clear out only its own prior output.
    fn is_generated_icon_name(name: &str) -> bool {
        let Some(rest) = name
            .strip_prefix("spell_s")
            .and_then(|r| r.strip_suffix(".png"))
        else {
            return false;
        };
        let parts: Vec<&str> = rest.splitn(2, "_r").collect();
        let [sheet, row_col] = parts.as_slice() else {
            return false;
        };
        let Some((row, col)) = row_col.split_once("_c") else {
            return false;
        };
        sheet.len() == 2
            && sheet.chars().all(|c| c.is_ascii_digit())
            && row.len() == 2
            && row.chars().all(|c| c.is_ascii_digit())
            && col.len() == 2
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
