//! Asset generation: PNG icons and WAV sound files.
//! Called once at startup to ensure the assets directory is populated.

#[cfg(feature = "tray")]
pub mod assets {
    use std::f32::consts::PI;
    use std::path::PathBuf;

    pub fn icons_dir() -> PathBuf {
        exe_dir().join("icons")
    }

    pub fn sounds_dir() -> PathBuf {
        exe_dir().join("sounds")
    }

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

    /// Sound options exposed in the action-editor combo.
    /// Each entry is (stored_value, display_label).
    /// stored_value is "" for none, or a relative path like "sounds/ding.wav".
    pub const SOUND_OPTIONS: &[(&str, &str)] = &[
        ("", "(none)"),
        ("sounds/ding.wav", "Ding"),
        ("sounds/alert.wav", "Alert"),
        ("sounds/chime.wav", "Chime"),
        ("sounds/notify.wav", "Notify"),
        ("sounds/warning.wav", "Warning"),
    ];

    pub fn sound_option_index(sound: &Option<String>) -> usize {
        let s = sound.as_deref().unwrap_or("");
        SOUND_OPTIONS.iter().position(|(k, _)| *k == s).unwrap_or(0)
    }

    pub fn sound_value_for_index(idx: usize) -> Option<String> {
        SOUND_OPTIONS.get(idx).and_then(|(k, _)| {
            if k.is_empty() {
                None
            } else {
                Some(k.to_string())
            }
        })
    }

