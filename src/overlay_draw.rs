/// Shared Win32 DIB pixel-blit helpers used by both overlay windows
/// (`overlay.rs`'s alert deck and `overlay_dps.rs`'s meter table).
///
/// Premultiplied-BGRA compositing primitives plus GDI-text-to-DIB coverage
/// extraction. Kept in one place so the two windows can't drift on subtle
/// alpha-blend math.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_draw {
    use std::mem;

    #[cfg(target_os = "windows")]
    use std::ffi::c_void;

    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::COLORREF;

    #[cfg(target_os = "windows")]
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
        GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER,
        CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DEFAULT_QUALITY, DIB_RGB_COLORS,
        DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD, FW_NORMAL, HFONT, HGDIOBJ,
        OUT_DEFAULT_PRECIS, TRANSPARENT,
    };

    #[cfg(target_os = "windows")]
    use windows::core::PCWSTR;

    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::HWND;
    #[cfg(target_os = "windows")]
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, SWP_FRAMECHANGED,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_TRANSPARENT,
    };

    // ── Pixel helpers ─────────────────────────────────────────────────────────

    /// Pack an RGBA colour into a premultiplied 32-bpp DIB pixel.
    /// Memory layout: [B, G, R, A] → u32 = (A<<24)|(R<<16)|(G<<8)|B.
    #[inline]
    pub fn premult(r: u8, g: u8, b: u8, a: u8) -> u32 {
        let a32 = a as u32;
        ((a32) << 24)
            | ((r as u32 * a32 / 255) << 16)
            | ((g as u32 * a32 / 255) << 8)
            | (b as u32 * a32 / 255)
    }

    /// Porter-Duff src-over on two premultiplied pixels.
    #[inline]
    pub fn blend_over(src: u32, dst: u32) -> u32 {
        let sa = src >> 24;
        let inv = 255 - sa;
        let ch = |sh: u32| ((src >> sh & 0xFF) + (((dst >> sh & 0xFF) * inv) >> 8)).min(255);
        ((sa + (((dst >> 24) * inv) >> 8)).min(255) << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
    }

    /// Fill a rounded rectangle into the DIB pixel slice.
    #[allow(clippy::too_many_arguments)]
    pub fn fill_rrect(
        pix: &mut [u32],
        dw: i32,
        dh: i32,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        r: i32,
        c: u32,
    ) {
        let (x1, y1) = (x1.max(0), y1.max(0));
        let (x2, y2) = (x2.min(dw), y2.min(dh));
        let rf = r as f32;
        for y in y1..y2 {
            for x in x1..x2 {
                if (x < x1 + r || x >= x2 - r) && (y < y1 + r || y >= y2 - r) {
                    let cx = if x < x1 + r {
                        (x1 + r) as f32
                    } else {
                        (x2 - r) as f32
                    };
                    let cy = if y < y1 + r {
                        (y1 + r) as f32
                    } else {
                        (y2 - r) as f32
                    };
                    let dx = x as f32 - cx;
                    let dy = y as f32 - cy;
                    if dx * dx + dy * dy > rf * rf {
                        continue;
                    }
                }
                let idx = (y * dw + x) as usize;
                pix[idx] = blend_over(c, pix[idx]);
            }
        }
    }

    /// Fill a plain rectangle into the DIB pixel slice.
    #[allow(clippy::too_many_arguments)]
    pub fn fill_rect(
        pix: &mut [u32],
        dw: i32,
        dh: i32,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        c: u32,
    ) {
        let (x1, y1) = (x1.max(0), y1.max(0));
        let (x2, y2) = (x2.min(dw), y2.min(dh));
        for y in y1..y2 {
            let base = (y * dw) as usize;
            for x in x1 as usize..x2 as usize {
                pix[base + x] = blend_over(c, pix[base + x]);
            }
        }
    }

    // ── GDI text → DIB compositing ────────────────────────────────────────────

    /// Render `text` with GDI (white on opaque black) into an `rw × rh` glyph
    /// coverage mask (0 = no ink, 255 = full ink), derived from the brightness
    /// channel so anti-aliasing survives as partial coverage.
    ///
    /// # Safety
    /// `font` must be a valid `HFONT`. Must be called on a thread with an
    /// active GDI context (this function creates and tears down its own DC).
    #[cfg(target_os = "windows")]
    unsafe fn render_glyph_coverage(
        text: &str,
        font: HFONT,
        rw: i32,
        rh: i32,
        dt_flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
    ) -> Option<Vec<u8>> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: rw,
                biHeight: -rh,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };

        let hdc_screen = GetDC(None);
        let hdc_tmp = CreateCompatibleDC(hdc_screen);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(hbm) = CreateDIBSection(hdc_screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc_tmp);
            ReleaseDC(None, hdc_screen);
            return None;
        };
        ReleaseDC(None, hdc_screen);

        let tmp = std::slice::from_raw_parts_mut(bits as *mut u32, (rw * rh) as usize);
        // Opaque black background. GDI will not write alpha; any non-black pixel
        // after drawing is text coverage (handles anti-aliasing correctly).
        tmp.fill(0xFF00_0000u32);

        let old_bm = SelectObject(hdc_tmp, HGDIOBJ(hbm.0));
        let old_fn = SelectObject(hdc_tmp, HGDIOBJ(font.0));
        SetBkMode(hdc_tmp, TRANSPARENT);
        SetTextColor(hdc_tmp, COLORREF(0x00FF_FFFF)); // white text

        let mut rect = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: rw,
            bottom: rh,
        };
        let mut tw: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let len = tw.len() - 1;
        DrawTextW(
            hdc_tmp,
            &mut tw[..len],
            &mut rect,
            DT_SINGLELINE | DT_VCENTER | dt_flags,
        );

        SelectObject(hdc_tmp, old_fn);
        SelectObject(hdc_tmp, old_bm);
        let _ = DeleteDC(hdc_tmp);

        let coverage = tmp
            .iter()
            .map(|&px| ((px >> 16 & 0xFF).max(px >> 8 & 0xFF).max(px & 0xFF)) as u8)
            .collect();

        let _ = DeleteObject(HGDIOBJ(hbm.0));
        Some(coverage)
    }

    /// Composite a glyph coverage mask into `pix` at `(dx, dy)` offset by
    /// `(ox, oy)`, tinted with `rgb` scaled by `alpha`.
    #[cfg(target_os = "windows")]
    #[allow(clippy::too_many_arguments)]
    fn composite_coverage(
        pix: &mut [u32],
        dw: i32,
        dh: i32,
        coverage: &[u8],
        rw: i32,
        dx: i32,
        dy: i32,
        ox: i32,
        oy: i32,
        (tr, tg, tb): (u8, u8, u8),
        alpha: f32,
    ) {
        for (i, &cov) in coverage.iter().enumerate() {
            if cov == 0 {
                continue;
            }
            let sa = (cov as f32 / 255.0 * alpha * 255.0) as u8;
            if sa == 0 {
                continue;
            }
            let sx = dx + ox + (i as i32 % rw);
            let sy = dy + oy + (i as i32 / rw);
            if sx < 0 || sy < 0 || sx >= dw || sy >= dh {
                continue;
            }
            let di = (sy * dw + sx) as usize;
            pix[di] = blend_over(premult(tr, tg, tb, sa), pix[di]);
        }
    }

    /// Draw `text` with GDI, then alpha-composite with `text_rgb` into `pix`.
    ///
    /// `dt_flags` are `DrawTextW` flags OR'd on top of the fixed
    /// `DT_SINGLELINE | DT_VCENTER` — callers must include an alignment flag
    /// (`DT_LEFT` or `DT_RIGHT`) themselves, plus anything else needed (e.g.
    /// `DT_END_ELLIPSIS` for truncating names that don't fit their column).
    ///
    /// # Safety
    /// `font` must be a valid `HFONT` and `pix` must be a properly sized,
    /// writable `dw * dh` pixel buffer. Must be called on a thread with an
    /// active GDI context (this function creates and tears down its own DC).
    #[cfg(target_os = "windows")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn composite_text(
        pix: &mut [u32],
        dw: i32,
        dh: i32,
        text: &str,
        font: HFONT,
        text_rgb: (u8, u8, u8),
        alpha: f32,
        dx: i32,
        dy: i32,
        rw: i32,
        rh: i32,
        dt_flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
    ) {
        if rw <= 0 || rh <= 0 || alpha < 0.004 {
            return;
        }
        let Some(coverage) = render_glyph_coverage(text, font, rw, rh, dt_flags) else {
            return;
        };
        composite_coverage(pix, dw, dh, &coverage, rw, dx, dy, 0, 0, text_rgb, alpha);
    }

    /// Like [`composite_text`], but first draws a solid-colour stroke by
    /// compositing the same glyph coverage mask at several pixel offsets
    /// around the fill position — cheap "poor man's outline" that only pays
    /// for one GDI render pass regardless of stroke width.
    ///
    /// # Safety
    /// Same requirements as [`composite_text`].
    #[cfg(target_os = "windows")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn composite_text_stroked(
        pix: &mut [u32],
        dw: i32,
        dh: i32,
        text: &str,
        font: HFONT,
        text_rgb: (u8, u8, u8),
        stroke_rgb: (u8, u8, u8),
        stroke_width: i32,
        alpha: f32,
        dx: i32,
        dy: i32,
        rw: i32,
        rh: i32,
        dt_flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
    ) {
        if rw <= 0 || rh <= 0 || alpha < 0.004 {
            return;
        }
        let Some(coverage) = render_glyph_coverage(text, font, rw, rh, dt_flags) else {
            return;
        };

        if stroke_width > 0 {
            let w = stroke_width;
            for oy in -w..=w {
                for ox in -w..=w {
                    if ox == 0 && oy == 0 || ox * ox + oy * oy > w * w + 1 {
                        continue;
                    }
                    composite_coverage(
                        pix, dw, dh, &coverage, rw, dx, dy, ox, oy, stroke_rgb, alpha,
                    );
                }
            }
        }
        composite_coverage(pix, dw, dh, &coverage, rw, dx, dy, 0, 0, text_rgb, alpha);
    }

    // ── Utilities ─────────────────────────────────────────────────────────────

    /// # Safety
    /// Must be called on a thread that can create GDI objects. The returned
    /// `HFONT` is owned by the caller and must eventually be released via
    /// `DeleteObject`.
    #[cfg(target_os = "windows")]
    pub unsafe fn make_font(name: &str, pt_size: i32, bold: bool) -> HFONT {
        let height = -(pt_size * 96 / 72);
        let weight = if bold {
            FW_BOLD.0 as i32
        } else {
            FW_NORMAL.0 as i32
        };
        let nw: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut face = [0u16; 32];
        let n = nw.len().min(31);
        face[..n].copy_from_slice(&nw[..n]);
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR(face.as_ptr()),
        )
    }

    /// Toggle click-through (`WS_EX_TRANSPARENT`) on an overlay window in
    /// response to its lock state. Shared by all three overlay windows
    /// (alert deck, history, meter) so they can't drift on the style bits —
    /// while locked, mouse messages pass straight through to the game, which
    /// also means a locked window can no longer receive the click that would
    /// unlock it; unlocking has to happen via the Settings dialog checkbox,
    /// picked up on the next config poll.
    ///
    /// # Safety
    /// `hwnd` must be a valid window handle.
    #[cfg(target_os = "windows")]
    pub unsafe fn apply_lock_style(hwnd: HWND, locked: bool) {
        let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new = if locked {
            cur | WS_EX_TRANSPARENT.0 as isize
        } else {
            cur & !(WS_EX_TRANSPARENT.0 as isize)
        };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }

    /// Parse `#RRGGBB` or `RRGGBB` → `0x00RRGGBB`.
    pub fn parse_hex_color(s: &str) -> Option<u32> {
        let s = s.trim_start_matches('#');
        if s.len() == 6 {
            let r = u32::from_str_radix(&s[0..2], 16).ok()?;
            let g = u32::from_str_radix(&s[2..4], 16).ok()?;
            let b = u32::from_str_radix(&s[4..6], 16).ok()?;
            Some((r << 16) | (g << 8) | b)
        } else {
            None
        }
    }
}