    /// Build the full sound options list: built-in presets + user .wav/.mp3 files
    /// found in the `sounds/` directory next to the executable.
    pub fn build_sound_options() -> Vec<(String, String)> {
        let mut opts: Vec<(String, String)> = SOUND_OPTIONS
            .iter()
            .map(|(k, l)| (k.to_string(), l.to_string()))
            .collect();

        let dir = sounds_dir();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let built_in: std::collections::HashSet<&str> =
                SOUND_OPTIONS.iter().map(|(k, _)| *k).collect();
            let mut user_files: Vec<(String, String)> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let fname = e.file_name().to_string_lossy().into_owned();
                    let ext = std::path::Path::new(&fname)
                        .extension()
                        .and_then(|x| x.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if ext != "wav" && ext != "mp3" {
                        return None;
                    }
                    let key = format!("sounds/{}", fname);
                    if built_in.contains(key.as_str()) {
                        return None;
                    }
                    let label = std::path::Path::new(&fname)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| fname.clone());
                    Some((key, label))
                })
                .collect();
            user_files.sort_by(|a, b| a.1.cmp(&b.1));
            opts.extend(user_files);
        }
        opts
    }

    /// Generate all missing icon and sound assets next to the executable.
    pub fn ensure_assets() {
        ensure_icons();
        ensure_sounds();
    }

    // ── Icon generation ───────────────────────────────────────────────────────

    fn ensure_icons() {
        let dir = icons_dir();
        let _ = std::fs::create_dir_all(&dir);

        type Maker = fn() -> image::RgbaImage;
        let icons: &[(&str, Maker)] = &[
            ("heart.png", make_heart as Maker),
            ("skull.png", make_skull as Maker),
            ("sword.png", make_sword as Maker),
            ("shield.png", make_shield as Maker),
            ("lightning.png", make_lightning as Maker),
            ("star.png", make_star as Maker),
            ("info.png", make_info_icon as Maker),
            ("alert.png", make_alert as Maker),
        ];

        for (name, maker) in icons {
            let path = dir.join(name);
            if !path.exists() {
                let img = maker();
                let _ = image::DynamicImage::ImageRgba8(img).save(&path);
            }
        }
    }

    const SZ: u32 = 32;
    const DARK_BG: image::Rgba<u8> = image::Rgba([25, 25, 30, 210]);

    fn make_heart() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(SZ, SZ, DARK_BG);
        let s = SZ as f32;
        for y in 0..SZ {
            for x in 0..SZ {
                let fx = (x as f32 / s) * 2.2 - 1.1;
                let fy = (y as f32 / s) * 2.2 - 1.0;
                let v = fx * fx + fy * fy - 1.0;
                if v * v * v - fx * fx * fy * fy * fy <= 0.06 {
                    img.put_pixel(x, y, image::Rgba([225, 55, 75, 255]));
                }
            }
        }
        img
    }

    fn make_skull() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(SZ, SZ, DARK_BG);
        let cx = SZ as f32 / 2.0;
        let cy = SZ as f32 * 0.42;
        let r2 = (SZ as f32 * 0.38_f32).powi(2);
        for y in 0..SZ {
            for x in 0..SZ {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                if dx * dx + dy * dy <= r2 {
                    img.put_pixel(x, y, image::Rgba([210, 210, 210, 255]));
                }
                let jt = SZ as f32 * 0.60;
                let jh = SZ as f32 * 0.26;
                let jp = SZ as f32 * 0.18;
                if (y as f32) >= jt
                    && (y as f32) < (jt + jh)
                    && (x as f32) >= jp
                    && (x as f32) < (SZ as f32 - jp)
                {
                    img.put_pixel(x, y, image::Rgba([210, 210, 210, 255]));
                }
            }
        }
        for y in 0..SZ {
            for x in 0..SZ {
                let re2 = (SZ as f32 * 0.09_f32).powi(2);
                let ey = cy - SZ as f32 * 0.04;
                let lx = cx - SZ as f32 * 0.14;
                let rx = cx + SZ as f32 * 0.14;
                if (x as f32 - lx).powi(2) + (y as f32 - ey).powi(2) <= re2
                    || (x as f32 - rx).powi(2) + (y as f32 - ey).powi(2) <= re2
                {
                    img.put_pixel(x, y, DARK_BG);
                }
            }
        }
        img
    }

    fn make_sword() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(SZ, SZ, DARK_BG);
        let s = SZ as f32;
        let blade = image::Rgba([215, 215, 200, 255]);
        let guard = image::Rgba([190, 155, 45, 255]);
        let grip = image::Rgba([155, 95, 35, 255]);
        let len = (s * 0.72) as u32;
        for i in 0..len {
            let t = i as f32 / len as f32;
            let bx = (s * 0.82 - t * s * 0.62) as i32;
            let by = (s * 0.08 + t * s * 0.74) as i32;
            for dx in -1i32..=1 {
                for dy in -1i32..=1 {
                    let px = (bx + dx).clamp(0, SZ as i32 - 1) as u32;
                    let py = (by + dy).clamp(0, SZ as i32 - 1) as u32;
                    img.put_pixel(px, py, blade);
                }
            }
        }
        // Cross-guard
        for i in 0..10u32 {
            let fx = (s * 0.30 + i as f32 * 1.6) as u32;
            let gy = (s * 0.52) as u32;
            if fx < SZ {
                img.put_pixel(fx, gy, guard);
            }
            if fx < SZ && gy + 1 < SZ {
                img.put_pixel(fx, gy + 1, guard);
            }
        }
        // Grip
        for i in 0..6u32 {
            let hx = (s * 0.20 + i as f32) as u32;
            let hy = (s * 0.57 + i as f32) as u32;
            if hx < SZ && hy < SZ {
                img.put_pixel(hx, hy, grip);
            }
            if hx < SZ && hy + 1 < SZ {
                img.put_pixel(hx, hy + 1, grip);
            }
            if hx + 1 < SZ && hy < SZ {
                img.put_pixel(hx + 1, hy, grip);
            }
        }
        img
    }

    fn make_shield() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(SZ, SZ, DARK_BG);
        let s = SZ as f32;
        let shape: &[(f32, f32)] = &[
            (0.14, 0.07),
            (0.86, 0.07),
            (0.92, 0.48),
            (0.50, 0.94),
            (0.08, 0.48),
        ];
        let shield_col = image::Rgba([45, 85, 215, 255]);
        let cross_col = image::Rgba([200, 220, 255, 255]);
        for y in 0..SZ {
            for x in 0..SZ {
                if point_in_polygon(x as f32 / s, y as f32 / s, shape) {
                    img.put_pixel(x, y, shield_col);
                }
            }
        }
        let ht = 4u32;
        let margin = 7u32;
        let mid = SZ / 2;
        for x in margin..SZ - margin {
            for dy in 0..ht {
                let py = mid.saturating_sub(ht / 2) + dy;
                if x < SZ && py < SZ && point_in_polygon(x as f32 / s, py as f32 / s, shape) {
                    img.put_pixel(x, py, cross_col);
                }
            }
        }
        for y in margin..SZ - margin {
            for dx in 0..ht {
                let px = mid.saturating_sub(ht / 2) + dx;
                if px < SZ && y < SZ && point_in_polygon(px as f32 / s, y as f32 / s, shape) {
                    img.put_pixel(px, y, cross_col);
                }
            }
        }
        img
    }

    fn make_lightning() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(SZ, SZ, DARK_BG);
        let s = SZ as f32;
        let bolt: &[(f32, f32)] = &[
            (0.58, 0.04),
            (0.22, 0.52),
            (0.48, 0.46),
            (0.18, 0.96),
            (0.80, 0.46),
            (0.56, 0.52),
        ];
        for y in 0..SZ {
            for x in 0..SZ {
                if point_in_polygon(x as f32 / s, y as f32 / s, bolt) {
                    img.put_pixel(x, y, image::Rgba([255, 225, 30, 255]));
                }
            }
        }
        img
    }

    fn make_star() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(SZ, SZ, DARK_BG);
        let s = SZ as f32;
        let n = 5usize;
        let outer = 0.44f32;
        let inner = 0.18f32;
        let (cx, cy) = (0.5f32, 0.5f32);
        let mut verts = Vec::new();
        for i in 0..n * 2 {
            let angle = (i as f32 * PI / n as f32) - PI / 2.0;
            let r = if i % 2 == 0 { outer } else { inner };
            verts.push((cx + r * angle.cos(), cy + r * angle.sin()));
        }
        for y in 0..SZ {
            for x in 0..SZ {
                if point_in_polygon(x as f32 / s, y as f32 / s, &verts) {
                    img.put_pixel(x, y, image::Rgba([255, 205, 0, 255]));
                }
            }
        }
        img
    }

    fn make_info_icon() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(SZ, SZ, DARK_BG);
        let s = SZ as f32;
        let cx = s / 2.0;
        let cy = s / 2.0;
        let r2 = (s * 0.43_f32).powi(2);
        for y in 0..SZ {
            for x in 0..SZ {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                if dx * dx + dy * dy <= r2 {
                    img.put_pixel(x, y, image::Rgba([30, 100, 215, 255]));
                }
            }
        }
        let cxi = cx as i32;
        let cyi = cy as i32;
        let white = image::Rgba([255, 255, 255, 255]);
        // Dot
        for dx in -1i32..=1 {
            for dy in -1i32..=1 {
                let px = (cxi + dx) as u32;
                let py = (cyi - 8 + dy) as u32;
                if px < SZ && py < SZ {
                    img.put_pixel(px, py, white);
                }
            }
        }
        // Stem
        for row in 0..9i32 {
            for dx in -1i32..=1 {
                let px = (cxi + dx) as u32;
                let py = (cyi - 3 + row) as u32;
                if px < SZ && py < SZ {
                    img.put_pixel(px, py, white);
                }
            }
        }
        img
    }

    fn make_alert() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(SZ, SZ, DARK_BG);
        let s = SZ as f32;
        let tri: &[(f32, f32)] = &[(0.50, 0.05), (0.95, 0.90), (0.05, 0.90)];
        for y in 0..SZ {
            for x in 0..SZ {
                if point_in_polygon(x as f32 / s, y as f32 / s, tri) {
                    img.put_pixel(x, y, image::Rgba([255, 145, 0, 255]));
                }
            }
        }
        let cx = (SZ / 2) as i32;
        let white = image::Rgba([255, 255, 255, 255]);
        for row in 8i32..20 {
            for dx in -1i32..=1 {
                let px = (cx + dx) as u32;
                if px < SZ && (row as u32) < SZ {
                    img.put_pixel(px, row as u32, white);
                }
            }
        }
        for row in 22i32..25 {
            for dx in -1i32..=1 {
                let px = (cx + dx) as u32;
                if px < SZ && (row as u32) < SZ {
                    img.put_pixel(px, row as u32, white);
                }
            }
        }
        img
    }

    fn point_in_polygon(px: f32, py: f32, verts: &[(f32, f32)]) -> bool {
        let n = verts.len();
        let mut inside = false;
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = verts[i];
            let (xj, yj) = verts[j];
            if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
                inside = !inside;
            }
            j = i;
        }
        inside
    }

    // ── Sound generation ──────────────────────────────────────────────────────

    fn ensure_sounds() {
        let dir = sounds_dir();
        let _ = std::fs::create_dir_all(&dir);

        type SoundMaker = fn() -> Vec<u8>;
        let sounds: &[(&str, SoundMaker)] = &[
            ("ding.wav", make_ding as SoundMaker),
            ("alert.wav", make_alert_snd as SoundMaker),
            ("chime.wav", make_chime as SoundMaker),
            ("notify.wav", make_notify as SoundMaker),
            ("warning.wav", make_warning as SoundMaker),
        ];

        for (name, maker) in sounds {
            let path = dir.join(name);
            if !path.exists() {
                let data = maker();
                let _ = std::fs::write(&path, &data);
            }
        }
    }

    fn make_ding() -> Vec<u8> {
        tone_wav(880.0, 0.30, 0.70)
    }
    fn make_alert_snd() -> Vec<u8> {
        sweep_wav(440.0, 880.0, 0.40, 0.65)
    }
    fn make_notify() -> Vec<u8> {
        tone_wav(523.0, 0.45, 0.60)
    }
    fn make_warning() -> Vec<u8> {
        tone_wav(220.0, 0.55, 0.50)
    }

    fn tone_wav(freq: f32, dur: f32, vol: f32) -> Vec<u8> {
        const SR: u32 = 22050;
        let n = (SR as f32 * dur) as usize;
        let mut s: Vec<i16> = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / SR as f32;
            let env = (1.0 - t / dur).max(0.0).powf(1.2);
            s.push((env * vol * 32767.0 * (2.0 * PI * freq * t).sin()) as i16);
        }
        pcm_wav(&s, SR)
    }

    fn sweep_wav(f0: f32, f1: f32, dur: f32, vol: f32) -> Vec<u8> {
        const SR: u32 = 22050;
        let n = (SR as f32 * dur) as usize;
        let mut s: Vec<i16> = Vec::with_capacity(n);
        let mut phase = 0.0f32;
        for i in 0..n {
            let t = i as f32 / SR as f32;
            phase += 2.0 * PI * (f0 + (f1 - f0) * t / dur) / SR as f32;
            let env = (1.0 - t / dur).max(0.0);
            s.push((env * vol * 32767.0 * phase.sin()) as i16);
        }
        pcm_wav(&s, SR)
    }

    fn make_chime() -> Vec<u8> {
        const SR: u32 = 22050;
        let dur = 0.70f32;
        let n = (SR as f32 * dur) as usize;
        let mut buf = vec![0.0f32; n];
        for (freq, start) in &[(523.0f32, 0.0f32), (659.0, 0.18), (784.0, 0.36)] {
            let si = (*start * SR as f32) as usize;
            let nd = 0.40f32;
            for i in si..n {
                let t = (i - si) as f32 / SR as f32;
                if t > nd {
                    break;
                }
                let env = (1.0 - t / nd).max(0.0).powf(1.5);
                buf[i] += env * 0.36 * (2.0 * PI * freq * t).sin();
            }
        }
        let s: Vec<i16> = buf
            .iter()
            .map(|&v| (v.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
        pcm_wav(&s, SR)
    }

    fn pcm_wav(samples: &[i16], sr: u32) -> Vec<u8> {
        let data = samples.len() * 2;
        let mut out = Vec::with_capacity(44 + data);
        let u16b = |v: u16, o: &mut Vec<u8>| o.extend_from_slice(&v.to_le_bytes());
        let u32b = |v: u32, o: &mut Vec<u8>| o.extend_from_slice(&v.to_le_bytes());
        out.extend_from_slice(b"RIFF");
        u32b((36 + data) as u32, &mut out);
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        u32b(16, &mut out);
        u16b(1, &mut out); // PCM
        u16b(1, &mut out); // mono
        u32b(sr, &mut out);
        u32b(sr * 2, &mut out); // byte rate
        u16b(2, &mut out); // block align
        u16b(16, &mut out); // bits/sample
        out.extend_from_slice(b"data");
        u32b(data as u32, &mut out);
        for &s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }
}
