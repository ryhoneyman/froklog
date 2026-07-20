/// Unified Settings window — the single dialog behind the tray's "Settings…"
/// item, hosting every settings surface as a tab via a Win32 TabControl:
///   Tab 0 — General    : spell icon import, overall sound volume
///   Tab 1 — Logging    : log path, server URL, player name, registration
///   Tab 2 — Triggers   : listbox of triggers + add/edit/delete/enable controls
///   Tab 3 — Overlays   : alert/history overlay font, size, timing, opacity
///   Tab 4 — DPS Meter  : max rows, idle-hide, font size
///   Tab 5 — Voice      : TTS engine, speed, priority filters
///   Tab 6 — Windows    : per-window enable/position/lock for the alert
///                        overlay, history overlay, and DPS meter
///   Tab 7 — Sounds     : sound enable/volume, sound label + package
///                        management (see `sound_packages`)
///
/// Trigger editing opens a child modal dialog (TriggerEditDialog) that contains
/// two listboxes — one for CONDITIONS, one for ACTIONS — each with their own
/// add/edit/delete/reorder buttons.  Conditions and actions are edited in small
/// focused sub-dialogs (ConditionEditDialog, ActionEditDialog).
#[cfg(feature = "tray")]
pub mod overlay_config {
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::{
        AlphaBlend, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject,
        FillRect, GetDC, GetStockObject, InvalidateRect, ReleaseDC, SelectObject, SetBkMode,
        SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, COLOR_BTNFACE,
        DEFAULT_GUI_FONT, DIB_RGB_COLORS, HBITMAP, HBRUSH, HDC, HGDIOBJ, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Controls::Dialogs::{
        ChooseColorW, GetOpenFileNameW, GetSaveFileNameW, CC_RGBINIT, CHOOSECOLORW,
        CHOOSECOLOR_FLAGS, OFN_FILEMUSTEXIST, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
        OPENFILENAMEW,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
        IsDialogMessageW, LoadCursorW, MessageBoxW, PeekMessageW, PostMessageW, PostQuitMessage,
        RegisterClassExW, SendMessageW, SetCursor, SetForegroundWindow, SetWindowLongPtrW,
        SetWindowPos, SetWindowTextW, TranslateMessage, WindowFromPoint, CB_ADDSTRING,
        CB_FINDSTRINGEXACT, CB_GETCURSEL, CB_GETLBTEXT, CB_GETLBTEXTLEN, CB_SETCURSEL,
        CREATESTRUCTW, GWLP_USERDATA, HMENU, IDC_ARROW, IDC_HAND, LB_ADDSTRING, LB_GETCURSEL,
        MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, MESSAGEBOX_STYLE, MSG,
        PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SWP_NOZORDER, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
        WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_HSCROLL, WM_LBUTTONDOWN, WM_MBUTTONDBLCLK,
        WM_NOTIFY, WM_SETCURSOR, WM_SETFONT, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD,
        WS_EX_APPWINDOW, WS_EX_DLGMODALFRAME, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    };

    use crate::config::{Config, TtsAudioMode, TtsSpeed};
    use crate::overlay_draw::overlay_draw::premult;
    use crate::tray::tray::AppHandle;
    use crate::triggers::engine::{
        Action, Condition, ConditionLogic, MatchType, Treatment, TriggerConfig, TriggerDef, VarOp,
        VoicePriority,
    };

    // ── Control IDs ───────────────────────────────────────────────────────────

    // Logging tab (log path, server, player, registration)
    const IDC_GAME_COMBO: i32 = 101;
    const IDC_SERVER_COMBO: i32 = 102;
    const IDC_PLAYER_EDIT: i32 = 103;
    const IDC_LOGFILE_EDIT: i32 = 104;
    const IDC_LOGFILE_BROWSE: i32 = 105;
    const IDC_URL_EDIT: i32 = 106;
    const IDC_URL_TEST: i32 = 107;
    const IDC_URL_STATUS: i32 = 108;
    const IDC_STREAMID_VALUE: i32 = 109;
    const IDC_PASSWORD_EDIT: i32 = 110;
    const IDC_REGISTER_BTN: i32 = 111;
    const IDC_PUBLIC_CHECK: i32 = 112;
    const IDC_REMOTE_LOGGING_CHECK: i32 = 113;
    const IDC_COPY_STREAMID: i32 = 115;

    // General tab (spell icon import, sound volume)
    const IDC_IMPORT_SPELL_ICONS: i32 = 114;
    const IDC_SOUND_VOLUME_SLIDER: i32 = 116;
    const IDC_SOUND_ENABLED_CHECK: i32 = 117;

    // DPS Meter tab
    const IDC_METER_ENABLED: i32 = 150;
    const IDC_METER_LOCKED: i32 = 151;
    const IDC_METER_MAX_ROWS: i32 = 152;
    const IDC_METER_IDLE_SECS: i32 = 153;
    const IDC_METER_FONT_SIZE: i32 = 154;
    const IDC_METER_RESET_POS: i32 = 155;
    const IDC_METER_X: i32 = 156;
    const IDC_METER_Y: i32 = 157;

    // Async result messages posted from background threads (Logging/General tabs).
    const WM_URL_TEST_DONE: u32 = WM_APP + 1;
    const WM_REGISTER_DONE: u32 = WM_APP + 2;
    const WM_SPELL_ICONS_DONE: u32 = WM_APP + 5;
    // Posted by the DPS meter's gear icon to bring an already-open Settings
    // window to the front and switch it to a specific tab. wparam = tab index.
    pub(crate) const WM_SWITCH_TAB: u32 = WM_APP + 3;
    /// Tab index for the DPS Meter tab, used by `overlay_dps.rs`'s gear icon.
    pub(crate) const TAB_DPS_METER: i32 = 4;
    /// Posted by the tray icon's right-click handler to bring an already-open
    /// Settings window to the front without changing its current tab.
    pub(crate) const WM_BRING_TO_FRONT: u32 = WM_APP + 4;

    // Tab control
    const IDC_TAB: i32 = 200;

    // Triggers tab
    const IDC_TRIGGER_LIST: i32 = 201;
    const IDC_BTN_ADD: i32 = 202;
    const IDC_BTN_EDIT: i32 = 203;
    const IDC_BTN_DELETE: i32 = 204;
    const IDC_BTN_MOVE_UP: i32 = 205;
    const IDC_BTN_MOVE_DOWN: i32 = 206;
    const IDC_BTN_TOGGLE: i32 = 207;

    // Appearance tab
    const IDC_FONT_COMBO: i32 = 210;
    const IDC_START_FONT_SIZE: i32 = 211;
    const IDC_ALPHA_EDIT: i32 = 212;
    const IDC_MAX_FONT_SIZE: i32 = 213;
    const IDC_FLY_MS: i32 = 214;
    const IDC_OVERLAY_ENABLED: i32 = 215;
    const IDC_OVERLAY_X: i32 = 216;
    const IDC_OVERLAY_Y: i32 = 217;
    const IDC_HOLD_SECS: i32 = 222;
    const IDC_HIST_FONT_SIZE: i32 = 223;
    const IDC_HIST_IDLE: i32 = 224;
    const IDC_HIST_MAX_ENTRIES: i32 = 225;
    const IDC_HIST_WIDTH: i32 = 226;
    const IDC_HIST_X: i32 = 227;
    const IDC_HIST_Y: i32 = 228;
    const IDC_OVERLAY_LOCKED: i32 = 229;
    const IDC_HIST_LOCKED: i32 = 230;
    const IDC_HIST_ENABLED: i32 = 231;
    const IDC_SHOW_ALL_WINDOWS: i32 = 232;
    const IDC_OVERLAY_RESET_POS: i32 = 233;
    const IDC_HIST_RESET_POS: i32 = 234;

    // Bottom buttons
    const IDC_SAVE: i32 = 220;
    const IDC_CANCEL: i32 = 221;

    // Trigger edit dialog
    const IDC_EDIT_NAME: i32 = 300;
    const IDC_EDIT_ENABLED: i32 = 301;
    const IDC_COND_LOGIC: i32 = 302;
    const IDC_COND_LIST: i32 = 303;
    const IDC_COND_ADD: i32 = 304;
    const IDC_COND_EDIT: i32 = 305;
    const IDC_COND_DEL: i32 = 306;
    const IDC_COND_UP: i32 = 307;
    const IDC_COND_DOWN: i32 = 308;
    const IDC_ACTION_LIST: i32 = 309;
    const IDC_ACTION_ADD: i32 = 310;
    const IDC_ACTION_EDIT: i32 = 311;
    const IDC_ACTION_DEL: i32 = 312;
    const IDC_ACTION_UP: i32 = 313;
    const IDC_ACTION_DOWN: i32 = 314;
    const IDC_EDIT_OK: i32 = 315;
    const IDC_EDIT_CANCEL: i32 = 316;

    // Condition edit dialog
    const IDC_COND_TYPE: i32 = 400;
    const IDC_COND_MATCH_TYPE: i32 = 401;
    const IDC_COND_PATTERN: i32 = 402;
    const IDC_COND_VAR_NAME: i32 = 403;
    const IDC_COND_VAR_OP: i32 = 404;
    const IDC_COND_VAR_VALUE: i32 = 405;
    const IDC_COND_OK: i32 = 406;
    const IDC_COND_CANCEL: i32 = 407;

    // Action edit dialog
    const IDC_ACTION_TYPE: i32 = 500;
    const IDC_ACTION_ICON: i32 = 501;
    #[allow(dead_code)]
    const IDC_ACTION_COLOR: i32 = 502;
    const IDC_ACTION_MESSAGE: i32 = 503;
    const IDC_ACTION_DELAY: i32 = 504;
    const IDC_ACTION_SOUND: i32 = 505;
    const IDC_ACTION_SOUND_TEST: i32 = 520;
    const IDC_ACTION_VAR_NAME: i32 = 507;
    const IDC_ACTION_VAR_VALUE: i32 = 508;
    const IDC_ACTION_OK: i32 = 509;
    const IDC_ACTION_CANCEL: i32 = 510;
    const IDC_ACTION_MSG_COLOR_BTN: i32 = 511;
    const IDC_ACTION_ICON_COLOR_BTN: i32 = 512;
    const IDC_ACTION_BORDER_COLOR_BTN: i32 = 518;
    // Voice Alert action fields
    const IDC_ACTION_TTS_TEXT: i32 = 513;
    const IDC_ACTION_PRIORITY_EMERGENCY: i32 = 514;
    const IDC_ACTION_PRIORITY_OPERATIONAL: i32 = 515;
    const IDC_ACTION_PRIORITY_AMBIENT: i32 = 516;
    const IDC_ACTION_TREATMENT: i32 = 517;
    const IDC_ACTION_OVERLAY_PRIORITY: i32 = 519;

    // Voice tab controls
    const IDC_VOICE_TTS_ENABLED: i32 = 600;
    const IDC_VOICE_SPEED: i32 = 601;
    const IDC_VOICE_MODE_SMART: i32 = 602;
    const IDC_VOICE_MODE_QUEUE: i32 = 603;
    const IDC_VOICE_MODE_INTERRUPT: i32 = 604;
    const IDC_VOICE_READ_EMERGENCY: i32 = 605;
    const IDC_VOICE_READ_OPERATIONAL: i32 = 606;
    const IDC_VOICE_READ_AMBIENT: i32 = 607;
    const IDC_VOICE_VOICE_COMBO: i32 = 608;

    // Sounds tab
    const IDC_SOUND_LABEL_LIST: i32 = 900;
    const IDC_SOUND_LABEL_ADD: i32 = 901;
    const IDC_SOUND_LABEL_EDIT: i32 = 902;
    const IDC_SOUND_LABEL_DELETE: i32 = 903;
    const IDC_SOUND_PKG_COMBO: i32 = 904;
    const IDC_SOUND_PKG_NEW: i32 = 905;
    const IDC_SOUND_PKG_RENAME: i32 = 906;
    const IDC_SOUND_PKG_DELETE: i32 = 907;
    const IDC_SOUND_PKG_EXPORT: i32 = 908;
    const IDC_SOUND_PKG_IMPORT: i32 = 909;
    // Sound label edit dialog
    const IDC_SLBL_NAME: i32 = 910;
    const IDC_SLBL_FILE: i32 = 911;
    const IDC_SLBL_BROWSE: i32 = 912;
    const IDC_SLBL_TEST: i32 = 913;
    const IDC_SLBL_OK: i32 = 914;
    const IDC_SLBL_CANCEL: i32 = 915;
    // Generic text-prompt dialog (New/Rename package)
    const IDC_PROMPT_EDIT: i32 = 920;
    const IDC_PROMPT_OK: i32 = 921;
    const IDC_PROMPT_CANCEL: i32 = 922;

    // Win32 control style / message constants.
    const SS_LEFT: u32 = 0x0000_0000;
    const SS_ETCHEDHORZ: u32 = 0x0000_0010;
    const SS_NOTIFY: u32 = 0x0000_0100;
    const BS_PUSHBUTTON: u32 = 0x0000_0000;
    const BS_DEFPUSHBUTTON: u32 = 0x0000_0001;
    const BS_AUTOCHECKBOX: u32 = 0x0000_0003;
    const BS_GROUPBOX: u32 = 0x0000_0007;
    const BS_AUTORADIOBUTTON: u32 = 0x0000_0009;
    const WS_GROUP_VAL: u32 = 0x0002_0000;
    const CBS_DROPDOWNLIST: u32 = 0x0000_0003;
    const CBS_HASSTRINGS: u32 = 0x0000_0200;
    const ES_AUTOHSCROLL: u32 = 0x0000_0080;
    const ES_NUMBER: u32 = 0x0000_2000;
    const ES_READONLY: u32 = 0x0000_0800;
    const ES_PASSWORD: u32 = 0x0000_0020;
    const EN_CHANGE: usize = 0x0300;
    const LBS_NOTIFY: u32 = 0x0001;
    const LBS_HASSTRINGS: u32 = 0x0040;
    const WS_VSCROLL_VAL: u32 = 0x0020_0000;
    const EM_SETCUEBANNER: u32 = 0x1501;
    const BM_SETCHECK: u32 = 0x00F1;
    const BM_GETCHECK: u32 = 0x00F0;
    const BST_CHECKED: usize = 1;
    const LBN_DBLCLK: usize = 2;
    const LBN_SELCHANGE: usize = 1;
    const CBN_SELCHANGE: usize = 1;
    // STN_CLICKED for SS_NOTIFY statics and BN_CLICKED for buttons are both 0;
    // STN_ENABLE(2)/STN_DISABLE(3) share the static's control ID too, so an
    // EnableWindow() call on a swatch reenters WM_COMMAND with that same id —
    // handlers must check for this to avoid treating it as a real click.
    const STN_CLICKED: usize = 0;
    const TCM_INSERTITEMW: u32 = 0x133E;
    const TCM_GETCURSEL: u32 = 0x130B;
    const TCM_SETCURSEL: u32 = 0x130C;
    const TCM_ADJUSTRECT: u32 = 0x1328;
    const LB_SETCURSEL: u32 = 0x0186;
    const LB_RESETCONTENT: u32 = 0x0184;
    const LB_GETTEXT: u32 = 0x0189;
    const LB_GETTEXTLEN: u32 = 0x018A;
    const CB_RESETCONTENT: u32 = 0x014B;
    const CBS_OWNERDRAWFIXED: u32 = 0x0010;
    const WM_DRAWITEM: u32 = 0x002B;
    const WM_MEASUREITEM: u32 = 0x002C;
    const WM_CTLCOLORSTATIC: u32 = 0x0138;
    const WM_MOUSEWHEEL: u32 = 0x020A;
    const CB_SETITEMHEIGHT: u32 = 0x0153;
    const ODS_SELECTED: u32 = 0x0001;
    // Trackbar (msctls_trackbar32) — used by the General tab's volume slider.
    const TBM_GETPOS: u32 = 0x0400;
    const TBM_SETRANGE: u32 = 0x0406;
    const TBM_SETPOS: u32 = 0x0405;
    const TBS_HORZ: u32 = 0x0000;
    const TBS_AUTOTICKS: u32 = 0x0001;
    const SWATCH_SIZE: i32 = 20;
    const SWATCH_PAD: i32 = 4;
    const ICON_ITEM_H: i32 = 26;
    /// Max rows shown before the icon combo's dropdown scrolls, so a large
    /// icon set (e.g. hundreds of extracted spell icons) doesn't request an
    /// absurdly tall dropdown — native combo-box scrolling handles the rest.
    const ICON_COMBO_MAX_ROWS: i32 = 14;

    // Owner-draw structs (stable Win32 ABI).
    #[repr(C)]
    struct DrawItemStruct {
        ctl_type: u32,
        ctl_id: u32,
        item_id: u32,
        item_action: u32,
        item_state: u32,
        hwnd_item: HWND,
        hdc: HDC,
        rc_item: windows::Win32::Foundation::RECT,
        item_data: usize,
    }

    #[repr(C)]
    struct MeasureItemStruct {
        ctl_type: u32,
        ctl_id: u32,
        item_id: u32,
        item_width: u32,
        item_height: u32,
        item_data: usize,
    }

    // ── Icon item (dynamic — built at runtime from presets + PNG files) ──────────

    struct IconItem {
        key: String,
        label: String,
        color: u32,
    }

    fn build_icon_items() -> Vec<IconItem> {
        let swatch = crate::assets::icon_swatch_color;
        // "(none)" then "Color Box" lead the list. Color Box's swatch is
        // drawn as a transparency checkerboard in draw_icon_combo_item, not
        // a solid fill — the `color` field here is unused for that entry.
        let mut items = vec![
            IconItem {
                key: String::new(),
                label: "(none)".into(),
                color: 0x00888888,
            },
            IconItem {
                key: "colorbox".into(),
                label: "Color Box".into(),
                color: swatch("colorbox"),
            },
        ];

        // PNG/JPG files from the icons directory, excluding the app's own
        // stock icon set (heart/skull/sword/etc.) — those still work for any
        // pre-existing trigger that references one by filename, they're just
        // not offered as a fresh choice in this list.
        if let Ok(dir) = std::fs::read_dir(crate::assets::icons_dir()) {
            let mut files: Vec<String> = dir
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let p = e.path();
                    matches!(
                        p.extension().and_then(|x| x.to_str()),
                        Some("png") | Some("jpg")
                    )
                })
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|f| !crate::assets::STOCK_ICON_FILES.contains(&f.as_str()))
                .collect();
            files.sort();
            for f in files {
                let label = f
                    .trim_end_matches(".jpg")
                    .trim_end_matches(".png")
                    .to_string();
                let color = swatch(&f);
                items.push(IconItem {
                    key: f,
                    label,
                    color,
                });
            }
        }

        items
    }

    /// Falls back to the "(none)" entry when `key` isn't found, e.g. a
    /// trigger saved with one of the now-removed built-in icons
    /// (heal/damage/warn/spell, or a stock icon like skull/heart/sword).
    fn find_icon_index(items: &[IconItem], key: &str) -> usize {
        items
            .iter()
            .position(|it| it.key == key)
            .or_else(|| items.iter().position(|it| it.key.is_empty()))
            .unwrap_or(0)
    }

    // ── Icon thumbnails (real PNG/JPG previews in the icon combo) ────────────

    /// A decoded icon file, premultiplied into a GDI-ready DIB section.
    /// `hbitmap` must be deleted exactly once — see `ActionEditState`'s
    /// `WM_DESTROY` handler, which owns every entry in `icon_thumbs`.
    struct ThumbBitmap {
        hbitmap: HBITMAP,
        w: i32,
        h: i32,
    }

    /// Lazily decodes and caches `filename` from `icons_dir()` as a
    /// premultiplied DIB thumbnail, capped at `ICON_ITEM_H`-ish so an
    /// oversized custom PNG can't blow up the combo row. `None` is cached
    /// too (decode failure), so a bad file is only ever tried once per
    /// dialog session.
    unsafe fn get_icon_thumb<'a>(
        cache: &'a mut std::collections::HashMap<String, Option<ThumbBitmap>>,
        filename: &str,
    ) -> Option<&'a ThumbBitmap> {
        cache
            .entry(filename.to_string())
            .or_insert_with(|| load_icon_thumb(filename))
            .as_ref()
    }

    unsafe fn load_icon_thumb(filename: &str) -> Option<ThumbBitmap> {
        let path = crate::assets::icons_dir().join(filename);
        let rgba = image::open(&path).ok()?.to_rgba8();
        let (src_w, src_h) = rgba.dimensions();
        // Matches SWATCH_SIZE exactly (not just "fits within the row") so
        // square source icons (every one we ship or extract) land flush in
        // the swatch box with no fractional-pixel centering overflow.
        let max_dim = SWATCH_SIZE as u32;
        let rgba = if src_w > max_dim || src_h > max_dim {
            image::imageops::resize(
                &rgba,
                max_dim,
                max_dim,
                image::imageops::FilterType::Nearest,
            )
        } else {
            rgba
        };
        let (w, h) = rgba.dimensions();
        let (w, h) = (w as i32, h as i32);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // negative = top-down DIB, matching our row-major decode
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let hdc_screen = GetDC(None);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbm_result = CreateDIBSection(hdc_screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
        ReleaseDC(None, hdc_screen);
        let hbitmap = hbm_result.ok()?;

        let pix = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        for (i, chunk) in rgba.into_raw().chunks_exact(4).enumerate() {
            pix[i] = premult(chunk[0], chunk[1], chunk[2], chunk[3]);
        }

        Some(ThumbBitmap { hbitmap, w, h })
    }

    /// Renders the classic white/light-grey transparency checkerboard (as
    /// seen in color pickers everywhere) into a `w`x`h` box at `(x, y)`,
    /// used for the "Color Box" entry's swatch instead of a solid fill —
    /// that entry has no fixed color, the user picks one separately.
    unsafe fn draw_transparency_checkerboard(hdc: HDC, x: i32, y: i32, w: i32, h: i32) {
        const CELL: i32 = 5;
        let white = CreateSolidBrush(COLORREF(0x00FFFFFF));
        let grey = CreateSolidBrush(COLORREF(0x00C0C0C0));
        let mut row = 0;
        let mut cy = y;
        while cy < y + h {
            let mut col = 0;
            let mut cx = x;
            while cx < x + w {
                let rect = windows::Win32::Foundation::RECT {
                    left: cx,
                    top: cy,
                    right: (cx + CELL).min(x + w),
                    bottom: (cy + CELL).min(y + h),
                };
                let brush = if (row + col) % 2 == 0 { white } else { grey };
                FillRect(hdc, &rect, brush);
                cx += CELL;
                col += 1;
            }
            cy += CELL;
            row += 1;
        }
        let _ = DeleteObject(HGDIOBJ(white.0));
        let _ = DeleteObject(HGDIOBJ(grey.0));
    }

    unsafe fn draw_icon_combo_item(
        dis: &DrawItemStruct,
        icon_items: &[IconItem],
        thumb_cache: &mut std::collections::HashMap<String, Option<ThumbBitmap>>,
    ) {
        if dis.item_id == u32::MAX {
            return;
        }
        let Some(item) = icon_items.get(dis.item_id as usize) else {
            return;
        };
        let key = &item.key;
        let label = &item.label;
        let selected = dis.item_state & ODS_SELECTED != 0;
        let (bg, fg): (u32, u32) = if selected {
            (0x00D77800, 0x00FFFFFF)
        } else {
            (0x00FFFFFF, 0x00000000)
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg));
        FillRect(dis.hdc, &dis.rc_item, bg_brush);
        let _ = DeleteObject(HGDIOBJ(bg_brush.0));

        if !key.is_empty() {
            let sw_x = dis.rc_item.left + SWATCH_PAD;
            let sw_y = dis.rc_item.top + (ICON_ITEM_H - SWATCH_SIZE) / 2;
            let is_image_file = key.ends_with(".png") || key.ends_with(".jpg");
            let thumb = if is_image_file {
                get_icon_thumb(thumb_cache, key)
            } else {
                None
            };
            if let Some(thumb) = thumb {
                let hdc_mem = CreateCompatibleDC(dis.hdc);
                let old = SelectObject(hdc_mem, HGDIOBJ(thumb.hbitmap.0));
                let blend = BLENDFUNCTION {
                    BlendOp: 0,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: 1,
                };
                let _ = AlphaBlend(
                    dis.hdc,
                    sw_x + (SWATCH_SIZE - thumb.w) / 2,
                    sw_y + (SWATCH_SIZE - thumb.h) / 2,
                    thumb.w,
                    thumb.h,
                    hdc_mem,
                    0,
                    0,
                    thumb.w,
                    thumb.h,
                    blend,
                );
                SelectObject(hdc_mem, old);
                let _ = DeleteDC(hdc_mem);
            } else if key == "colorbox" {
                draw_transparency_checkerboard(dis.hdc, sw_x, sw_y, SWATCH_SIZE, SWATCH_SIZE);
            } else {
                let sw_brush = CreateSolidBrush(COLORREF(item.color));
                let sw_rect = windows::Win32::Foundation::RECT {
                    left: sw_x,
                    top: sw_y,
                    right: sw_x + SWATCH_SIZE,
                    bottom: sw_y + SWATCH_SIZE,
                };
                FillRect(dis.hdc, &sw_rect, sw_brush);
                let _ = DeleteObject(HGDIOBJ(sw_brush.0));
            }
        }

        SetBkMode(dis.hdc, TRANSPARENT);
        SetTextColor(dis.hdc, COLORREF(fg));
        let text_x = dis.rc_item.left + SWATCH_PAD * 2 + SWATCH_SIZE;
        let text_y = dis.rc_item.top + (ICON_ITEM_H - 14) / 2;
        let lw: Vec<u16> = label.encode_utf16().collect();
        let _ = TextOutW(dis.hdc, text_x, text_y, &lw);
    }

    const CLASS_OVERLAY_CFG: &str = "FroklogOverlayCfg\0";
    const CLASS_TRIGGER_EDIT: &str = "FroklogTriggerEdit\0";
    const CLASS_COND_EDIT: &str = "FroklogCondEdit\0";
    const CLASS_ACTION_EDIT: &str = "FroklogActionEdit\0";
    const CLASS_SOUND_LABEL_EDIT: &str = "FroklogSoundLabelEdit\0";
    const CLASS_PROMPT_TEXT: &str = "FroklogPromptText\0";

    const FONT_NAMES: &[&str] = &[
        "Segoe UI",
        "Arial",
        "Consolas",
        "Courier New",
        "Tahoma",
        "Verdana",
        "Times New Roman",
    ];

    // ── Logging tab static data ───────────────────────────────────────────────

    const GAMES: &[&str] = &["Everquest Legends"];
    const GAME_IDS: &[&str] = &["eql"];
    const SERVERS: &[&str] = &["Test"];

    fn label_to_game_id(label: &str) -> &'static str {
        GAMES
            .iter()
            .position(|&g| g == label)
            .and_then(|i| GAME_IDS.get(i).copied())
            .unwrap_or("eql")
    }

    fn migrate_game(s: String) -> String {
        // Old configs stored the display label; normalise to the ID.
        if GAMES.contains(&s.as_str()) {
            label_to_game_id(&s).to_string()
        } else if s.is_empty() {
            GAME_IDS[0].to_string()
        } else {
            s
        }
    }

    // ── Main dialog state ─────────────────────────────────────────────────────

    struct ConfigState {
        handle: Arc<AppHandle>,
        triggers: TriggerConfig,
        cfg: Config,
        tab_hwnd: HWND,
        // General tab
        general_panel: HWND,
        general_controls: Vec<HWND>,
        btn_import_spell_icons: HWND,
        lbl_import_spell_icons_status: HWND,
        chk_sound_enabled: HWND,
        edit_sound_volume: HWND,
        lbl_sound_volume_value: HWND,
        // Logging tab
        logging_panel: HWND,
        logging_controls: Vec<HWND>,
        combo_game: HWND,
        combo_server: HWND,
        edit_player: HWND,
        edit_logfile: HWND,
        edit_url: HWND,
        lbl_url_status: HWND,
        lbl_streamid: HWND,
        edit_password: HWND,
        btn_register: HWND,
        btn_copy_streamid: HWND,
        chk_public: HWND,
        chk_remote_logging: HWND,
        draft_log_path: String,
        draft_server_url: String,
        draft_player: String,
        draft_server: String,
        draft_game: String,
        draft_password: String,
        draft_public: bool,
        draft_remote_logging: bool,
        is_registered: bool,
        stream_id_text: String,
        player_user_set: bool,
        server_user_set: bool,
        // DPS Meter tab
        meter_panel: HWND,
        meter_controls: Vec<HWND>,
        chk_meter_enabled: HWND,
        chk_meter_locked: HWND,
        edit_meter_max_rows: HWND,
        edit_meter_idle_secs: HWND,
        edit_meter_font_size: HWND,
        edit_meter_x: HWND,
        edit_meter_y: HWND,
        trigger_list: HWND,
        btn_add: HWND,
        btn_edit: HWND,
        btn_delete: HWND,
        btn_move_up: HWND,
        btn_move_down: HWND,
        btn_toggle: HWND,
        font_combo: HWND,
        edit_start_font_size: HWND,
        edit_max_font_size: HWND,
        edit_alpha: HWND,
        edit_fly_ms: HWND,
        edit_hold_secs: HWND,
        chk_overlay_enabled: HWND,
        edit_overlay_x: HWND,
        edit_overlay_y: HWND,
        chk_overlay_locked: HWND,
        edit_hist_font_size: HWND,
        edit_hist_idle: HWND,
        edit_hist_max_entries: HWND,
        edit_hist_width: HWND,
        edit_hist_x: HWND,
        edit_hist_y: HWND,
        chk_hist_locked: HWND,
        chk_hist_enabled: HWND,
        triggers_panel: HWND,
        appearance_panel: HWND,
        appearance_controls: Vec<HWND>,
        // Windows tab (per-window enable/position/lock)
        windows_panel: HWND,
        windows_controls: Vec<HWND>,
        // Sounds tab
        sounds_panel: HWND,
        sounds_controls: Vec<HWND>,
        combo_sound_pkg: HWND,
        sound_label_list: HWND,
        btn_sound_label_edit: HWND,
        btn_sound_label_delete: HWND,
        // Voice tab
        voice_panel: HWND,
        voice_controls: Vec<HWND>,
        chk_tts_enabled: HWND,
        combo_tts_speed: HWND,
        radio_tts_smart: HWND,
        radio_tts_queue: HWND,
        radio_tts_interrupt: HWND,
        chk_tts_emergency: HWND,
        chk_tts_operational: HWND,
        chk_tts_ambient: HWND,
        combo_tts_voice: HWND,
        /// (display_name, token_key_id) — populated when Voice tab is built.
        voice_names: Vec<(String, String)>,
        current_tab: i32,
    }

    // ── Public entry point ────────────────────────────────────────────────────

    /// Opens the unified Settings dialog, landing on `initial_tab` (0=General,
    /// 1=Logging, 2=Triggers, 3=Overlays, 4=DPS Meter, 5=Voice, 6=Windows,
    /// 7=Sounds). Callers are expected to
    /// guard against opening a second instance via `AppHandle.settings_open`;
    /// if a dialog is already open, post `WM_SWITCH_TAB` to
    /// `AppHandle.settings_hwnd` instead of calling this again.
    pub fn open_settings(handle: Arc<AppHandle>, initial_tab: i32) {
        std::thread::Builder::new()
            .name("froklog-settings".into())
            .spawn(move || run_config_thread(handle, initial_tab))
            .expect("spawn settings thread");
    }

    fn run_config_thread(handle: Arc<AppHandle>, initial_tab: i32) {
        // COM is needed for SAPI voice enumeration in the Voice tab.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }

        let cfg = handle.config.lock().unwrap().clone();
        let triggers = TriggerConfig::load();

        let state = Box::new(ConfigState {
            handle: Arc::clone(&handle),
            triggers,
            general_panel: HWND::default(),
            general_controls: Vec::new(),
            btn_import_spell_icons: HWND::default(),
            lbl_import_spell_icons_status: HWND::default(),
            chk_sound_enabled: HWND::default(),
            edit_sound_volume: HWND::default(),
            lbl_sound_volume_value: HWND::default(),
            logging_panel: HWND::default(),
            logging_controls: Vec::new(),
            combo_game: HWND::default(),
            combo_server: HWND::default(),
            edit_player: HWND::default(),
            edit_logfile: HWND::default(),
            edit_url: HWND::default(),
            lbl_url_status: HWND::default(),
            lbl_streamid: HWND::default(),
            edit_password: HWND::default(),
            btn_register: HWND::default(),
            btn_copy_streamid: HWND::default(),
            chk_public: HWND::default(),
            chk_remote_logging: HWND::default(),
            draft_log_path: cfg.log_path.clone().unwrap_or_default(),
            draft_server_url: cfg.server_url.clone().unwrap_or_default(),
            draft_player: cfg.effective_player_name(),
            draft_server: cfg
                .server_name
                .clone()
                .or_else(|| cfg.server_name_from_log())
                .unwrap_or_else(|| SERVERS[0].to_string()),
            draft_game: migrate_game(cfg.game.clone().unwrap_or_default()),
            draft_password: cfg.stream_password.clone().unwrap_or_default(),
            draft_public: cfg.public_stream,
            draft_remote_logging: cfg.remote_logging_enabled,
            is_registered: cfg.is_registered(),
            stream_id_text: cfg
                .stream_id
                .clone()
                .unwrap_or_else(|| "Not registered".into()),
            player_user_set: cfg.player_name.is_some(),
            server_user_set: cfg.server_name.is_some(),
            meter_panel: HWND::default(),
            meter_controls: Vec::new(),
            chk_meter_enabled: HWND::default(),
            chk_meter_locked: HWND::default(),
            edit_meter_max_rows: HWND::default(),
            edit_meter_idle_secs: HWND::default(),
            edit_meter_font_size: HWND::default(),
            edit_meter_x: HWND::default(),
            edit_meter_y: HWND::default(),
            cfg,
            tab_hwnd: HWND::default(),
            trigger_list: HWND::default(),
            btn_add: HWND::default(),
            btn_edit: HWND::default(),
            btn_delete: HWND::default(),
            btn_move_up: HWND::default(),
            btn_move_down: HWND::default(),
            btn_toggle: HWND::default(),
            font_combo: HWND::default(),
            edit_start_font_size: HWND::default(),
            edit_max_font_size: HWND::default(),
            edit_alpha: HWND::default(),
            edit_fly_ms: HWND::default(),
            edit_hold_secs: HWND::default(),
            chk_overlay_enabled: HWND::default(),
            edit_overlay_x: HWND::default(),
            edit_overlay_y: HWND::default(),
            chk_overlay_locked: HWND::default(),
            edit_hist_font_size: HWND::default(),
            edit_hist_idle: HWND::default(),
            edit_hist_max_entries: HWND::default(),
            edit_hist_width: HWND::default(),
            edit_hist_x: HWND::default(),
            edit_hist_y: HWND::default(),
            chk_hist_locked: HWND::default(),
            chk_hist_enabled: HWND::default(),
            triggers_panel: HWND::default(),
            appearance_panel: HWND::default(),
            appearance_controls: Vec::new(),
            windows_panel: HWND::default(),
            windows_controls: Vec::new(),
            sounds_panel: HWND::default(),
            sounds_controls: Vec::new(),
            combo_sound_pkg: HWND::default(),
            sound_label_list: HWND::default(),
            btn_sound_label_edit: HWND::default(),
            btn_sound_label_delete: HWND::default(),
            voice_panel: HWND::default(),
            voice_controls: Vec::new(),
            chk_tts_enabled: HWND::default(),
            combo_tts_speed: HWND::default(),
            radio_tts_smart: HWND::default(),
            radio_tts_queue: HWND::default(),
            radio_tts_interrupt: HWND::default(),
            chk_tts_emergency: HWND::default(),
            chk_tts_operational: HWND::default(),
            chk_tts_ambient: HWND::default(),
            combo_tts_voice: HWND::default(),
            voice_names: Vec::new(),
            current_tab: 0,
        });
        let state_ptr = Box::into_raw(state);

        unsafe {
            let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let hinstance = HINSTANCE(hmodule.0);
            let class_w: Vec<u16> = CLASS_OVERLAY_CFG.encode_utf16().collect();

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(config_wnd_proc),
                hInstance: hinstance,
                lpszClassName: PCWSTR(class_w.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 as usize + 1) as *mut c_void),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let w = 470i32;
            // create_controls lays out with a 470x520px client area; add ~50px for
            // the title bar + WS_EX_DLGMODALFRAME borders so nothing is clipped.
            let h = 570i32;
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            let x = (sw - w) / 2;
            let y = (sh - h) / 2;

            let title = wide("froklog Settings");
            let hwnd = CreateWindowExW(
                WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW,
                PCWSTR(class_w.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
                x,
                y,
                w,
                h,
                None,
                None,
                hinstance,
                Some(state_ptr as *const c_void),
            )
            .expect("CreateWindowExW settings");

            handle
                .settings_hwnd
                .store(hwnd.0 as isize, Ordering::Relaxed);
            switch_tab(&mut *state_ptr, initial_tab);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if !IsDialogMessageW(hwnd, &msg).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        unsafe { windows::Win32::System::Com::CoUninitialize() };
        handle.settings_hwnd.store(0, Ordering::Relaxed);
        handle.settings_open.store(false, Ordering::Relaxed);
    }

    // ── Window procedure ──────────────────────────────────────────────────────

    unsafe extern "system" fn config_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut ConfigState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let state = &mut *ptr;
            create_controls(hwnd, state);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ConfigState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_COMMAND => {
                handle_command(hwnd, state, wparam, lparam);
                LRESULT(0)
            }

            WM_NOTIFY => {
                let nmhdr = &*(lparam.0 as *const windows::Win32::UI::Controls::NMHDR);
                if nmhdr.idFrom as i32 == IDC_TAB
                    && nmhdr.code == windows::Win32::UI::Controls::TCN_SELCHANGE
                {
                    let tab =
                        SendMessageW(state.tab_hwnd, TCM_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                    switch_tab(state, tab);
                }
                LRESULT(0)
            }

            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }

            WM_DESTROY => {
                // End any in-progress "Show All Windows" positioning session —
                // the HUD windows return to their normal enabled/idle-driven
                // visibility once this dialog is gone.
                state
                    .handle
                    .force_show_windows
                    .store(false, Ordering::Relaxed);
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }

            WM_SWITCH_TAB => {
                let tab = wparam.0 as i32;
                switch_tab(state, tab);
                SendMessageW(
                    state.tab_hwnd,
                    TCM_SETCURSEL,
                    WPARAM(tab as usize),
                    LPARAM(0),
                );
                let _ = SetForegroundWindow(hwnd);
                LRESULT(0)
            }

            WM_BRING_TO_FRONT => {
                let _ = SetForegroundWindow(hwnd);
                LRESULT(0)
            }

            WM_URL_TEST_DONE => {
                let result = *Box::from_raw(lparam.0 as *mut UrlTestResult);
                let text = match result {
                    UrlTestResult::Connected {
                        requires_password: false,
                    } => "Connected — open registration".to_string(),
                    UrlTestResult::Connected {
                        requires_password: true,
                    } => "Connected — password required".to_string(),
                    UrlTestResult::Failed(e) => format!("Could not reach server: {e}"),
                };
                set_wnd_text(state.lbl_url_status, &text);
                let _ = EnableWindow(state.edit_url, BOOL(1));
                let _ = EnableWindow(GetDlgItem(hwnd, IDC_URL_TEST).unwrap_or_default(), BOOL(1));
                LRESULT(0)
            }

            WM_SPELL_ICONS_DONE => {
                let result =
                    *Box::from_raw(lparam.0 as *mut crate::spell_icons::spell_icons::ExtractResult);
                let status = format!(
                    "{} extracted, {} duplicates skipped",
                    result.extracted, result.duplicates_skipped,
                );
                set_wnd_text(state.lbl_import_spell_icons_status, &status);
                let _ = EnableWindow(
                    GetDlgItem(hwnd, IDC_IMPORT_SPELL_ICONS).unwrap_or_default(),
                    BOOL(1),
                );

                let mut detail = format!(
                    "Searched: {}{}\n\n\
                     Extracted {} new icon(s) from {} sheet(s) into the icons folder.\n\n\
                     Sheets found: {}\nSheets missing: {}\nCells scanned: {}\nBlank cells skipped: {}\nDuplicate icons skipped: {}",
                    result.searched_dir.display(),
                    if result.searched_dir_exists {
                        ""
                    } else {
                        "  [does not exist]"
                    },
                    result.extracted,
                    result.sheets_found.len(),
                    if result.sheets_found.is_empty() {
                        "(none)".to_string()
                    } else {
                        result.sheets_found.join(", ")
                    },
                    if result.sheets_missing.is_empty() {
                        "(none)".to_string()
                    } else {
                        result.sheets_missing.join(", ")
                    },
                    result.cells_scanned,
                    result.blank_skipped,
                    result.duplicates_skipped,
                );
                if !result.dir_listing.is_empty() {
                    detail.push_str(&format!(
                        "\n\nNo Spells0N.tga sheets matched. Actually found in that folder:\n{}{}",
                        result.dir_listing.join(", "),
                        if result.dir_listing.len() >= 20 {
                            ", …"
                        } else {
                            ""
                        },
                    ));
                }
                if !result.errors.is_empty() {
                    detail.push_str(&format!("\n\nErrors:\n{}", result.errors.join("\n")));
                }
                if result.extracted > 0 {
                    detail.push_str(
                        "\n\nNew icons show up in the icon picker next time you add or edit a trigger action.",
                    );
                }
                msgbox(
                    hwnd,
                    "Import Spell Icons",
                    &detail,
                    MB_ICONINFORMATION | MB_OK,
                );
                LRESULT(0)
            }

            WM_REGISTER_DONE => {
                let result = *Box::from_raw(lparam.0 as *mut RegisterResult);
                match result {
                    RegisterResult::Ok {
                        stream_id,
                        stream_token,
                        view_token,
                    } => {
                        state.is_registered = true;
                        state.stream_id_text = stream_id.clone();
                        set_wnd_text(state.lbl_streamid, &stream_id);
                        set_wnd_text(state.btn_register, "Unregister");
                        let _ = EnableWindow(state.btn_copy_streamid, BOOL(1));
                        let mut cfg = state.handle.config.lock().unwrap();
                        cfg.stream_id = Some(stream_id);
                        cfg.stream_token = Some(stream_token);
                        cfg.view_token = Some(view_token);
                        // Also commit server_url and log_path so is_ready() becomes
                        // true immediately — without this the engine never starts if
                        // the user hasn't clicked Save yet.
                        let url = get_text(state.edit_url);
                        if !url.is_empty() {
                            cfg.server_url = Some(url);
                        }
                        let log = get_text(state.edit_logfile);
                        if !log.is_empty() {
                            cfg.log_path = Some(log);
                        }
                        cfg.save();
                        state.handle.restart.store(true, Ordering::Relaxed);
                    }
                    RegisterResult::Err(e) => {
                        msgbox(hwnd, "Registration failed", &e, MB_ICONERROR | MB_OK);
                    }
                }
                let _ = EnableWindow(state.btn_register, BOOL(1));
                LRESULT(0)
            }

            WM_MOUSEWHEEL => {
                let x = lparam.0 as i16 as i32;
                let y = (lparam.0 >> 16) as i16 as i32;
                let target = WindowFromPoint(POINT { x, y });
                if !target.0.is_null() && target != hwnd {
                    SendMessageW(target, msg, wparam, lparam)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }

            WM_HSCROLL => {
                let target = HWND(lparam.0 as *mut c_void);
                if target == state.edit_sound_volume {
                    let pos =
                        SendMessageW(state.edit_sound_volume, TBM_GETPOS, WPARAM(0), LPARAM(0)).0;
                    set_wnd_text(state.lbl_sound_volume_value, &format!("{pos}%"));
                }
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    // ── Command handler ───────────────────────────────────────────────────────

    unsafe fn handle_command(hwnd: HWND, state: &mut ConfigState, wparam: WPARAM, _lparam: LPARAM) {
        let id = (wparam.0 & 0xFFFF) as i32;
        let notif = (wparam.0 >> 16) & 0xFFFF;

        // Logging tab: live-validate as the user types/selects.
        if id == IDC_PLAYER_EDIT && notif == EN_CHANGE {
            state.player_user_set = true;
            refresh_register_btn(state);
        }
        if id == IDC_URL_EDIT && notif == EN_CHANGE {
            refresh_register_btn(state);
        }
        if id == IDC_SERVER_COMBO && notif == CBN_SELCHANGE {
            state.server_user_set = true;
        }

        match id {
            IDC_COPY_STREAMID => {
                copy_to_clipboard(&state.stream_id_text);
            }

            IDC_LOGFILE_BROWSE => {
                if let Some(path) = pick_log_file() {
                    set_wnd_text(state.edit_logfile, &path);
                    state.draft_log_path = path.clone();
                    if !state.player_user_set {
                        if let Some(p) = player_from_path(&path) {
                            set_wnd_text(state.edit_player, &p);
                        }
                    }
                    if !state.server_user_set {
                        if let Some(srv) = server_from_path(&path) {
                            let idx = combo_find(state.combo_server, &srv);
                            if idx >= 0 {
                                SendMessageW(
                                    state.combo_server,
                                    CB_SETCURSEL,
                                    WPARAM(idx as usize),
                                    LPARAM(0),
                                );
                            }
                        }
                    }
                }
            }

            IDC_URL_TEST => {
                let url = get_text(state.edit_url);
                if url.is_empty() {
                    set_wnd_text(state.lbl_url_status, "Enter a server URL first.");
                    return;
                }
                set_wnd_text(state.lbl_url_status, "Testing…");
                let _ = EnableWindow(state.edit_url, BOOL(0));
                let _ = EnableWindow(GetDlgItem(hwnd, IDC_URL_TEST).unwrap_or_default(), BOOL(0));
                let hwnd_usize = hwnd.0 as usize;
                std::thread::spawn(move || unsafe {
                    let result = test_url(&url);
                    let ptr = Box::into_raw(Box::new(result));
                    let _ = PostMessageW(
                        HWND(hwnd_usize as *mut c_void),
                        WM_URL_TEST_DONE,
                        WPARAM(0),
                        LPARAM(ptr as isize),
                    );
                });
            }

            IDC_IMPORT_SPELL_ICONS => {
                let log_path = get_text(state.edit_logfile);
                let eq_dir = if log_path.is_empty() {
                    None
                } else {
                    crate::spell_icons::spell_icons::eq_dir_from_log_path(&log_path)
                };
                let Some(eq_dir) = eq_dir else {
                    set_wnd_text(
                        state.lbl_import_spell_icons_status,
                        "Set a log file first (needs DIR\\Logs\\... to find DIR\\uifiles\\default\\).",
                    );
                    return;
                };
                set_wnd_text(state.lbl_import_spell_icons_status, "Extracting…");
                let _ = EnableWindow(
                    GetDlgItem(hwnd, IDC_IMPORT_SPELL_ICONS).unwrap_or_default(),
                    BOOL(0),
                );
                let hwnd_usize = hwnd.0 as usize;
                std::thread::spawn(move || {
                    let icons_dir = crate::assets::icons_dir();
                    let result = crate::spell_icons::spell_icons::extract_spell_icons(
                        &eq_dir,
                        &icons_dir,
                        crate::spell_icons::spell_icons::DEFAULT_CELL_SIZE,
                    );
                    let ptr = Box::into_raw(Box::new(result));
                    unsafe {
                        let _ = PostMessageW(
                            HWND(hwnd_usize as *mut c_void),
                            WM_SPELL_ICONS_DONE,
                            WPARAM(0),
                            LPARAM(ptr as isize),
                        );
                    }
                });
            }

            IDC_REGISTER_BTN => {
                if state.is_registered {
                    if msgbox(
                        hwnd,
                        "Unregister",
                        "Clear stream credentials? The viewer URL will stop working.",
                        MB_ICONWARNING | MB_YESNO,
                    ) == 6
                    {
                        // IDYES = 6
                        let mut cfg = state.handle.config.lock().unwrap();
                        cfg.stream_id = None;
                        cfg.stream_token = None;
                        cfg.view_token = None;
                        cfg.save();
                        drop(cfg);
                        state.handle.restart.store(true, Ordering::Relaxed);
                        state.is_registered = false;
                        state.stream_id_text = String::new();
                        set_wnd_text(state.lbl_streamid, "Not registered");
                        set_wnd_text(state.btn_register, "Register");
                        let _ = EnableWindow(state.btn_copy_streamid, BOOL(0));
                    }
                } else {
                    let url = get_text(state.edit_url);
                    let player = get_text(state.edit_player);
                    let server = combo_text(state.combo_server);
                    let game = label_to_game_id(&combo_text(state.combo_game)).to_string();
                    if url.is_empty() || player.is_empty() {
                        msgbox(
                            hwnd,
                            "Register",
                            "Enter a server URL (and test it) and a player name.",
                            MB_ICONWARNING | MB_OK,
                        );
                        return;
                    }
                    let _ = EnableWindow(state.btn_register, BOOL(0));
                    let password = get_text(state.edit_password);
                    let is_public =
                        SendMessageW(state.chk_public, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
                            as usize
                            == BST_CHECKED;
                    let hwnd_usize = hwnd.0 as usize;
                    std::thread::spawn(move || unsafe {
                        let result =
                            do_register(&url, &player, &server, &game, &password, is_public);
                        let ptr = Box::into_raw(Box::new(result));
                        let _ = PostMessageW(
                            HWND(hwnd_usize as *mut c_void),
                            WM_REGISTER_DONE,
                            WPARAM(0),
                            LPARAM(ptr as isize),
                        );
                    });
                }
            }

            IDC_METER_RESET_POS => {
                {
                    let mut cfg = state.handle.config.lock().unwrap();
                    cfg.meter_x = -1;
                    cfg.meter_y = -1;
                    cfg.save();
                }
                // Keep the dialog's own X/Y fields in sync so a later Save
                // doesn't write the stale pre-reset values back over this.
                set_wnd_text(state.edit_meter_x, "-1");
                set_wnd_text(state.edit_meter_y, "-1");
            }

            IDC_OVERLAY_RESET_POS => {
                {
                    let mut cfg = state.handle.config.lock().unwrap();
                    cfg.overlay_x = -1;
                    cfg.overlay_y = -1;
                    cfg.save();
                }
                set_wnd_text(state.edit_overlay_x, "-1");
                set_wnd_text(state.edit_overlay_y, "-1");
            }

            IDC_HIST_RESET_POS => {
                {
                    let mut cfg = state.handle.config.lock().unwrap();
                    cfg.overlay_history_x = -1;
                    cfg.overlay_history_y = -1;
                    cfg.save();
                }
                set_wnd_text(state.edit_hist_x, "-1");
                set_wnd_text(state.edit_hist_y, "-1");
            }

            IDC_SHOW_ALL_WINDOWS => {
                {
                    let mut cfg = state.handle.config.lock().unwrap();
                    cfg.overlay_locked = false;
                    cfg.overlay_history_locked = false;
                    cfg.meter_locked = false;
                    cfg.save();
                }
                // Reflect the unlock in the dialog's own checkboxes so a
                // later Save doesn't write the stale locked state back.
                for chk in [
                    state.chk_overlay_locked,
                    state.chk_hist_locked,
                    state.chk_meter_locked,
                ] {
                    SendMessageW(chk, BM_SETCHECK, WPARAM(0), LPARAM(0));
                }
                // Forces the three HUD windows to render a draggable
                // placeholder even with nothing real to show; cleared when
                // this dialog closes (see WM_DESTROY below).
                state
                    .handle
                    .force_show_windows
                    .store(true, Ordering::Relaxed);
            }

            IDC_TRIGGER_LIST if notif == LBN_DBLCLK => {
                edit_selected_trigger(hwnd, state);
            }
            IDC_TRIGGER_LIST if notif == LBN_SELCHANGE => {
                refresh_trigger_buttons(state);
            }

            IDC_BTN_ADD => {
                let new_def = TriggerDef {
                    name: "New Trigger".to_string(),
                    enabled: true,
                    condition_logic: ConditionLogic::All,
                    conditions: Vec::new(),
                    actions: Vec::new(),
                };
                let idx = state.triggers.triggers.len();
                state.triggers.triggers.push(new_def);
                rebuild_trigger_list(state);
                SendMessageW(state.trigger_list, LB_SETCURSEL, WPARAM(idx), LPARAM(0));
                refresh_trigger_buttons(state);
                edit_selected_trigger(hwnd, state);
            }

            IDC_BTN_EDIT => {
                edit_selected_trigger(hwnd, state);
            }

            IDC_BTN_DELETE => {
                let sel =
                    SendMessageW(state.trigger_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel < 0 || sel as usize >= state.triggers.triggers.len() {
                    return;
                }
                let name = state.triggers.triggers[sel as usize].name.clone();
                let prompt = format!("Delete trigger \"{}\"?", name);
                if msgbox(hwnd, "Delete Trigger", &prompt, MB_ICONWARNING | MB_YESNO) == 6 {
                    state.triggers.triggers.remove(sel as usize);
                    rebuild_trigger_list(state);
                    refresh_trigger_buttons(state);
                }
            }

            IDC_BTN_MOVE_UP => {
                let sel =
                    SendMessageW(state.trigger_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel <= 0 || sel as usize >= state.triggers.triggers.len() {
                    return;
                }
                state.triggers.triggers.swap(sel as usize, sel as usize - 1);
                rebuild_trigger_list(state);
                SendMessageW(
                    state.trigger_list,
                    LB_SETCURSEL,
                    WPARAM((sel - 1) as usize),
                    LPARAM(0),
                );
                refresh_trigger_buttons(state);
            }

            IDC_BTN_MOVE_DOWN => {
                let sel =
                    SendMessageW(state.trigger_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                let n = state.triggers.triggers.len();
                if sel < 0 || sel as usize + 1 >= n {
                    return;
                }
                state.triggers.triggers.swap(sel as usize, sel as usize + 1);
                rebuild_trigger_list(state);
                SendMessageW(
                    state.trigger_list,
                    LB_SETCURSEL,
                    WPARAM((sel + 1) as usize),
                    LPARAM(0),
                );
                refresh_trigger_buttons(state);
            }

            IDC_BTN_TOGGLE => {
                let sel =
                    SendMessageW(state.trigger_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel < 0 || sel as usize >= state.triggers.triggers.len() {
                    return;
                }
                let t = &mut state.triggers.triggers[sel as usize];
                t.enabled = !t.enabled;
                rebuild_trigger_list(state);
                SendMessageW(
                    state.trigger_list,
                    LB_SETCURSEL,
                    WPARAM(sel as usize),
                    LPARAM(0),
                );
                refresh_trigger_buttons(state);
            }

            IDC_SOUND_PKG_COMBO if notif == CBN_SELCHANGE => {
                rebuild_sound_label_list(state);
                refresh_sound_label_buttons(state);
            }

            IDC_SOUND_LABEL_LIST if notif == LBN_DBLCLK => {
                edit_selected_sound_label(hwnd, state);
            }
            IDC_SOUND_LABEL_LIST if notif == LBN_SELCHANGE => {
                refresh_sound_label_buttons(state);
            }

            IDC_SOUND_LABEL_ADD => {
                let pkg = current_sound_package(state);
                if let Some((name, path)) = open_sound_label_editor(hwnd, None, None) {
                    if let Err(e) = crate::sound_packages::sound_packages::add_or_replace_label(
                        &pkg,
                        &name,
                        std::path::Path::new(&path),
                    ) {
                        msgbox(
                            hwnd,
                            "Add Sound Label",
                            &format!("Could not add label: {e}"),
                            MB_ICONERROR | MB_OK,
                        );
                    } else {
                        rebuild_sound_label_list(state);
                        refresh_sound_label_buttons(state);
                    }
                }
            }

            IDC_SOUND_LABEL_EDIT => {
                edit_selected_sound_label(hwnd, state);
            }

            IDC_SOUND_LABEL_DELETE => {
                let pkg = current_sound_package(state);
                let Some(name) = selected_sound_label_name(state) else {
                    return;
                };
                let prompt = format!("Delete sound label \"{name}\"?");
                if msgbox(
                    hwnd,
                    "Delete Sound Label",
                    &prompt,
                    MB_ICONWARNING | MB_YESNO,
                ) == 6
                {
                    crate::sound_packages::sound_packages::delete_label(&pkg, &name);
                    rebuild_sound_label_list(state);
                    refresh_sound_label_buttons(state);
                }
            }

            IDC_SOUND_PKG_NEW => {
                let active = current_sound_package(state);
                let suggested = format!("{active} copy");
                if let Some(name) =
                    prompt_text_dialog(hwnd, "New Package", "Package name:", &suggested)
                {
                    let name = name.trim();
                    if name.is_empty() {
                        return;
                    }
                    let unique = crate::sound_packages::sound_packages::unique_package_name(name);
                    match crate::sound_packages::sound_packages::clone_package(&active, &unique) {
                        Ok(()) => select_sound_package(state, &unique),
                        Err(e) => {
                            msgbox(
                                hwnd,
                                "New Package",
                                &format!("Could not create package: {e}"),
                                MB_ICONERROR | MB_OK,
                            );
                        }
                    };
                }
            }

            IDC_SOUND_PKG_RENAME => {
                let active = current_sound_package(state);
                if active == crate::sound_packages::sound_packages::DEFAULT_PACKAGE {
                    msgbox(
                        hwnd,
                        "Rename Package",
                        "The default package cannot be renamed.",
                        MB_ICONWARNING | MB_OK,
                    );
                    return;
                }
                if let Some(new_name) =
                    prompt_text_dialog(hwnd, "Rename Package", "New name:", &active)
                {
                    let new_name = new_name.trim();
                    if new_name.is_empty() || new_name == active {
                        return;
                    }
                    let unique =
                        crate::sound_packages::sound_packages::unique_package_name(new_name);
                    match crate::sound_packages::sound_packages::rename_package(&active, &unique) {
                        Ok(()) => {
                            if state.cfg.sound_package == active {
                                state.cfg.sound_package = unique.clone();
                            }
                            select_sound_package(state, &unique);
                        }
                        Err(e) => {
                            msgbox(
                                hwnd,
                                "Rename Package",
                                &format!("Could not rename package: {e}"),
                                MB_ICONERROR | MB_OK,
                            );
                        }
                    };
                }
            }

            IDC_SOUND_PKG_DELETE => {
                let active = current_sound_package(state);
                if active == crate::sound_packages::sound_packages::DEFAULT_PACKAGE {
                    msgbox(
                        hwnd,
                        "Delete Package",
                        "The default package cannot be deleted.",
                        MB_ICONWARNING | MB_OK,
                    );
                    return;
                }
                if active == state.cfg.sound_package {
                    msgbox(
                        hwnd,
                        "Delete Package",
                        "This package is currently active. Switch the active package to \
                         something else (and Save) before deleting it.",
                        MB_ICONWARNING | MB_OK,
                    );
                    return;
                }
                let prompt = format!(
                    "Delete package \"{active}\" and all its sounds? This cannot be undone."
                );
                if msgbox(hwnd, "Delete Package", &prompt, MB_ICONWARNING | MB_YESNO) == 6 {
                    match crate::sound_packages::sound_packages::delete_package(&active) {
                        Ok(()) => {
                            refresh_sound_packages_combo(state);
                            rebuild_sound_label_list(state);
                            refresh_sound_label_buttons(state);
                        }
                        Err(e) => {
                            msgbox(
                                hwnd,
                                "Delete Package",
                                &format!("Could not delete package: {e}"),
                                MB_ICONERROR | MB_OK,
                            );
                        }
                    };
                }
            }

            IDC_SOUND_PKG_EXPORT => {
                let active = current_sound_package(state);
                if let Some(path) = pick_save_zip_file(&active) {
                    match crate::sound_packages::sound_packages::export_package_zip(
                        &active,
                        std::path::Path::new(&path),
                    ) {
                        Ok(()) => {
                            msgbox(
                                hwnd,
                                "Export Package",
                                &format!("Exported \"{active}\" to:\n{path}"),
                                MB_ICONINFORMATION | MB_OK,
                            );
                        }
                        Err(e) => {
                            msgbox(
                                hwnd,
                                "Export Package",
                                &format!("Export failed: {e}"),
                                MB_ICONERROR | MB_OK,
                            );
                        }
                    }
                }
            }

            IDC_SOUND_PKG_IMPORT => {
                if let Some(path) = pick_zip_file() {
                    match crate::sound_packages::sound_packages::import_package_zip(
                        std::path::Path::new(&path),
                    ) {
                        Ok(name) => {
                            select_sound_package(state, &name);
                            msgbox(
                                hwnd,
                                "Import Package",
                                &format!("Imported package \"{name}\"."),
                                MB_ICONINFORMATION | MB_OK,
                            );
                        }
                        Err(e) => {
                            msgbox(
                                hwnd,
                                "Import Package",
                                &format!("Import failed: {e}"),
                                MB_ICONERROR | MB_OK,
                            );
                        }
                    }
                }
            }

            IDC_SAVE => {
                save_and_close(hwnd, state);
            }

            IDC_CANCEL => {
                let _ = DestroyWindow(hwnd);
            }

            _ => {}
        }
    }

    // ── Save ──────────────────────────────────────────────────────────────────

    unsafe fn save_and_close(hwnd: HWND, state: &mut ConfigState) {
        // ── Logging tab: validate before touching anything ─────────────────
        let player = get_text(state.edit_player);
        if !player.is_empty() && !player.chars().all(|c| c.is_ascii_alphabetic()) {
            msgbox(
                hwnd,
                "Validation error",
                "Player name may only contain letters A–Z.",
                MB_ICONWARNING | MB_OK,
            );
            return;
        }
        let log_path = get_text(state.edit_logfile);
        let server_url = get_text(state.edit_url);
        let password = get_text(state.edit_password);
        let game_label = combo_text(state.combo_game);
        let game = label_to_game_id(&game_label).to_string();
        let server = combo_text(state.combo_server);
        let public = SendMessageW(state.chk_public, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
            == BST_CHECKED;
        let remote_logging_enabled =
            SendMessageW(state.chk_remote_logging, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED;

        // ── General tab ──────────────────────────────────────────────────────
        let sound_enabled = SendMessageW(state.chk_sound_enabled, BM_GETCHECK, WPARAM(0), LPARAM(0))
            .0 as usize
            == BST_CHECKED;
        let sound_volume =
            SendMessageW(state.edit_sound_volume, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as u8;
        let sound_package_sel = combo_text(state.combo_sound_pkg);
        let sound_package = if sound_package_sel.is_empty() {
            crate::sound_packages::sound_packages::DEFAULT_PACKAGE.to_string()
        } else {
            sound_package_sel
        };

        // ── DPS Meter tab ───────────────────────────────────────────────────
        let meter_enabled = SendMessageW(state.chk_meter_enabled, BM_GETCHECK, WPARAM(0), LPARAM(0))
            .0 as usize
            == BST_CHECKED;
        let meter_locked = SendMessageW(state.chk_meter_locked, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
            as usize
            == BST_CHECKED;
        let meter_max_rows: usize = get_text(state.edit_meter_max_rows)
            .parse()
            .unwrap_or(12)
            .clamp(1, 30);
        let meter_idle_secs: u32 = get_text(state.edit_meter_idle_secs).parse().unwrap_or(10);
        let meter_font_size: u32 = get_text(state.edit_meter_font_size)
            .parse()
            .unwrap_or(11)
            .clamp(8, 32);
        let meter_x: i32 = get_text(state.edit_meter_x).parse().unwrap_or(-1);
        let meter_y: i32 = get_text(state.edit_meter_y).parse().unwrap_or(-1);

        let font_idx =
            SendMessageW(state.font_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
        let font_name = if font_idx < FONT_NAMES.len() {
            FONT_NAMES[font_idx].to_string()
        } else {
            "Segoe UI".to_string()
        };

        let start_font_size: u32 = get_text(state.edit_start_font_size)
            .parse()
            .unwrap_or(10)
            .clamp(6, 72);
        let max_font_size: u32 = get_text(state.edit_max_font_size)
            .parse()
            .unwrap_or(60)
            .clamp(6, 300);
        let fly_ms: u32 = get_text(state.edit_fly_ms).parse().unwrap_or(240).max(16);
        let hold_secs: f32 = get_text(state.edit_hold_secs)
            .parse::<f32>()
            .unwrap_or(2.5)
            .max(0.0);
        let alpha: u8 = get_text(state.edit_alpha)
            .parse::<u32>()
            .unwrap_or(200)
            .min(255) as u8;
        let overlay_enabled =
            SendMessageW(state.chk_overlay_enabled, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED;

        let overlay_x: i32 = get_text(state.edit_overlay_x).parse().unwrap_or(-1);
        let overlay_y: i32 = get_text(state.edit_overlay_y).parse().unwrap_or(-1);
        let overlay_locked =
            SendMessageW(state.chk_overlay_locked, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED;

        let history_font_size: u32 = get_text(state.edit_hist_font_size)
            .parse()
            .unwrap_or(12)
            .clamp(8, 72);
        let history_idle_secs: u32 = get_text(state.edit_hist_idle).parse().unwrap_or(8);
        let history_max_entries: usize = get_text(state.edit_hist_max_entries)
            .parse()
            .unwrap_or(8)
            .clamp(1, 50);
        let history_width: i32 = get_text(state.edit_hist_width)
            .parse()
            .unwrap_or(320)
            .max(160);
        let history_x: i32 = get_text(state.edit_hist_x).parse().unwrap_or(-1);
        let history_y: i32 = get_text(state.edit_hist_y).parse().unwrap_or(-1);
        let history_locked = SendMessageW(state.chk_hist_locked, BM_GETCHECK, WPARAM(0), LPARAM(0))
            .0 as usize
            == BST_CHECKED;
        let history_enabled =
            SendMessageW(state.chk_hist_enabled, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED;

        let tts_enabled = SendMessageW(state.chk_tts_enabled, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
            as usize
            == BST_CHECKED;

        let tts_speed_idx =
            SendMessageW(state.combo_tts_speed, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
        let tts_speed = match tts_speed_idx {
            0 => TtsSpeed::Normal,
            1 => TtsSpeed::Fast,
            2 => TtsSpeed::Faster,
            _ => TtsSpeed::Fastest,
        };

        let tts_audio_mode =
            if SendMessageW(state.radio_tts_smart, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED
            {
                TtsAudioMode::SmartPriority
            } else if SendMessageW(state.radio_tts_queue, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
                as usize
                == BST_CHECKED
            {
                TtsAudioMode::QueueAll
            } else {
                TtsAudioMode::InterruptConstantly
            };

        let tts_read_emergency =
            SendMessageW(state.chk_tts_emergency, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED;
        let tts_read_operational =
            SendMessageW(state.chk_tts_operational, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED;
        let tts_read_ambient =
            SendMessageW(state.chk_tts_ambient, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED;

        let tts_voice_idx =
            SendMessageW(state.combo_tts_voice, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
        let tts_voice = state
            .voice_names
            .get(tts_voice_idx)
            .map(|(_, id)| id.clone())
            .unwrap_or_default();

        let patch_creds = {
            let mut cfg = state.handle.config.lock().unwrap();

            // Snapshot registration credentials before mutation so we can PATCH
            // the server if public_stream changed without re-registering.
            let old_public = cfg.public_stream;
            let patch_creds = if cfg.is_registered() && old_public != public {
                cfg.stream_id
                    .as_ref()
                    .zip(cfg.stream_token.as_ref())
                    .zip(cfg.server_url.as_ref())
                    .map(|((id, tok), url)| (url.clone(), id.clone(), tok.clone()))
            } else {
                None
            };

            cfg.log_path = if log_path.is_empty() {
                None
            } else {
                Some(log_path)
            };
            cfg.server_url = if server_url.is_empty() {
                None
            } else {
                Some(server_url)
            };
            cfg.player_name = if player.is_empty() {
                None
            } else {
                Some(player)
            };
            cfg.server_name = Some(server);
            cfg.game = Some(game);
            cfg.stream_password = if password.is_empty() {
                None
            } else {
                Some(password)
            };
            cfg.public_stream = public;
            cfg.remote_logging_enabled = remote_logging_enabled;
            cfg.sound_enabled = sound_enabled;
            cfg.sound_volume = sound_volume;
            cfg.sound_package = sound_package.clone();
            crate::overlay::overlay::set_sound_enabled(sound_enabled);
            crate::overlay::overlay::set_sound_volume_percent(sound_volume);
            crate::overlay::overlay::set_active_sound_package(&sound_package);

            cfg.meter_enabled = meter_enabled;
            cfg.meter_locked = meter_locked;
            cfg.meter_max_rows = meter_max_rows;
            cfg.meter_idle_secs = meter_idle_secs;
            cfg.meter_font_size = meter_font_size;
            cfg.meter_x = meter_x;
            cfg.meter_y = meter_y;

            cfg.overlay_font = font_name;
            cfg.overlay_start_font_size = start_font_size;
            cfg.overlay_max_font_size = max_font_size;
            cfg.overlay_fly_ms = fly_ms;
            cfg.overlay_hold_secs = hold_secs;
            cfg.overlay_alpha = alpha;
            cfg.overlay_enabled = overlay_enabled;
            cfg.overlay_x = overlay_x;
            cfg.overlay_y = overlay_y;
            cfg.overlay_locked = overlay_locked;
            cfg.overlay_history_font_size = history_font_size;
            cfg.overlay_history_idle_secs = history_idle_secs;
            cfg.overlay_history_max_entries = history_max_entries;
            cfg.overlay_history_width = history_width;
            cfg.overlay_history_x = history_x;
            cfg.overlay_history_y = history_y;
            cfg.overlay_history_locked = history_locked;
            cfg.overlay_history_enabled = history_enabled;
            cfg.tts_enabled = tts_enabled;
            cfg.tts_speed = tts_speed;
            cfg.tts_audio_mode = tts_audio_mode;
            cfg.tts_read_emergency = tts_read_emergency;
            cfg.tts_read_operational = tts_read_operational;
            cfg.tts_read_ambient = tts_read_ambient;
            cfg.tts_voice = tts_voice;
            cfg.save();

            patch_creds
        };

        if let Some((url, id, tok)) = patch_creds {
            std::thread::spawn(move || {
                patch_public_stream(&url, &id, &tok, public);
            });
        }

        state.triggers.save();

        if let Some(engine) = state.handle.trigger_engine.lock().unwrap().as_ref() {
            engine.reload(&state.triggers);
        }

        state.handle.restart.store(true, Ordering::Relaxed);

        let _ = DestroyWindow(hwnd);
    }

    // ── Tab switching ─────────────────────────────────────────────────────────

    // Tab indices: 0=General 1=Logging 2=Triggers 3=Overlays 4=DPS Meter 5=Voice 6=Windows 7=Sounds
    unsafe fn switch_tab(state: &mut ConfigState, tab: i32) {
        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOW};
        state.current_tab = tab;

        let show_hide = |show: bool| if show { SW_SHOW } else { SW_HIDE };

        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.general_panel,
            show_hide(tab == 0),
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.logging_panel,
            show_hide(tab == 1),
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.triggers_panel,
            show_hide(tab == 2),
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.appearance_panel,
            show_hide(tab == 3),
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.meter_panel,
            show_hide(tab == 4),
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.voice_panel,
            show_hide(tab == 5),
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.windows_panel,
            show_hide(tab == 6),
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.sounds_panel,
            show_hide(tab == 7),
        );

        let general_show = show_hide(tab == 0);
        for &h in &state.general_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, general_show);
        }

        let gen_show = show_hide(tab == 1);
        for &h in &state.logging_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, gen_show);
        }

        let tr_show = show_hide(tab == 2);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.trigger_list, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_add, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_edit, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_delete, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_move_up, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_move_down, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_toggle, tr_show);

        let ap_show = show_hide(tab == 3);
        for i in 0..state.appearance_controls.len() {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                state.appearance_controls[i],
                ap_show,
            );
        }

        let meter_show = show_hide(tab == 4);
        for &h in &state.meter_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, meter_show);
        }

        let vp_show = show_hide(tab == 5);
        for i in 0..state.voice_controls.len() {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                state.voice_controls[i],
                vp_show,
            );
        }

        let win_show = show_hide(tab == 6);
        for &h in &state.windows_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, win_show);
        }

        let snd_show = show_hide(tab == 7);
        for &h in &state.sounds_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, snd_show);
        }
    }

    // ── Trigger list ──────────────────────────────────────────────────────────

    unsafe fn rebuild_trigger_list(state: &mut ConfigState) {
        SendMessageW(
            state.trigger_list,
            windows::Win32::UI::WindowsAndMessaging::LB_RESETCONTENT,
            WPARAM(0),
            LPARAM(0),
        );
        for t in &state.triggers.triggers {
            let nconds = t.conditions.len();
            let nacts = t.actions.len();
            let logic = match t.condition_logic {
                ConditionLogic::All => "ALL",
                ConditionLogic::Any => "ANY",
            };
            let label = format!(
                "[{}] {}  ({} {} cond, {} act)",
                if t.enabled { "✓" } else { " " },
                t.name,
                logic,
                nconds,
                nacts,
            );
            let lw = wide(&label);
            SendMessageW(
                state.trigger_list,
                LB_ADDSTRING,
                WPARAM(0),
                LPARAM(lw.as_ptr() as isize),
            );
        }
    }

    unsafe fn refresh_trigger_buttons(state: &ConfigState) {
        let sel = SendMessageW(state.trigger_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
        let n = state.triggers.triggers.len() as i32;
        let has_sel = sel >= 0 && sel < n;
        let _ = EnableWindow(state.btn_edit, BOOL(if has_sel { 1 } else { 0 }));
        let _ = EnableWindow(state.btn_delete, BOOL(if has_sel { 1 } else { 0 }));
        let _ = EnableWindow(state.btn_toggle, BOOL(if has_sel { 1 } else { 0 }));
        let _ = EnableWindow(
            state.btn_move_up,
            BOOL(if has_sel && sel > 0 { 1 } else { 0 }),
        );
        let _ = EnableWindow(
            state.btn_move_down,
            BOOL(if has_sel && sel < n - 1 { 1 } else { 0 }),
        );
    }

    // ── Sound labels / packages ───────────────────────────────────────────────

    /// The package name the "Active Package" combo currently shows selected
    /// (not necessarily the persisted `Config.sound_package` — that's only
    /// updated on Save), falling back to the default package if nothing is
    /// selected (e.g. the combo hasn't been populated yet).
    unsafe fn current_sound_package(state: &ConfigState) -> String {
        let s = combo_text(state.combo_sound_pkg);
        if s.is_empty() {
            crate::sound_packages::sound_packages::DEFAULT_PACKAGE.to_string()
        } else {
            s
        }
    }

    unsafe fn refresh_sound_packages_combo(state: &ConfigState) {
        let packages = crate::sound_packages::sound_packages::list_packages();
        let prev_selected = combo_text(state.combo_sound_pkg);
        let select = if prev_selected.is_empty() {
            state.cfg.sound_package.clone()
        } else {
            prev_selected
        };
        SendMessageW(state.combo_sound_pkg, CB_RESETCONTENT, WPARAM(0), LPARAM(0));
        for pkg in &packages {
            cb_add(state.combo_sound_pkg, pkg);
        }
        let idx = packages.iter().position(|p| *p == select).unwrap_or(0);
        SendMessageW(state.combo_sound_pkg, CB_SETCURSEL, WPARAM(idx), LPARAM(0));
    }

    unsafe fn rebuild_sound_label_list(state: &ConfigState) {
        SendMessageW(
            state.sound_label_list,
            LB_RESETCONTENT,
            WPARAM(0),
            LPARAM(0),
        );
        let pkg = current_sound_package(state);
        let mut labels = crate::sound_packages::sound_packages::load_manifest(&pkg).labels;
        labels.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in &labels {
            let text = format!("{}  —  {}", entry.name, entry.file);
            let lw = wide(&text);
            SendMessageW(
                state.sound_label_list,
                LB_ADDSTRING,
                WPARAM(0),
                LPARAM(lw.as_ptr() as isize),
            );
        }
    }

    unsafe fn refresh_sound_label_buttons(state: &ConfigState) {
        let sel = SendMessageW(state.sound_label_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
        let has_sel = sel >= 0;
        let _ = EnableWindow(
            state.btn_sound_label_edit,
            BOOL(if has_sel { 1 } else { 0 }),
        );
        let _ = EnableWindow(
            state.btn_sound_label_delete,
            BOOL(if has_sel { 1 } else { 0 }),
        );
    }

    /// Selected row's label name, read back from the listbox text rather than
    /// re-sorting the manifest — keeps this in exact sync with what's shown.
    unsafe fn selected_sound_label_name(state: &ConfigState) -> Option<String> {
        let sel = SendMessageW(state.sound_label_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
        if sel < 0 {
            return None;
        }
        let len = SendMessageW(
            state.sound_label_list,
            LB_GETTEXTLEN,
            WPARAM(sel as usize),
            LPARAM(0),
        )
        .0 as usize;
        let mut buf = vec![0u16; len + 1];
        SendMessageW(
            state.sound_label_list,
            LB_GETTEXT,
            WPARAM(sel as usize),
            LPARAM(buf.as_mut_ptr() as isize),
        );
        let text = String::from_utf16_lossy(&buf[..len]);
        text.split("  —  ").next().map(|s| s.to_string())
    }

    /// Refreshes the package combo, selects `name`, and rebuilds the label
    /// list for it — the common tail of New/Rename/Import package.
    unsafe fn select_sound_package(state: &mut ConfigState, name: &str) {
        refresh_sound_packages_combo(state);
        let idx = combo_find(state.combo_sound_pkg, name);
        if idx >= 0 {
            SendMessageW(
                state.combo_sound_pkg,
                CB_SETCURSEL,
                WPARAM(idx as usize),
                LPARAM(0),
            );
        }
        rebuild_sound_label_list(state);
        refresh_sound_label_buttons(state);
    }

    unsafe fn edit_selected_sound_label(parent: HWND, state: &mut ConfigState) {
        let pkg = current_sound_package(state);
        let Some(old_name) = selected_sound_label_name(state) else {
            return;
        };
        let existing_file = crate::sound_packages::sound_packages::load_manifest(&pkg)
            .labels
            .into_iter()
            .find(|e| e.name == old_name)
            .map(|e| crate::sound_packages::sound_packages::package_dir(&pkg).join(e.file));
        if let Some((new_name, path)) =
            open_sound_label_editor(parent, Some(old_name.clone()), existing_file)
        {
            if new_name != old_name {
                let _ =
                    crate::sound_packages::sound_packages::rename_label(&pkg, &old_name, &new_name);
            }
            if let Err(e) = crate::sound_packages::sound_packages::add_or_replace_label(
                &pkg,
                &new_name,
                std::path::Path::new(&path),
            ) {
                msgbox(
                    parent,
                    "Edit Sound Label",
                    &format!("Could not update label: {e}"),
                    MB_ICONERROR | MB_OK,
                );
            }
            rebuild_sound_label_list(state);
            refresh_sound_label_buttons(state);
        }
    }

    // ── Sound label edit dialog ───────────────────────────────────────────────

    struct SoundLabelEditState {
        initial_name: String,
        initial_file: String,
        result: Option<(String, String)>,
        edit_name: HWND,
        edit_file: HWND,
        btn_test: HWND,
    }

    /// Opens the small "add/edit sound label" dialog. `initial_name`/
    /// `initial_file` seed the fields for an edit; both `None`/empty for a
    /// fresh Add. Returns `(label_name, absolute_sound_file_path)`.
    unsafe fn open_sound_label_editor(
        parent: HWND,
        initial_name: Option<String>,
        initial_file: Option<std::path::PathBuf>,
    ) -> Option<(String, String)> {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hinstance = HINSTANCE(hmodule.0);
        let class_w: Vec<u16> = CLASS_SOUND_LABEL_EDIT.encode_utf16().collect();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(sound_label_edit_proc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_w.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 as usize + 1) as *mut c_void),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let state = Box::new(SoundLabelEditState {
            initial_name: initial_name.unwrap_or_default(),
            initial_file: initial_file
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            result: None,
            edit_name: HWND::default(),
            edit_file: HWND::default(),
            btn_test: HWND::default(),
        });
        let state_ptr = Box::into_raw(state);

        let w = 390i32;
        let h = 190i32;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let title = wide("Sound Label");
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW,
            PCWSTR(class_w.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            parent,
            None,
            hinstance,
            Some(state_ptr as *const c_void),
        )
        .expect("CreateWindowExW sound label edit");

        drain_pending_clicks();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        SOUND_LABEL_EDIT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static SOUND_LABEL_EDIT_RESULT: std::cell::RefCell<Option<(String, String)>> =
            const { std::cell::RefCell::new(None) };
    }

    unsafe extern "system" fn sound_label_edit_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut SoundLabelEditState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let state = &mut *ptr;
            create_sound_label_edit_controls(hwnd, state);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SoundLabelEditState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                handle_sound_label_edit_command(hwnd, state, id);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(result) = state.result.take() {
                    SOUND_LABEL_EDIT_RESULT.with(|cell| {
                        *cell.borrow_mut() = Some(result);
                    });
                }
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn handle_sound_label_edit_command(
        hwnd: HWND,
        state: &mut SoundLabelEditState,
        id: i32,
    ) {
        match id {
            IDC_SLBL_BROWSE => {
                if let Some(path) = pick_sound_file() {
                    set_wnd_text(state.edit_file, &path);
                    if get_text(state.edit_name).trim().is_empty() {
                        set_wnd_text(
                            state.edit_name,
                            &crate::sound_packages::sound_packages::label_from_stem(&path),
                        );
                    }
                    let _ = EnableWindow(state.btn_test, BOOL(1));
                }
            }
            IDC_SLBL_TEST => {
                let path = get_text(state.edit_file);
                if !path.is_empty() {
                    crate::overlay::overlay::preview_sound(&path);
                }
            }
            IDC_SLBL_OK => {
                let name = get_text(state.edit_name).trim().to_string();
                let path = get_text(state.edit_file);
                if name.is_empty() || path.is_empty() {
                    msgbox(
                        hwnd,
                        "Sound Label",
                        "Enter a label name and choose a sound file.",
                        MB_ICONWARNING | MB_OK,
                    );
                    return;
                }
                state.result = Some((name, path));
                let _ = DestroyWindow(hwnd);
            }
            IDC_SLBL_CANCEL => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }

    unsafe fn create_sound_label_edit_controls(hwnd: HWND, state: &mut SoundLabelEditState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        let lx = 10i32;
        let lw = 90i32;
        let cx = lx + lw + 6;
        let ch = 22i32;
        let row = 32i32;
        let right_edge = 360i32;
        let btn_w = 70i32;
        let mut y = 14i32;

        mk_label(hwnd, hi, font, "Label name:", lx, y, lw, ch);
        state.edit_name = mk_edit(
            hwnd,
            hi,
            font,
            &state.initial_name,
            cx,
            y,
            right_edge - cx,
            ch,
            IDC_SLBL_NAME,
            0,
        );
        y += row;

        mk_label(hwnd, hi, font, "Sound file:", lx, y, lw, ch);
        let file_w = right_edge - cx - btn_w - 4;
        state.edit_file = mk_edit(
            hwnd,
            hi,
            font,
            &state.initial_file,
            cx,
            y,
            file_w,
            ch,
            IDC_SLBL_FILE,
            ES_READONLY,
        );
        mk_button_ex(
            hwnd,
            hi,
            font,
            "Browse…",
            cx + file_w + 4,
            y,
            btn_w,
            ch,
            IDC_SLBL_BROWSE,
        );
        y += row;

        state.btn_test = mk_button_ex(
            hwnd,
            hi,
            font,
            "\u{25B6} Preview",
            cx,
            y,
            120,
            ch,
            IDC_SLBL_TEST,
        );
        let _ = EnableWindow(
            state.btn_test,
            BOOL(if state.initial_file.is_empty() { 0 } else { 1 }),
        );
        y += row + 8;

        mk_button_ex(
            hwnd,
            hi,
            font,
            "Cancel",
            right_edge - btn_w,
            y,
            btn_w,
            ch + 2,
            IDC_SLBL_CANCEL,
        );
        mk_default_button(
            hwnd,
            hi,
            font,
            "OK",
            right_edge - btn_w * 2 - 8,
            y,
            btn_w,
            ch + 2,
            IDC_SLBL_OK,
        );
    }

    // ── Generic text-prompt dialog (New/Rename package) ───────────────────────

    struct PromptTextState {
        label: String,
        initial: String,
        result: Option<String>,
        edit_value: HWND,
    }

    unsafe fn prompt_text_dialog(
        parent: HWND,
        title: &str,
        label: &str,
        initial: &str,
    ) -> Option<String> {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hinstance = HINSTANCE(hmodule.0);
        let class_w: Vec<u16> = CLASS_PROMPT_TEXT.encode_utf16().collect();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(prompt_text_proc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_w.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 as usize + 1) as *mut c_void),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let state = Box::new(PromptTextState {
            label: label.to_string(),
            initial: initial.to_string(),
            result: None,
            edit_value: HWND::default(),
        });
        let state_ptr = Box::into_raw(state);

        let w = 300i32;
        let h = 130i32;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let title_w = wide(title);
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW,
            PCWSTR(class_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            parent,
            None,
            hinstance,
            Some(state_ptr as *const c_void),
        )
        .expect("CreateWindowExW prompt text");

        drain_pending_clicks();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        PROMPT_TEXT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static PROMPT_TEXT_RESULT: std::cell::RefCell<Option<String>> =
            const { std::cell::RefCell::new(None) };
    }

    unsafe extern "system" fn prompt_text_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut PromptTextState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let state = &mut *ptr;
            create_prompt_text_controls(hwnd, state);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PromptTextState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    IDC_PROMPT_OK => {
                        state.result = Some(get_text(state.edit_value));
                        let _ = DestroyWindow(hwnd);
                    }
                    IDC_PROMPT_CANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(result) = state.result.take() {
                    PROMPT_TEXT_RESULT.with(|cell| {
                        *cell.borrow_mut() = Some(result);
                    });
                }
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn create_prompt_text_controls(hwnd: HWND, state: &mut PromptTextState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        let lx = 10i32;
        let ch = 22i32;
        let right_edge = 270i32;
        let btn_w = 70i32;
        let mut y = 14i32;

        mk_label(hwnd, hi, font, &state.label, lx, y, right_edge - lx, ch);
        y += ch + 6;
        state.edit_value = mk_edit(
            hwnd,
            hi,
            font,
            &state.initial,
            lx,
            y,
            right_edge - lx,
            ch,
            IDC_PROMPT_EDIT,
            0,
        );
        y += ch + 16;

        mk_button_ex(
            hwnd,
            hi,
            font,
            "Cancel",
            right_edge - btn_w,
            y,
            btn_w,
            ch + 2,
            IDC_PROMPT_CANCEL,
        );
        mk_default_button(
            hwnd,
            hi,
            font,
            "OK",
            right_edge - btn_w * 2 - 8,
            y,
            btn_w,
            ch + 2,
            IDC_PROMPT_OK,
        );
    }

    // ── Trigger editor ────────────────────────────────────────────────────────

    unsafe fn edit_selected_trigger(parent: HWND, state: &mut ConfigState) {
        let sel = SendMessageW(state.trigger_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
        if sel < 0 || sel as usize >= state.triggers.triggers.len() {
            return;
        }
        let original = state.triggers.triggers[sel as usize].clone();
        if let Some(edited) = open_trigger_editor(parent, original) {
            state.triggers.triggers[sel as usize] = edited;
            rebuild_trigger_list(state);
            SendMessageW(
                state.trigger_list,
                LB_SETCURSEL,
                WPARAM(sel as usize),
                LPARAM(0),
            );
        }
    }

    // ── Trigger editor dialog ─────────────────────────────────────────────────

    struct TriggerEditState {
        def: TriggerDef,
        result: Option<TriggerDef>,
        // Controls
        edit_name: HWND,
        chk_enabled: HWND,
        combo_logic: HWND,
        cond_list: HWND,
        btn_cond_add: HWND,
        btn_cond_edit: HWND,
        btn_cond_del: HWND,
        btn_cond_up: HWND,
        btn_cond_down: HWND,
        action_list: HWND,
        btn_action_add: HWND,
        btn_action_edit: HWND,
        btn_action_del: HWND,
        btn_action_up: HWND,
        btn_action_down: HWND,
    }

    /// Discards any queued mouse-button messages for this thread.
    ///
    /// Editor windows (trigger/condition/action) are plain owned top-level
    /// windows, not real modal dialogs, and every one of them is centred on
    /// screen — often near the exact spot where the button that spawned them
    /// sits. A stray click still in the queue (e.g. a double-click on
    /// "+ Add") gets hit-tested against whatever window is now on top when
    /// it's finally dispatched, so without this it can land on a control in
    /// the freshly created window (e.g. a colour swatch) instead of being
    /// discarded with the click that opened it.
    unsafe fn drain_pending_clicks() {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, WM_LBUTTONDOWN, WM_MBUTTONDBLCLK, PM_REMOVE).as_bool() {}
    }

    unsafe fn open_trigger_editor(parent: HWND, def: TriggerDef) -> Option<TriggerDef> {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hinstance = HINSTANCE(hmodule.0);
        let class_w: Vec<u16> = CLASS_TRIGGER_EDIT.encode_utf16().collect();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(trigger_edit_proc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_w.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 as usize + 1) as *mut c_void),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let state = Box::new(TriggerEditState {
            def,
            result: None,
            edit_name: HWND::default(),
            chk_enabled: HWND::default(),
            combo_logic: HWND::default(),
            cond_list: HWND::default(),
            btn_cond_add: HWND::default(),
            btn_cond_edit: HWND::default(),
            btn_cond_del: HWND::default(),
            btn_cond_up: HWND::default(),
            btn_cond_down: HWND::default(),
            action_list: HWND::default(),
            btn_action_add: HWND::default(),
            btn_action_edit: HWND::default(),
            btn_action_del: HWND::default(),
            btn_action_up: HWND::default(),
            btn_action_down: HWND::default(),
        });
        let state_ptr = Box::into_raw(state);

        let w = 530i32;
        let h = 540i32;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let title = wide("Edit Trigger");
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW,
            PCWSTR(class_w.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            parent,
            None,
            hinstance,
            Some(state_ptr as *const c_void),
        )
        .expect("CreateWindowExW trigger edit");

        drain_pending_clicks();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        TRIGGER_EDIT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static TRIGGER_EDIT_RESULT: std::cell::RefCell<Option<TriggerDef>> =
            const { std::cell::RefCell::new(None) };
    }

    unsafe extern "system" fn trigger_edit_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut TriggerEditState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let state = &mut *ptr;
            create_trigger_edit_controls(hwnd, state);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TriggerEditState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let notif = (wparam.0 >> 16) & 0xFFFF;
                handle_trigger_edit_command(hwnd, state, id, notif);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(result) = state.result.take() {
                    TRIGGER_EDIT_RESULT.with(|cell| {
                        *cell.borrow_mut() = Some(result);
                    });
                }
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let x = lparam.0 as i16 as i32;
                let y = (lparam.0 >> 16) as i16 as i32;
                let target = WindowFromPoint(POINT { x, y });
                if !target.0.is_null() && target != hwnd {
                    SendMessageW(target, msg, wparam, lparam)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn handle_trigger_edit_command(
        hwnd: HWND,
        state: &mut TriggerEditState,
        id: i32,
        notif: usize,
    ) {
        match id {
            // ── Condition list ─────────────────────────────────────────────
            IDC_COND_LIST if notif == LBN_DBLCLK => edit_selected_condition(hwnd, state),
            IDC_COND_LIST if notif == LBN_SELCHANGE => refresh_cond_action_buttons(state),

            IDC_COND_ADD => {
                let blank = Condition::Match {
                    match_type: MatchType::Regex,
                    pattern: String::new(),
                };
                if let Some(edited) = open_condition_editor(hwnd, blank) {
                    state.def.conditions.push(edited);
                    rebuild_cond_list(state);
                }
            }
            IDC_COND_EDIT => edit_selected_condition(hwnd, state),
            IDC_COND_DEL => {
                let sel =
                    SendMessageW(state.cond_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel >= 0 && (sel as usize) < state.def.conditions.len() {
                    state.def.conditions.remove(sel as usize);
                    rebuild_cond_list(state);
                    refresh_cond_action_buttons(state);
                }
            }
            IDC_COND_UP => {
                let sel =
                    SendMessageW(state.cond_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel > 0 && (sel as usize) < state.def.conditions.len() {
                    state.def.conditions.swap(sel as usize, sel as usize - 1);
                    rebuild_cond_list(state);
                    SendMessageW(
                        state.cond_list,
                        LB_SETCURSEL,
                        WPARAM((sel - 1) as usize),
                        LPARAM(0),
                    );
                    refresh_cond_action_buttons(state);
                }
            }
            IDC_COND_DOWN => {
                let sel =
                    SendMessageW(state.cond_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                let n = state.def.conditions.len();
                if sel >= 0 && (sel as usize + 1) < n {
                    state.def.conditions.swap(sel as usize, sel as usize + 1);
                    rebuild_cond_list(state);
                    SendMessageW(
                        state.cond_list,
                        LB_SETCURSEL,
                        WPARAM((sel + 1) as usize),
                        LPARAM(0),
                    );
                    refresh_cond_action_buttons(state);
                }
            }

            // ── Action list ────────────────────────────────────────────────
            IDC_ACTION_LIST if notif == LBN_DBLCLK => edit_selected_action(hwnd, state),
            IDC_ACTION_LIST if notif == LBN_SELCHANGE => refresh_cond_action_buttons(state),

            IDC_ACTION_ADD => {
                let blank = Action::Overlay {
                    icon: String::new(),
                    color: String::new(),
                    message: String::new(),
                    message_color: String::new(),
                    border_color: String::new(),
                    delay_secs: 0.0,
                    treatment: Treatment::default(),
                    priority: VoicePriority::default(),
                };
                if let Some(edited) = open_action_editor(hwnd, blank) {
                    state.def.actions.push(edited);
                    rebuild_action_list(state);
                }
            }
            IDC_ACTION_EDIT => edit_selected_action(hwnd, state),
            IDC_ACTION_DEL => {
                let sel =
                    SendMessageW(state.action_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel >= 0 && (sel as usize) < state.def.actions.len() {
                    state.def.actions.remove(sel as usize);
                    rebuild_action_list(state);
                    refresh_cond_action_buttons(state);
                }
            }
            IDC_ACTION_UP => {
                let sel =
                    SendMessageW(state.action_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel > 0 && (sel as usize) < state.def.actions.len() {
                    state.def.actions.swap(sel as usize, sel as usize - 1);
                    rebuild_action_list(state);
                    SendMessageW(
                        state.action_list,
                        LB_SETCURSEL,
                        WPARAM((sel - 1) as usize),
                        LPARAM(0),
                    );
                    refresh_cond_action_buttons(state);
                }
            }
            IDC_ACTION_DOWN => {
                let sel =
                    SendMessageW(state.action_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                let n = state.def.actions.len();
                if sel >= 0 && (sel as usize + 1) < n {
                    state.def.actions.swap(sel as usize, sel as usize + 1);
                    rebuild_action_list(state);
                    SendMessageW(
                        state.action_list,
                        LB_SETCURSEL,
                        WPARAM((sel + 1) as usize),
                        LPARAM(0),
                    );
                    refresh_cond_action_buttons(state);
                }
            }

            // ── OK / Cancel ────────────────────────────────────────────────
            IDC_EDIT_OK => {
                let name = get_text(state.edit_name);
                let enabled = SendMessageW(state.chk_enabled, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
                    as usize
                    == BST_CHECKED;
                let logic_idx =
                    SendMessageW(state.combo_logic, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
                let logic = if logic_idx == 0 {
                    ConditionLogic::All
                } else {
                    ConditionLogic::Any
                };

                state.result = Some(TriggerDef {
                    name,
                    enabled,
                    condition_logic: logic,
                    conditions: state.def.conditions.clone(),
                    actions: state.def.actions.clone(),
                });
                let _ = DestroyWindow(hwnd);
            }
            IDC_EDIT_CANCEL => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }

    // ── Condition list helpers ────────────────────────────────────────────────

    unsafe fn rebuild_cond_list(state: &TriggerEditState) {
        SendMessageW(state.cond_list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
        for c in &state.def.conditions {
            let label = condition_label(c);
            let lw = wide(&label);
            SendMessageW(
                state.cond_list,
                LB_ADDSTRING,
                WPARAM(0),
                LPARAM(lw.as_ptr() as isize),
            );
        }
    }

    fn condition_label(c: &Condition) -> String {
        match c {
            Condition::Match {
                match_type,
                pattern,
            } => {
                let mt = match match_type {
                    MatchType::Exact => "exact",
                    MatchType::Regex => "regex",
                    MatchType::Glob => "glob",
                };
                format!("[match/{mt}]  {pattern}")
            }
            Condition::Var {
                var_name,
                op,
                value,
            } => {
                let op_s = match op {
                    VarOp::Isset => "isset".to_string(),
                    VarOp::Equals => format!("== {value}"),
                    VarOp::Gt => format!("> {value}"),
                    VarOp::Gte => format!(">= {value}"),
                    VarOp::Lt => format!("< {value}"),
                    VarOp::Lte => format!("<= {value}"),
                    VarOp::Matches => format!("matches {value}"),
                };
                format!("[var]  {var_name}  {op_s}")
            }
        }
    }

    unsafe fn edit_selected_condition(hwnd: HWND, state: &mut TriggerEditState) {
        let sel = SendMessageW(state.cond_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
        if sel < 0 || (sel as usize) >= state.def.conditions.len() {
            return;
        }
        let original = state.def.conditions[sel as usize].clone();
        if let Some(edited) = open_condition_editor(hwnd, original) {
            state.def.conditions[sel as usize] = edited;
            rebuild_cond_list(state);
            SendMessageW(
                state.cond_list,
                LB_SETCURSEL,
                WPARAM(sel as usize),
                LPARAM(0),
            );
        }
    }

    // ── Action list helpers ───────────────────────────────────────────────────

    unsafe fn rebuild_action_list(state: &TriggerEditState) {
        SendMessageW(state.action_list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
        for a in &state.def.actions {
            let label = action_label(a);
            let lw = wide(&label);
            SendMessageW(
                state.action_list,
                LB_ADDSTRING,
                WPARAM(0),
                LPARAM(lw.as_ptr() as isize),
            );
        }
    }

    fn action_label(a: &Action) -> String {
        match a {
            Action::Overlay {
                icon,
                message,
                delay_secs,
                treatment,
                priority,
                ..
            } => {
                let mut tags = Vec::new();
                match priority {
                    VoicePriority::Emergency => tags.push("emergency"),
                    VoicePriority::Ambient => tags.push("ambient"),
                    VoicePriority::Operational => {}
                }
                match treatment {
                    Treatment::None => {}
                    Treatment::Glow => tags.push("glow"),
                    Treatment::Vibrate => tags.push("vibrate"),
                    Treatment::Pulse => tags.push("pulse"),
                }
                let suffix = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", tags.join(", "))
                };
                if *delay_secs > 0.0 {
                    format!("[overlay/{icon}] +{delay_secs:.1}s  {message}{suffix}")
                } else {
                    format!("[overlay/{icon}]  {message}{suffix}")
                }
            }
            Action::VoiceAlert { tts_text, priority } => {
                let prio = match priority {
                    VoicePriority::Emergency => "emergency",
                    VoicePriority::Operational => "operational",
                    VoicePriority::Ambient => "ambient",
                };
                format!("[voice/{prio}]  {tts_text}")
            }
            Action::PlaySound { sound, delay_secs } => {
                let snd = sound.as_deref().unwrap_or("(none)");
                if *delay_secs > 0.0 {
                    format!("[play_sound] +{delay_secs:.1}s  {snd}")
                } else {
                    format!("[play_sound]  {snd}")
                }
            }
            Action::StoreVar { var_name, value } => {
                format!("[store_var]  {var_name} = {value}")
            }
        }
    }

    unsafe fn edit_selected_action(hwnd: HWND, state: &mut TriggerEditState) {
        let sel = SendMessageW(state.action_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
        if sel < 0 || (sel as usize) >= state.def.actions.len() {
            return;
        }
        let original = state.def.actions[sel as usize].clone();
        if let Some(edited) = open_action_editor(hwnd, original) {
            state.def.actions[sel as usize] = edited;
            rebuild_action_list(state);
            SendMessageW(
                state.action_list,
                LB_SETCURSEL,
                WPARAM(sel as usize),
                LPARAM(0),
            );
        }
    }

    unsafe fn refresh_cond_action_buttons(state: &TriggerEditState) {
        let csel = SendMessageW(state.cond_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
        let nc = state.def.conditions.len() as i32;
        let has_c = csel >= 0 && csel < nc;
        let _ = EnableWindow(state.btn_cond_edit, BOOL(if has_c { 1 } else { 0 }));
        let _ = EnableWindow(state.btn_cond_del, BOOL(if has_c { 1 } else { 0 }));
        let _ = EnableWindow(
            state.btn_cond_up,
            BOOL(if has_c && csel > 0 { 1 } else { 0 }),
        );
        let _ = EnableWindow(
            state.btn_cond_down,
            BOOL(if has_c && csel < nc - 1 { 1 } else { 0 }),
        );

        let asel = SendMessageW(state.action_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
        let na = state.def.actions.len() as i32;
        let has_a = asel >= 0 && asel < na;
        let _ = EnableWindow(state.btn_action_edit, BOOL(if has_a { 1 } else { 0 }));
        let _ = EnableWindow(state.btn_action_del, BOOL(if has_a { 1 } else { 0 }));
        let _ = EnableWindow(
            state.btn_action_up,
            BOOL(if has_a && asel > 0 { 1 } else { 0 }),
        );
        let _ = EnableWindow(
            state.btn_action_down,
            BOOL(if has_a && asel < na - 1 { 1 } else { 0 }),
        );
    }

    // ── Trigger edit controls ─────────────────────────────────────────────────

    unsafe fn create_trigger_edit_controls(hwnd: HWND, state: &mut TriggerEditState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        let lx = 10i32;
        let lw = 90i32;
        let cx = lx + lw + 6;
        let cw = 200i32;
        let ch = 22i32;
        let row = 30i32;
        let btn_w = 80i32;
        let list_w = 360i32;
        let rbx = lx + list_w + 6; // right button column x
        let rbw = 72i32; // right button width
        let win_w = rbx + rbw + 8;
        let mut y = 10i32;

        // ── Name + Enabled ────────────────────────────────────────────────
        mk_label(hwnd, hi, font, "Name:", lx, y, lw, ch);
        state.edit_name = mk_edit(
            hwnd,
            hi,
            font,
            &state.def.name,
            cx,
            y,
            cw,
            ch,
            IDC_EDIT_NAME,
            0,
        );
        state.chk_enabled = mk_checkbox(
            hwnd,
            hi,
            font,
            "Enabled",
            cx + cw + 10,
            y,
            90,
            ch,
            IDC_EDIT_ENABLED,
        );
        if state.def.enabled {
            SendMessageW(
                state.chk_enabled,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row;

        mk_separator(hwnd, hi, font, lx, y, win_w - lx);
        y += 12;

        // ── IF [All/Any] of these conditions ─────────────────────────────
        mk_label(hwnd, hi, font, "IF", lx, y, 20, ch);
        state.combo_logic = mk_combo(hwnd, hi, font, lx + 24, y, 90, IDC_COND_LOGIC);
        cb_add(state.combo_logic, "ALL (AND)");
        cb_add(state.combo_logic, "ANY (OR)");
        let logic_idx = match state.def.condition_logic {
            ConditionLogic::All => 0usize,
            ConditionLogic::Any => 1usize,
        };
        SendMessageW(
            state.combo_logic,
            CB_SETCURSEL,
            WPARAM(logic_idx),
            LPARAM(0),
        );
        mk_label(hwnd, hi, font, "of these conditions:", lx + 122, y, 160, ch);
        y += row;

        // Conditions listbox + buttons.
        let list_h = 110i32;
        state.cond_list = mk_child(
            hwnd,
            hi,
            font,
            "LISTBOX",
            "",
            lx,
            y,
            list_w,
            list_h,
            IDC_COND_LIST,
            LBS_NOTIFY | LBS_HASSTRINGS | WS_VSCROLL_VAL | WS_BORDER.0 | WS_TABSTOP.0,
        );
        let mut by = y;
        state.btn_cond_add = mk_button_ex(hwnd, hi, font, "+ Add", rbx, by, rbw, ch, IDC_COND_ADD);
        by += row;
        state.btn_cond_edit = mk_button_ex(hwnd, hi, font, "Edit", rbx, by, rbw, ch, IDC_COND_EDIT);
        by += row;
        state.btn_cond_del =
            mk_button_ex(hwnd, hi, font, "✕ Remove", rbx, by, rbw, ch, IDC_COND_DEL);
        by += row + 4;
        state.btn_cond_up = mk_button_ex(hwnd, hi, font, "▲ Up", rbx, by, rbw, ch, IDC_COND_UP);
        by += row;
        state.btn_cond_down =
            mk_button_ex(hwnd, hi, font, "▼ Down", rbx, by, rbw, ch, IDC_COND_DOWN);
        y += list_h + 10;

        mk_separator(hwnd, hi, font, lx, y, win_w - lx);
        y += 12;

        // ── THEN all of these actions ─────────────────────────────────────
        mk_label(
            hwnd,
            hi,
            font,
            "THEN execute all of these actions:",
            lx,
            y,
            300,
            ch,
        );
        y += row;

        state.action_list = mk_child(
            hwnd,
            hi,
            font,
            "LISTBOX",
            "",
            lx,
            y,
            list_w,
            list_h,
            IDC_ACTION_LIST,
            LBS_NOTIFY | LBS_HASSTRINGS | WS_VSCROLL_VAL | WS_BORDER.0 | WS_TABSTOP.0,
        );
        let mut ay = y;
        state.btn_action_add =
            mk_button_ex(hwnd, hi, font, "+ Add", rbx, ay, rbw, ch, IDC_ACTION_ADD);
        ay += row;
        state.btn_action_edit =
            mk_button_ex(hwnd, hi, font, "Edit", rbx, ay, rbw, ch, IDC_ACTION_EDIT);
        ay += row;
        state.btn_action_del =
            mk_button_ex(hwnd, hi, font, "✕ Remove", rbx, ay, rbw, ch, IDC_ACTION_DEL);
        ay += row + 4;
        state.btn_action_up = mk_button_ex(hwnd, hi, font, "▲ Up", rbx, ay, rbw, ch, IDC_ACTION_UP);
        ay += row;
        state.btn_action_down =
            mk_button_ex(hwnd, hi, font, "▼ Down", rbx, ay, rbw, ch, IDC_ACTION_DOWN);
        y += list_h + 12;

        // ── Populate lists ────────────────────────────────────────────────
        rebuild_cond_list(state);
        rebuild_action_list(state);
        refresh_cond_action_buttons(state);

        // ── OK / Cancel ───────────────────────────────────────────────────
        let right = win_w;
        // Use whichever is lower: content-flow y or the last action button bottom + gap,
        // so OK/Cancel never overlap the ▼ Down button.
        let ok_y = y.max(ay + ch + 8);
        mk_button_ex(
            hwnd,
            hi,
            font,
            "Cancel",
            right - btn_w,
            ok_y,
            btn_w,
            ch,
            IDC_EDIT_CANCEL,
        );
        mk_default_button(
            hwnd,
            hi,
            font,
            "OK",
            right - btn_w * 2 - 8,
            ok_y,
            btn_w,
            ch,
            IDC_EDIT_OK,
        );

        let _ = (by, ay); // suppress unused warnings
    }

    // ── Condition editor dialog ───────────────────────────────────────────────

    struct ConditionEditState {
        cond: Condition,
        result: Option<Condition>,
        // controls
        type_combo: HWND, // 0=Match 1=Var
        match_type_combo: HWND,
        edit_pattern: HWND,
        edit_var_name: HWND,
        op_combo: HWND,
        edit_var_value: HWND,
        // groups for show/hide
        match_controls: Vec<HWND>,
        var_controls: Vec<HWND>,
    }

    unsafe fn open_condition_editor(parent: HWND, cond: Condition) -> Option<Condition> {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hinstance = HINSTANCE(hmodule.0);
        let class_w: Vec<u16> = CLASS_COND_EDIT.encode_utf16().collect();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(cond_edit_proc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_w.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 as usize + 1) as *mut c_void),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let state = Box::new(ConditionEditState {
            cond,
            result: None,
            type_combo: HWND::default(),
            match_type_combo: HWND::default(),
            edit_pattern: HWND::default(),
            edit_var_name: HWND::default(),
            op_combo: HWND::default(),
            edit_var_value: HWND::default(),
            match_controls: Vec::new(),
            var_controls: Vec::new(),
        });
        let state_ptr = Box::into_raw(state);

        let w = 440i32;
        let h = 240i32;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let title = wide("Edit Condition");
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW,
            PCWSTR(class_w.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            parent,
            None,
            hinstance,
            Some(state_ptr as *const c_void),
        )
        .expect("CreateWindowExW cond edit");

        drain_pending_clicks();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        COND_EDIT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static COND_EDIT_RESULT: std::cell::RefCell<Option<Condition>> =
            const { std::cell::RefCell::new(None) };
    }

    unsafe extern "system" fn cond_edit_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut ConditionEditState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let state = &mut *ptr;
            create_cond_edit_controls(hwnd, state);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ConditionEditState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let notif = (wparam.0 >> 16) & 0xFFFF;

                match id {
                    IDC_COND_TYPE if notif == CBN_SELCHANGE => {
                        let sel = SendMessageW(state.type_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0))
                            .0 as usize;
                        update_cond_type_visibility(state, sel == 0);
                    }
                    IDC_COND_OK => {
                        let type_sel =
                            SendMessageW(state.type_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0
                                as usize;
                        state.result = Some(if type_sel == 0 {
                            // Match
                            let mt_sel = SendMessageW(
                                state.match_type_combo,
                                CB_GETCURSEL,
                                WPARAM(0),
                                LPARAM(0),
                            )
                            .0 as usize;
                            let match_type = match mt_sel {
                                0 => MatchType::Exact,
                                1 => MatchType::Regex,
                                _ => MatchType::Glob,
                            };
                            Condition::Match {
                                match_type,
                                pattern: get_text(state.edit_pattern),
                            }
                        } else {
                            // Var
                            let op_sel =
                                SendMessageW(state.op_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0
                                    as usize;
                            let op = match op_sel {
                                0 => VarOp::Isset,
                                1 => VarOp::Equals,
                                2 => VarOp::Gt,
                                3 => VarOp::Gte,
                                4 => VarOp::Lt,
                                5 => VarOp::Lte,
                                _ => VarOp::Matches,
                            };
                            Condition::Var {
                                var_name: get_text(state.edit_var_name),
                                op,
                                value: get_text(state.edit_var_value),
                            }
                        });
                        let _ = DestroyWindow(hwnd);
                    }
                    IDC_COND_CANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(r) = state.result.take() {
                    COND_EDIT_RESULT.with(|cell| *cell.borrow_mut() = Some(r));
                }
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let x = lparam.0 as i16 as i32;
                let y = (lparam.0 >> 16) as i16 as i32;
                let target = WindowFromPoint(POINT { x, y });
                if !target.0.is_null() && target != hwnd {
                    SendMessageW(target, msg, wparam, lparam)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn create_cond_edit_controls(hwnd: HWND, state: &mut ConditionEditState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        let lx = 10i32;
        let lw = 110i32;
        let cx = lx + lw + 6;
        let cw = 260i32;
        let ch = 22i32;
        let row = 30i32;
        let btn_w = 80i32;
        let mut y = 10i32;

        // Type selector
        mk_label(hwnd, hi, font, "Condition type:", lx, y, lw, ch);
        state.type_combo = mk_combo(hwnd, hi, font, cx, y, 130, IDC_COND_TYPE);
        cb_add(state.type_combo, "Match (log line)");
        cb_add(state.type_combo, "Variable");
        let is_match = matches!(&state.cond, Condition::Match { .. });
        SendMessageW(
            state.type_combo,
            CB_SETCURSEL,
            WPARAM(if is_match { 0 } else { 1 }),
            LPARAM(0),
        );
        y += row;

        mk_separator(hwnd, hi, font, lx, y, cw + lw + 6);
        y += 12;

        // ── Match fields ──────────────────────────────────────────────────
        let ml = mk_label(hwnd, hi, font, "Match type:", lx, y, lw, ch);
        state.match_type_combo = mk_combo(hwnd, hi, font, cx, y, 130, IDC_COND_MATCH_TYPE);
        cb_add(state.match_type_combo, "Exact (substring)");
        cb_add(state.match_type_combo, "Regex");
        cb_add(state.match_type_combo, "Glob  (* ? {name})");
        let mt_idx = match &state.cond {
            Condition::Match { match_type, .. } => match match_type {
                MatchType::Exact => 0,
                MatchType::Regex => 1,
                MatchType::Glob => 2,
            },
            _ => 1,
        };
        SendMessageW(
            state.match_type_combo,
            CB_SETCURSEL,
            WPARAM(mt_idx),
            LPARAM(0),
        );
        state.match_controls.push(ml);
        state.match_controls.push(state.match_type_combo);
        y += row;

        let pl = mk_label(hwnd, hi, font, "Pattern:", lx, y, lw, ch);
        let pat = match &state.cond {
            Condition::Match { pattern, .. } => pattern.as_str(),
            _ => "",
        };
        state.edit_pattern = mk_edit(hwnd, hi, font, pat, cx, y, cw, ch, IDC_COND_PATTERN, 0);
        {
            let hint = wide("e.g. You have slain {target}!  or  (?P<name>.+) slain");
            SendMessageW(
                state.edit_pattern,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }
        state.match_controls.push(pl);
        state.match_controls.push(state.edit_pattern);
        y += row;

        // ── Variable fields ───────────────────────────────────────────────
        let vl = mk_label(hwnd, hi, font, "Variable name:", lx, y, lw, ch);
        let vname = match &state.cond {
            Condition::Var { var_name, .. } => var_name.as_str(),
            _ => "",
        };
        state.edit_var_name = mk_edit(hwnd, hi, font, vname, cx, y, 140, ch, IDC_COND_VAR_NAME, 0);
        state.var_controls.push(vl);
        state.var_controls.push(state.edit_var_name);

        let ol = mk_label(hwnd, hi, font, "Operator:", cx + 148, y, 70, ch);
        state.op_combo = mk_combo(hwnd, hi, font, cx + 222, y, 90, IDC_COND_VAR_OP);
        for op in &[
            "isset",
            "equals",
            "gt (>)",
            "gte (≥)",
            "lt (<)",
            "lte (≤)",
            "matches",
        ] {
            cb_add(state.op_combo, op);
        }
        let op_idx = match &state.cond {
            Condition::Var { op, .. } => match op {
                VarOp::Isset => 0,
                VarOp::Equals => 1,
                VarOp::Gt => 2,
                VarOp::Gte => 3,
                VarOp::Lt => 4,
                VarOp::Lte => 5,
                VarOp::Matches => 6,
            },
            _ => 0,
        };
        SendMessageW(state.op_combo, CB_SETCURSEL, WPARAM(op_idx), LPARAM(0));
        state.var_controls.push(ol);
        state.var_controls.push(state.op_combo);
        y += row;

        let vvl = mk_label(hwnd, hi, font, "Value:", lx, y, lw, ch);
        let vval = match &state.cond {
            Condition::Var { value, .. } => value.as_str(),
            _ => "",
        };
        state.edit_var_value = mk_edit(hwnd, hi, font, vval, cx, y, 200, ch, IDC_COND_VAR_VALUE, 0);
        state.var_controls.push(vvl);
        state.var_controls.push(state.edit_var_value);
        y += row + 6;

        // Show/hide based on current type.
        update_cond_type_visibility(state, is_match);

        // OK / Cancel.
        let right = cx + cw;
        mk_button_ex(
            hwnd,
            hi,
            font,
            "Cancel",
            right - btn_w,
            y,
            btn_w,
            ch,
            IDC_COND_CANCEL,
        );
        mk_default_button(
            hwnd,
            hi,
            font,
            "OK",
            right - btn_w * 2 - 8,
            y,
            btn_w,
            ch,
            IDC_COND_OK,
        );
    }

    unsafe fn update_cond_type_visibility(state: &ConditionEditState, is_match: bool) {
        let show_m = if is_match {
            windows::Win32::UI::WindowsAndMessaging::SW_SHOW
        } else {
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE
        };
        let show_v = if !is_match {
            windows::Win32::UI::WindowsAndMessaging::SW_SHOW
        } else {
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE
        };
        for &h in &state.match_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_m);
        }
        for &h in &state.var_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_v);
        }
    }

    // ── Action editor dialog ──────────────────────────────────────────────────

    struct ActionEditState {
        action: Action,
        result: Option<Action>,
        // icon items — populated at open time (presets + PNG files + Color Box)
        icon_items: Vec<IconItem>,
        // decoded icon thumbnails for the combo's owner-draw rows, keyed by
        // filename and populated lazily on first paint — see WM_DESTROY for
        // where the backing HBITMAPs get freed
        icon_thumbs: std::collections::HashMap<String, Option<ThumbBitmap>>,
        // sound label options — the union of every label defined across all
        // sound packages (see `sound_packages`), plus the current label if
        // it was since deleted from every package (so it isn't silently
        // dropped on re-save)
        sound_options: Vec<(String, String)>,
        // current colors as "#RRGGBB" strings; empty = default/none
        msg_color: String,
        icon_color: String,
        border_color: String,
        // cached solid brushes backing the swatch squares below, so
        // WM_CTLCOLORSTATIC never has to allocate a new GDI object per paint
        brush_msg_color: HBRUSH,
        brush_icon_color: HBRUSH,
        brush_border_color: HBRUSH,
        // controls
        type_combo: HWND, // 0=Overlay 1=StoreVar 2=VoiceAlert 3=PlaySound
        icon_combo: HWND,
        swatch_icon_color: HWND, // click to pick icon colour (only active with "colorbox")
        edit_message: HWND,
        swatch_msg_color: HWND,       // click to pick message text colour
        swatch_border_color: HWND,    // click to pick text stroke/outline colour
        treatment_combo: HWND,        // 0=None 1=Glow 2=Vibrate 3=Pulse — Overlay only
        overlay_priority_combo: HWND, // 0=Emergency 1=Operational 2=Ambient — Overlay only
        lbl_delay: HWND,
        edit_delay: HWND,
        lbl_sound: HWND,
        sound_combo: HWND,
        btn_sound_test: HWND, // ▶ preview button, hidden when no sound is selected
        edit_var_name: HWND,
        edit_var_value: HWND,
        // voice alert controls
        edit_tts_text: HWND,
        radio_priority_emergency: HWND,
        radio_priority_operational: HWND,
        radio_priority_ambient: HWND,
        // visibility groups
        overlay_controls: Vec<HWND>, // icon + message — Overlay only
        sound_controls: Vec<HWND>,   // sound — PlaySound only
        delay_controls: Vec<HWND>,   // delay — Overlay and PlaySound
        var_controls: Vec<HWND>,
        tts_controls: Vec<HWND>,
    }

    unsafe fn open_action_editor(parent: HWND, action: Action) -> Option<Action> {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hinstance = HINSTANCE(hmodule.0);
        let class_w: Vec<u16> = CLASS_ACTION_EDIT.encode_utf16().collect();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(action_edit_proc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_w.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 as usize + 1) as *mut c_void),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let (init_msg_color, init_icon_color, init_border_color) = match &action {
            Action::Overlay {
                message_color,
                color,
                border_color,
                ..
            } => (message_color.clone(), color.clone(), border_color.clone()),
            _ => (String::new(), String::new(), String::new()),
        };
        let brush_msg_color = make_swatch_brush(&init_msg_color, DEFAULT_TEXT_RGB);
        let brush_icon_color = make_swatch_brush(&init_icon_color, DEFAULT_ICON_SWATCH_RGB);
        let brush_border_color = make_swatch_brush(&init_border_color, DEFAULT_BORDER_RGB);
        // If this action's label was since deleted from every sound package,
        // make sure it's still in the list — otherwise the combo can't show
        // it and re-saving would silently clear it back to "(none)".
        let mut sound_options = crate::sound_packages::sound_packages::all_label_options();
        if let Action::PlaySound {
            sound: Some(ref s), ..
        } = action
        {
            if !s.is_empty() && !sound_options.iter().any(|(k, _)| k == s) {
                sound_options.push((s.clone(), s.clone()));
            }
        }
        let state = Box::new(ActionEditState {
            action,
            result: None,
            icon_items: build_icon_items(),
            icon_thumbs: std::collections::HashMap::new(),
            sound_options,
            msg_color: init_msg_color,
            icon_color: init_icon_color,
            border_color: init_border_color,
            brush_msg_color,
            brush_icon_color,
            brush_border_color,
            type_combo: HWND::default(),
            icon_combo: HWND::default(),
            swatch_icon_color: HWND::default(),
            edit_message: HWND::default(),
            swatch_msg_color: HWND::default(),
            swatch_border_color: HWND::default(),
            treatment_combo: HWND::default(),
            overlay_priority_combo: HWND::default(),
            lbl_delay: HWND::default(),
            edit_delay: HWND::default(),
            lbl_sound: HWND::default(),
            sound_combo: HWND::default(),
            btn_sound_test: HWND::default(),
            edit_var_name: HWND::default(),
            edit_var_value: HWND::default(),
            edit_tts_text: HWND::default(),
            radio_priority_emergency: HWND::default(),
            radio_priority_operational: HWND::default(),
            radio_priority_ambient: HWND::default(),
            overlay_controls: Vec::new(),
            sound_controls: Vec::new(),
            delay_controls: Vec::new(),
            var_controls: Vec::new(),
            tts_controls: Vec::new(),
        });
        let state_ptr = Box::into_raw(state);

        let w = 460i32;
        let h = 330i32;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let title = wide("Edit Action");
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW,
            PCWSTR(class_w.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            parent,
            None,
            hinstance,
            Some(state_ptr as *const c_void),
        )
        .expect("CreateWindowExW action edit");

        drain_pending_clicks();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        ACTION_EDIT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static ACTION_EDIT_RESULT: std::cell::RefCell<Option<Action>> =
            const { std::cell::RefCell::new(None) };
    }

    unsafe extern "system" fn action_edit_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut ActionEditState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let state = &mut *ptr;
            create_action_edit_controls(hwnd, state);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ActionEditState;
        if ptr.is_null() {
            if msg == WM_MEASUREITEM {
                let mis = &mut *(lparam.0 as *mut MeasureItemStruct);
                mis.item_height = ICON_ITEM_H as u32;
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        if msg == WM_MEASUREITEM {
            let mis = &mut *(lparam.0 as *mut MeasureItemStruct);
            mis.item_height = ICON_ITEM_H as u32;
            return LRESULT(1);
        }
        if msg == WM_DRAWITEM {
            let dis = &*(lparam.0 as *const DrawItemStruct);
            draw_icon_combo_item(dis, &state.icon_items, &mut state.icon_thumbs);
            return LRESULT(1);
        }
        if msg == WM_CTLCOLORSTATIC {
            let child = HWND(lparam.0 as *mut c_void);
            let brush = if child == state.swatch_msg_color {
                Some(state.brush_msg_color)
            } else if child == state.swatch_icon_color {
                Some(state.brush_icon_color)
            } else if child == state.swatch_border_color {
                Some(state.brush_border_color)
            } else {
                None
            };
            if let Some(b) = brush {
                return LRESULT(b.0 as isize);
            }
        }

        if msg == WM_SETCURSOR {
            let target = HWND(wparam.0 as *mut c_void);
            let over_swatch = target == state.swatch_msg_color
                || target == state.swatch_border_color
                || target == state.swatch_icon_color;
            if over_swatch && IsWindowEnabled(target).as_bool() {
                SetCursor(LoadCursorW(None, IDC_HAND).unwrap_or_default());
                return LRESULT(1);
            }
        }

        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let notif = (wparam.0 >> 16) & 0xFFFF;

                match id {
                    IDC_ACTION_TYPE if notif == CBN_SELCHANGE => {
                        let sel = SendMessageW(state.type_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0))
                            .0 as usize;
                        update_action_type_visibility(state, sel);
                    }
                    IDC_ACTION_SOUND if notif == CBN_SELCHANGE => {
                        update_sound_test_visibility(state);
                    }
                    IDC_ACTION_SOUND_TEST => {
                        if let Some(label) = read_selected_sound(state) {
                            crate::overlay::overlay::preview_sound_label(&label);
                        }
                    }
                    IDC_ACTION_ICON if notif == CBN_SELCHANGE => {
                        update_icon_color_visibility(state);
                    }
                    IDC_ACTION_MSG_COLOR_BTN if notif == STN_CLICKED => {
                        if let Some(c) = pick_color(hwnd, &state.msg_color) {
                            state.msg_color = c;
                            let _ = DeleteObject(HGDIOBJ(state.brush_msg_color.0));
                            state.brush_msg_color =
                                make_swatch_brush(&state.msg_color, DEFAULT_TEXT_RGB);
                            let _ = InvalidateRect(state.swatch_msg_color, None, true);
                        }
                    }
                    IDC_ACTION_BORDER_COLOR_BTN if notif == STN_CLICKED => {
                        if let Some(c) = pick_color(hwnd, &state.border_color) {
                            state.border_color = c;
                            let _ = DeleteObject(HGDIOBJ(state.brush_border_color.0));
                            state.brush_border_color =
                                make_swatch_brush(&state.border_color, DEFAULT_BORDER_RGB);
                            let _ = InvalidateRect(state.swatch_border_color, None, true);
                        }
                    }
                    IDC_ACTION_ICON_COLOR_BTN if notif == STN_CLICKED => {
                        if let Some(c) = pick_color(hwnd, &state.icon_color) {
                            state.icon_color = c;
                            let _ = DeleteObject(HGDIOBJ(state.brush_icon_color.0));
                            state.brush_icon_color =
                                make_swatch_brush(&state.icon_color, DEFAULT_ICON_SWATCH_RGB);
                            let _ = InvalidateRect(state.swatch_icon_color, None, true);
                        }
                    }
                    IDC_ACTION_OK => {
                        let type_sel =
                            SendMessageW(state.type_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0
                                as usize;
                        state.result = Some(if type_sel == 0 {
                            // Overlay
                            let icon_idx = SendMessageW(
                                state.icon_combo,
                                CB_GETCURSEL,
                                WPARAM(0),
                                LPARAM(0),
                            )
                            .0 as usize;
                            let icon = state
                                .icon_items
                                .get(icon_idx)
                                .map(|it| it.key.clone())
                                .unwrap_or_default();
                            let is_colorbox = icon == "colorbox";
                            let color = if is_colorbox {
                                state.icon_color.clone()
                            } else {
                                String::new()
                            };
                            let treatment_idx = SendMessageW(
                                state.treatment_combo,
                                CB_GETCURSEL,
                                WPARAM(0),
                                LPARAM(0),
                            )
                            .0 as usize;
                            let treatment = match treatment_idx {
                                1 => Treatment::Glow,
                                2 => Treatment::Vibrate,
                                3 => Treatment::Pulse,
                                _ => Treatment::None,
                            };
                            let priority_idx = SendMessageW(
                                state.overlay_priority_combo,
                                CB_GETCURSEL,
                                WPARAM(0),
                                LPARAM(0),
                            )
                            .0 as usize;
                            let priority = match priority_idx {
                                0 => VoicePriority::Emergency,
                                2 => VoicePriority::Ambient,
                                _ => VoicePriority::Operational,
                            };
                            Action::Overlay {
                                icon,
                                color,
                                message: get_text(state.edit_message),
                                message_color: state.msg_color.clone(),
                                border_color: state.border_color.clone(),
                                delay_secs: read_action_delay(state),
                                treatment,
                                priority,
                            }
                        } else if type_sel == 3 {
                            // PlaySound
                            Action::PlaySound {
                                sound: read_selected_sound(state),
                                delay_secs: read_action_delay(state),
                            }
                        } else if type_sel == 2 {
                            // VoiceAlert
                            let priority = if SendMessageW(
                                state.radio_priority_emergency,
                                BM_GETCHECK,
                                WPARAM(0),
                                LPARAM(0),
                            )
                            .0 as usize
                                == BST_CHECKED
                            {
                                VoicePriority::Emergency
                            } else if SendMessageW(
                                state.radio_priority_ambient,
                                BM_GETCHECK,
                                WPARAM(0),
                                LPARAM(0),
                            )
                            .0 as usize
                                == BST_CHECKED
                            {
                                VoicePriority::Ambient
                            } else {
                                VoicePriority::Operational
                            };
                            Action::VoiceAlert {
                                tts_text: get_text(state.edit_tts_text),
                                priority,
                            }
                        } else {
                            // StoreVar
                            Action::StoreVar {
                                var_name: get_text(state.edit_var_name),
                                value: get_text(state.edit_var_value),
                            }
                        });
                        let _ = DestroyWindow(hwnd);
                    }
                    IDC_ACTION_CANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(r) = state.result.take() {
                    ACTION_EDIT_RESULT.with(|cell| *cell.borrow_mut() = Some(r));
                }
                let _ = DeleteObject(HGDIOBJ(state.brush_msg_color.0));
                let _ = DeleteObject(HGDIOBJ(state.brush_icon_color.0));
                let _ = DeleteObject(HGDIOBJ(state.brush_border_color.0));
                for thumb in state.icon_thumbs.values().flatten() {
                    let _ = DeleteObject(HGDIOBJ(thumb.hbitmap.0));
                }
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let x = lparam.0 as i16 as i32;
                let y = (lparam.0 >> 16) as i16 as i32;
                let target = WindowFromPoint(POINT { x, y });
                if !target.0.is_null() && target != hwnd {
                    SendMessageW(target, msg, wparam, lparam)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn read_selected_sound(state: &ActionEditState) -> Option<String> {
        let snd_idx =
            SendMessageW(state.sound_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
        state.sound_options.get(snd_idx).and_then(
            |(k, _)| {
                if k.is_empty() {
                    None
                } else {
                    Some(k.clone())
                }
            },
        )
    }

    unsafe fn read_action_delay(state: &ActionEditState) -> f64 {
        get_text(state.edit_delay)
            .parse::<f64>()
            .unwrap_or(0.0)
            .max(0.0)
    }

    /// Open a native file picker restricted to WAV/MP3, returning the chosen absolute path.
    fn pick_sound_file() -> Option<String> {
        let filter: Vec<u16> = "Sound files\0*.wav;*.mp3\0\0".encode_utf16().collect();
        let mut buf = vec![0u16; 1024];
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
            nMaxFile: buf.len() as u32,
            Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST,
            ..Default::default()
        };
        let ok = unsafe { GetOpenFileNameW(&mut ofn) };
        if ok.as_bool() {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        } else {
            None
        }
    }

    unsafe fn create_action_edit_controls(hwnd: HWND, state: &mut ActionEditState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        // Layout constants.
        let lx = 10i32;
        let lw = 110i32;
        let cx = lx + lw + 6; // start of input controls
        let color_btn_w = 26i32; // small "…" color picker button
        let swatch_w = 22i32; // color swatch square shown next to each picker
        let right_margin = 10i32;
        // edit width: window client ≈ 460 − 2×frame. Use fixed right edge.
        let right_edge = 450i32;
        let edit_w = right_edge - cx - right_margin - 4 - color_btn_w - 4 - swatch_w;
        let cbx = cx + edit_w + 4; // x position of color button
        let ch = 22i32;
        let row = 30i32;
        let btn_w = 80i32;
        let mut y = 10i32;

        // ── Type selector ─────────────────────────────────────────────────
        mk_label(hwnd, hi, font, "Action type:", lx, y, lw, ch);
        state.type_combo = mk_combo(hwnd, hi, font, cx, y, 150, IDC_ACTION_TYPE);
        cb_add(state.type_combo, "Overlay message");
        cb_add(state.type_combo, "Store variable");
        cb_add(state.type_combo, "Voice Alert (TTS)");
        cb_add(state.type_combo, "Play Sound");
        let type_idx = match &state.action {
            Action::Overlay { .. } => 0usize,
            Action::StoreVar { .. } => 1usize,
            Action::VoiceAlert { .. } => 2usize,
            Action::PlaySound { .. } => 3usize,
        };
        SendMessageW(state.type_combo, CB_SETCURSEL, WPARAM(type_idx), LPARAM(0));
        y += row;

        mk_separator(hwnd, hi, font, lx, y, right_edge);
        y += 12;

        // ── Overlay fields ────────────────────────────────────────────────
        let fields_y = y;

        // Icon row: [Icon:] [icon_combo] [clickable colour swatch — only active for colorbox]
        let il = mk_label(hwnd, hi, font, "Icon:", lx, y, lw, ch);
        state.icon_combo = mk_icon_combo(
            hwnd,
            hi,
            font,
            cx,
            y,
            edit_w,
            IDC_ACTION_ICON,
            state.icon_items.len() as i32,
            ch,
        );
        for item in &state.icon_items {
            cb_add(state.icon_combo, &item.label);
        }
        let ico_key = match &state.action {
            Action::Overlay { icon, .. } => icon.as_str(),
            _ => "",
        };
        let ico_idx = find_icon_index(&state.icon_items, ico_key);
        SendMessageW(state.icon_combo, CB_SETCURSEL, WPARAM(ico_idx), LPARAM(0));
        state.swatch_icon_color = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            cbx,
            y,
            color_btn_w + 4 + swatch_w,
            ch,
            IDC_ACTION_ICON_COLOR_BTN,
            WS_BORDER.0 | SS_NOTIFY,
        );
        update_icon_color_visibility(state);
        state.overlay_controls.push(il);
        state.overlay_controls.push(state.icon_combo);
        state.overlay_controls.push(state.swatch_icon_color);
        y += row;

        // Message row: [Message:] [message_edit] [clickable colour swatch]
        let ml = mk_label(hwnd, hi, font, "Message:", lx, y, lw, ch);
        let msg_s = match &state.action {
            Action::Overlay { message, .. } => message.as_str(),
            _ => "",
        };
        state.edit_message = mk_edit(
            hwnd,
            hi,
            font,
            msg_s,
            cx,
            y,
            edit_w,
            ch,
            IDC_ACTION_MESSAGE,
            0,
        );
        {
            let hint = wide("{1},{2}=positional; {name}=named capture; {var}=stored var");
            SendMessageW(
                state.edit_message,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }
        state.swatch_msg_color = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            cbx,
            y,
            color_btn_w + 4 + swatch_w,
            ch,
            IDC_ACTION_MSG_COLOR_BTN,
            WS_BORDER.0 | SS_NOTIFY,
        );
        state.overlay_controls.push(ml);
        state.overlay_controls.push(state.edit_message);
        state.overlay_controls.push(state.swatch_msg_color);
        y += row;

        // Border color row: [Border color:] [clickable colour swatch] — text stroke/outline.
        let bcl = mk_label(hwnd, hi, font, "Border color:", lx, y, lw, ch);
        state.swatch_border_color = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            cx,
            y,
            color_btn_w + 4 + swatch_w,
            ch,
            IDC_ACTION_BORDER_COLOR_BTN,
            WS_BORDER.0 | SS_NOTIFY,
        );
        state.overlay_controls.push(bcl);
        state.overlay_controls.push(state.swatch_border_color);
        y += row;

        // Treatment row: [Treatment:] [treatment_combo] — visual effect while held at max size.
        let trl = mk_label(hwnd, hi, font, "Treatment:", lx, y, lw, ch);
        state.treatment_combo = mk_combo(hwnd, hi, font, cx, y, 150, IDC_ACTION_TREATMENT);
        for label in ["None", "Glow", "Vibrate", "Pulse"] {
            cb_add(state.treatment_combo, label);
        }
        let treatment_idx = match &state.action {
            Action::Overlay { treatment, .. } => match treatment {
                Treatment::None => 0usize,
                Treatment::Glow => 1usize,
                Treatment::Vibrate => 2usize,
                Treatment::Pulse => 3usize,
            },
            _ => 0usize,
        };
        SendMessageW(
            state.treatment_combo,
            CB_SETCURSEL,
            WPARAM(treatment_idx),
            LPARAM(0),
        );
        state.overlay_controls.push(trl);
        state.overlay_controls.push(state.treatment_combo);
        y += row;

        // Priority row: [Priority:] [overlay_priority_combo] — queue behaviour.
        let prl = mk_label(hwnd, hi, font, "Priority:", lx, y, lw, ch);
        state.overlay_priority_combo =
            mk_combo(hwnd, hi, font, cx, y, 150, IDC_ACTION_OVERLAY_PRIORITY);
        for label in [
            "Emergency (interrupts)",
            "Operational (queues)",
            "Ambient (may drop)",
        ] {
            cb_add(state.overlay_priority_combo, label);
        }
        let priority_idx = match &state.action {
            Action::Overlay { priority, .. } => match priority {
                VoicePriority::Emergency => 0usize,
                VoicePriority::Operational => 1usize,
                VoicePriority::Ambient => 2usize,
            },
            _ => 1usize,
        };
        SendMessageW(
            state.overlay_priority_combo,
            CB_SETCURSEL,
            WPARAM(priority_idx),
            LPARAM(0),
        );
        state.overlay_controls.push(prl);
        state.overlay_controls.push(state.overlay_priority_combo);
        y += row;

        // Sound row: [Sound:] [sound_combo] [... browse btn]
        state.lbl_sound = mk_label(hwnd, hi, font, "Sound:", lx, y, lw, ch);
        state.sound_combo = mk_combo(hwnd, hi, font, cx, y, edit_w, IDC_ACTION_SOUND);
        for (_, label) in &state.sound_options {
            cb_add(state.sound_combo, label);
        }
        {
            // Sound labels can be long (e.g. imported filenames) and share
            // common prefixes, so a plain combo clips them all to the same
            // few visible characters — enable horizontal scrolling of the
            // dropdown list instead of silently hiding the distinguishing
            // part of each name.
            use windows::Win32::UI::WindowsAndMessaging::CB_SETHORIZONTALEXTENT;
            let widest = state
                .sound_options
                .iter()
                .map(|(_, l)| l.chars().count())
                .max()
                .unwrap_or(0) as i32;
            SendMessageW(
                state.sound_combo,
                CB_SETHORIZONTALEXTENT,
                WPARAM((widest * 7 + 20).max(edit_w) as usize),
                LPARAM(0),
            );
        }
        let cur_sound = match &state.action {
            Action::PlaySound { sound, .. } => sound.clone(),
            _ => None,
        };
        let snd_key = cur_sound.as_deref().unwrap_or("");
        let snd_idx = state
            .sound_options
            .iter()
            .position(|(k, _)| k == snd_key)
            .unwrap_or(0);
        SendMessageW(state.sound_combo, CB_SETCURSEL, WPARAM(snd_idx), LPARAM(0));
        state.btn_sound_test = mk_button_ex(
            hwnd,
            hi,
            font,
            "\u{25B6}", // ▶ play icon
            cbx,
            y,
            color_btn_w,
            ch,
            IDC_ACTION_SOUND_TEST,
        );
        state.sound_controls.push(state.lbl_sound);
        state.sound_controls.push(state.sound_combo);
        y += row;

        // Delay row: [Delay (sec):] [delay_edit]
        state.lbl_delay = mk_label(hwnd, hi, font, "Delay (sec):", lx, y, lw, ch);
        let delay_s = match &state.action {
            Action::Overlay { delay_secs, .. } | Action::PlaySound { delay_secs, .. } => {
                if *delay_secs > 0.0 {
                    format!("{delay_secs}")
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };
        state.edit_delay = mk_edit(hwnd, hi, font, &delay_s, cx, y, 70, ch, IDC_ACTION_DELAY, 0);
        {
            let hint = wide("0 or empty = immediate");
            SendMessageW(
                state.edit_delay,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }
        state.delay_controls.push(state.lbl_delay);
        state.delay_controls.push(state.edit_delay);
        y += row;

        let overlay_end_y = y;

        // ── Store variable fields ─────────────────────────────────────────
        y = fields_y;
        let vnl = mk_label(hwnd, hi, font, "Variable name:", lx, y, lw, ch);
        let vname = match &state.action {
            Action::StoreVar { var_name, .. } => var_name.as_str(),
            _ => "",
        };
        state.edit_var_name = mk_edit(
            hwnd,
            hi,
            font,
            vname,
            cx,
            y,
            150,
            ch,
            IDC_ACTION_VAR_NAME,
            0,
        );
        state.var_controls.push(vnl);
        state.var_controls.push(state.edit_var_name);
        y += row;

        let vvl = mk_label(hwnd, hi, font, "Value:", lx, y, lw, ch);
        let vval = match &state.action {
            Action::StoreVar { value, .. } => value.as_str(),
            _ => "",
        };
        state.edit_var_value = mk_edit(
            hwnd,
            hi,
            font,
            vval,
            cx,
            y,
            edit_w,
            ch,
            IDC_ACTION_VAR_VALUE,
            0,
        );
        {
            let hint = wide("{1},{2}=positional; {name}=named capture; {var}=stored var");
            SendMessageW(
                state.edit_var_value,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }
        state.var_controls.push(vvl);
        state.var_controls.push(state.edit_var_value);
        y += row + 6;
        y = y.max(overlay_end_y);

        // ── Voice Alert fields ────────────────────────────────────────────
        let tts_y = fields_y;
        let ttl = mk_label(hwnd, hi, font, "TTS Text:", lx, tts_y, lw, ch);
        let init_tts = match &state.action {
            Action::VoiceAlert { tts_text, .. } => tts_text.as_str(),
            _ => "",
        };
        state.edit_tts_text = mk_edit(
            hwnd,
            hi,
            font,
            init_tts,
            cx,
            tts_y,
            edit_w + 4 + color_btn_w,
            ch,
            IDC_ACTION_TTS_TEXT,
            0,
        );
        {
            let hint = wide("{1},{2}=positional; {name}=named capture; {var}=stored var");
            SendMessageW(
                state.edit_tts_text,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }
        state.tts_controls.push(ttl);
        state.tts_controls.push(state.edit_tts_text);

        let prio_y = tts_y + row;
        let ptl = mk_label(hwnd, hi, font, "Priority:", lx, prio_y, lw, ch);
        // First radio button carries WS_GROUP to start the auto-radio group.
        state.radio_priority_emergency = mk_child(
            hwnd,
            hi,
            font,
            "BUTTON",
            "Emergency",
            cx,
            prio_y,
            95,
            ch,
            IDC_ACTION_PRIORITY_EMERGENCY,
            BS_AUTORADIOBUTTON | WS_GROUP_VAL | WS_TABSTOP.0,
        );
        state.radio_priority_operational = mk_child(
            hwnd,
            hi,
            font,
            "BUTTON",
            "Operational",
            cx + 100,
            prio_y,
            95,
            ch,
            IDC_ACTION_PRIORITY_OPERATIONAL,
            BS_AUTORADIOBUTTON | WS_TABSTOP.0,
        );
        state.radio_priority_ambient = mk_child(
            hwnd,
            hi,
            font,
            "BUTTON",
            "Ambient",
            cx + 200,
            prio_y,
            80,
            ch,
            IDC_ACTION_PRIORITY_AMBIENT,
            BS_AUTORADIOBUTTON | WS_TABSTOP.0,
        );

        // Set initial priority selection.
        let init_prio = match &state.action {
            Action::VoiceAlert { priority, .. } => priority.clone(),
            _ => VoicePriority::Operational,
        };
        let (chk_e, chk_o, chk_a) = match init_prio {
            VoicePriority::Emergency => (BST_CHECKED, 0, 0),
            VoicePriority::Operational => (0, BST_CHECKED, 0),
            VoicePriority::Ambient => (0, 0, BST_CHECKED),
        };
        SendMessageW(
            state.radio_priority_emergency,
            BM_SETCHECK,
            WPARAM(chk_e),
            LPARAM(0),
        );
        SendMessageW(
            state.radio_priority_operational,
            BM_SETCHECK,
            WPARAM(chk_o),
            LPARAM(0),
        );
        SendMessageW(
            state.radio_priority_ambient,
            BM_SETCHECK,
            WPARAM(chk_a),
            LPARAM(0),
        );

        state.tts_controls.push(ptl);
        state.tts_controls.push(state.radio_priority_emergency);
        state.tts_controls.push(state.radio_priority_operational);
        state.tts_controls.push(state.radio_priority_ambient);

        update_action_type_visibility(state, type_idx);

        let right = cx + edit_w;
        mk_button_ex(
            hwnd,
            hi,
            font,
            "Cancel",
            right - btn_w,
            y,
            btn_w,
            ch,
            IDC_ACTION_CANCEL,
        );
        mk_default_button(
            hwnd,
            hi,
            font,
            "OK",
            right - btn_w * 2 - 8,
            y,
            btn_w,
            ch,
            IDC_ACTION_OK,
        );
    }

    unsafe fn update_action_type_visibility(state: &ActionEditState, action_type: usize) {
        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOW};
        let show_o = if action_type == 0 { SW_SHOW } else { SW_HIDE };
        let show_v = if action_type == 1 { SW_SHOW } else { SW_HIDE };
        let show_t = if action_type == 2 { SW_SHOW } else { SW_HIDE };
        let show_s = if action_type == 3 { SW_SHOW } else { SW_HIDE };
        let show_d = if action_type == 0 || action_type == 3 {
            SW_SHOW
        } else {
            SW_HIDE
        };
        for &h in &state.overlay_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_o);
        }
        for &h in &state.sound_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_s);
        }
        for &h in &state.delay_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_d);
        }
        for &h in &state.var_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_v);
        }
        for &h in &state.tts_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_t);
        }
        reposition_sound_delay(state, action_type);
        update_sound_test_visibility(state);
        update_icon_color_visibility(state);
    }

    // Sound/Delay are laid out sequentially after the five Overlay-only rows
    // so Overlay keeps its normal reading order (icon, message, border,
    // treatment, priority, delay). But for Play Sound, those five rows are
    // hidden and Sound/Delay are the only fields — left at their Overlay-type
    // slot they'd sit ~150px below the separator with nothing above them.
    // Pull them up under the separator whenever Play Sound is selected.
    unsafe fn reposition_sound_delay(state: &ActionEditState, action_type: usize) {
        let lx = 10i32;
        let lw = 110i32;
        let cx = lx + lw + 6;
        let color_btn_w = 26i32;
        let swatch_w = 22i32;
        let right_margin = 10i32;
        let right_edge = 450i32;
        let edit_w = right_edge - cx - right_margin - 4 - color_btn_w - 4 - swatch_w;
        let cbx = cx + edit_w + 4;
        let ch = 22i32;
        let row = 30i32;
        let fields_y = 10 + row + 12;

        let (sound_y, delay_y) = if action_type == 3 {
            (fields_y, fields_y + row)
        } else {
            (fields_y + 5 * row, fields_y + 6 * row)
        };

        let mv = |h: HWND, x: i32, y: i32, w: i32| {
            let _ = SetWindowPos(h, None, x, y, w, ch, SWP_NOZORDER);
        };
        mv(state.lbl_sound, lx, sound_y, lw);
        // Combo boxes' window rect covers the dropped-down list too, not just
        // the closed line — resizing it to `ch` (as `mv` does for ordinary
        // controls) collapses the dropdown to nothing. Keep the tall extent
        // it was created with (see `mk_combo`) and only move it.
        let _ = SetWindowPos(
            state.sound_combo,
            None,
            cx,
            sound_y,
            edit_w,
            200,
            SWP_NOZORDER,
        );
        mv(state.btn_sound_test, cbx, sound_y, color_btn_w);
        mv(state.lbl_delay, lx, delay_y, lw);
        mv(state.edit_delay, cx, delay_y, 70);
    }

    // Shows the ▶ sound-preview button only when the action type is PlaySound
    // *and* a real sound (not "(none)") is currently selected.
    unsafe fn update_sound_test_visibility(state: &ActionEditState) {
        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOW};
        let type_sel =
            SendMessageW(state.type_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
        let snd_idx =
            SendMessageW(state.sound_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
        let has_sound = state
            .sound_options
            .get(snd_idx)
            .map(|(k, _)| !k.is_empty())
            .unwrap_or(false);
        let show = if type_sel == 3 && has_sound {
            SW_SHOW
        } else {
            SW_HIDE
        };
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_sound_test, show);
    }

    // Shows the clickable colour swatch only when "Color Box" is the
    // selected icon — for any real icon (or "(none)") there's no color to
    // pick, so the swatch is hidden rather than just greyed out.
    unsafe fn update_icon_color_visibility(state: &ActionEditState) {
        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOW};
        let sel = SendMessageW(state.icon_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
        let is_colorbox = state
            .icon_items
            .get(sel)
            .map(|it| it.key == "colorbox")
            .unwrap_or(false);
        let show = if is_colorbox { SW_SHOW } else { SW_HIDE };
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.swatch_icon_color, show);
        let _ = EnableWindow(
            state.swatch_icon_color,
            BOOL(if is_colorbox { 1 } else { 0 }),
        );
    }

    // ── Control creation — main config window ─────────────────────────────────

    unsafe fn create_controls(hwnd: HWND, state: &mut ConfigState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        // Sized to fit the tallest/widest tab's content (Logging, ~408px wide x
        // ~400px tall) with a comfortable margin, rather than the old fixed
        // 560x560 that left a lot of empty space on shorter/narrower tabs.
        let win_w = 470i32;
        let win_h = 520i32;
        let margin = 8i32;
        let tab_h = win_h - 60;
        let btn_w = 80i32;
        let btn_h = 24i32;

        state.tab_hwnd = mk_child(
            hwnd,
            hi,
            font,
            "SysTabControl32",
            "",
            margin,
            margin,
            win_w - margin * 2,
            tab_h,
            IDC_TAB,
            0,
        );
        insert_tab(state.tab_hwnd, 0, "General");
        insert_tab(state.tab_hwnd, 1, "Logging");
        insert_tab(state.tab_hwnd, 2, "Triggers");
        insert_tab(state.tab_hwnd, 3, "Overlays");
        insert_tab(state.tab_hwnd, 4, "DPS Meter");
        insert_tab(state.tab_hwnd, 5, "Voice");
        insert_tab(state.tab_hwnd, 6, "Windows");
        insert_tab(state.tab_hwnd, 7, "Sounds");

        let mut tab_area = windows::Win32::Foundation::RECT {
            left: margin,
            top: margin,
            right: win_w - margin,
            bottom: tab_h + margin,
        };
        SendMessageW(
            state.tab_hwnd,
            TCM_ADJUSTRECT,
            WPARAM(0),
            LPARAM(&mut tab_area as *mut _ as isize),
        );
        let ta = tab_area;

        state.general_panel = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            ta.left,
            ta.top,
            ta.right - ta.left,
            ta.bottom - ta.top,
            0,
            0,
        );
        create_general_panel(state, hwnd, hi, font, ta);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.general_panel,
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
        for &h in &state.general_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                h,
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }

        state.logging_panel = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            ta.left,
            ta.top,
            ta.right - ta.left,
            ta.bottom - ta.top,
            0,
            0,
        );
        create_logging_panel(state, hwnd, hi, font, ta);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.logging_panel,
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
        for &h in &state.logging_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                h,
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }

        state.triggers_panel = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            ta.left,
            ta.top,
            ta.right - ta.left,
            ta.bottom - ta.top,
            0,
            0,
        );
        create_triggers_panel(hwnd, hi, font, &state.triggers, ta);
        state.trigger_list = GetDlgItem(hwnd, IDC_TRIGGER_LIST).unwrap_or_default();
        state.btn_add = GetDlgItem(hwnd, IDC_BTN_ADD).unwrap_or_default();
        state.btn_edit = GetDlgItem(hwnd, IDC_BTN_EDIT).unwrap_or_default();
        state.btn_delete = GetDlgItem(hwnd, IDC_BTN_DELETE).unwrap_or_default();
        state.btn_move_up = GetDlgItem(hwnd, IDC_BTN_MOVE_UP).unwrap_or_default();
        state.btn_move_down = GetDlgItem(hwnd, IDC_BTN_MOVE_DOWN).unwrap_or_default();
        state.btn_toggle = GetDlgItem(hwnd, IDC_BTN_TOGGLE).unwrap_or_default();
        rebuild_trigger_list(state);
        refresh_trigger_buttons(state);

        state.appearance_panel = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            ta.left,
            ta.top,
            ta.right - ta.left,
            ta.bottom - ta.top,
            0,
            0,
        );
        create_appearance_panel(state, hwnd, hi, font, ta);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.appearance_panel,
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
        for i in 0..state.appearance_controls.len() {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                state.appearance_controls[i],
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }

        state.meter_panel = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            ta.left,
            ta.top,
            ta.right - ta.left,
            ta.bottom - ta.top,
            0,
            0,
        );
        create_meter_panel(state, hwnd, hi, font, ta);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.meter_panel,
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
        for &h in &state.meter_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                h,
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }

        state.voice_panel = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            ta.left,
            ta.top,
            ta.right - ta.left,
            ta.bottom - ta.top,
            0,
            0,
        );
        create_voice_panel(state, hwnd, hi, font, ta);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.voice_panel,
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
        for i in 0..state.voice_controls.len() {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                state.voice_controls[i],
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }

        state.windows_panel = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            ta.left,
            ta.top,
            ta.right - ta.left,
            ta.bottom - ta.top,
            0,
            0,
        );
        create_windows_panel(state, hwnd, hi, font, ta);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.windows_panel,
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
        for &h in &state.windows_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                h,
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }

        state.sounds_panel = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            ta.left,
            ta.top,
            ta.right - ta.left,
            ta.bottom - ta.top,
            0,
            0,
        );
        create_sounds_panel(state, hwnd, hi, font, ta);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.sounds_panel,
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );
        for &h in &state.sounds_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                h,
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }

        let by = win_h - btn_h - margin * 2;
        mk_button_ex(
            hwnd,
            hi,
            font,
            "Cancel",
            win_w - margin - btn_w,
            by,
            btn_w,
            btn_h,
            IDC_CANCEL,
        );
        mk_default_button(
            hwnd,
            hi,
            font,
            "Save",
            win_w - margin - btn_w * 2 - 8,
            by,
            btn_w,
            btn_h,
            IDC_SAVE,
        );
    }

    unsafe fn create_logging_panel(
        state: &mut ConfigState,
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let ox = ta.left;
        let oy = ta.top;
        let lx = 12i32;
        let lw = 92i32;
        let cx = 110i32;
        let cw = 298i32;
        let cw2 = 212i32;
        let bx = 328i32;
        let bw = 80i32;
        let ch = 22i32;
        let row = 30i32;
        let mut y = 12i32;

        macro_rules! g {
            ($h:expr) => {
                state.logging_controls.push($h)
            };
        }

        // ── Game ─────────────────────────────────────────────────────────────
        g!(mk_label(parent, hi, font, "Game:", ox + lx, oy + y, lw, ch));
        state.combo_game = mk_combo(parent, hi, font, ox + cx, oy + y, cw, IDC_GAME_COMBO);
        g!(state.combo_game);
        for gname in GAMES {
            cb_add(state.combo_game, gname);
        }
        SendMessageW(
            state.combo_game,
            CB_SETCURSEL,
            WPARAM(
                GAME_IDS
                    .iter()
                    .position(|&id| id == state.draft_game)
                    .unwrap_or(0),
            ),
            LPARAM(0),
        );
        y += row;

        // ── Server ───────────────────────────────────────────────────────────
        g!(mk_label(
            parent,
            hi,
            font,
            "Server:",
            ox + lx,
            oy + y,
            lw,
            ch
        ));
        state.combo_server = mk_combo(parent, hi, font, ox + cx, oy + y, cw, IDC_SERVER_COMBO);
        g!(state.combo_server);
        for s in SERVERS {
            cb_add(state.combo_server, s);
        }
        SendMessageW(
            state.combo_server,
            CB_SETCURSEL,
            WPARAM(
                SERVERS
                    .iter()
                    .position(|&s| s == state.draft_server)
                    .unwrap_or(0),
            ),
            LPARAM(0),
        );
        y += row;

        // ── Player ───────────────────────────────────────────────────────────
        g!(mk_label(
            parent,
            hi,
            font,
            "Player:",
            ox + lx,
            oy + y,
            lw,
            ch
        ));
        state.edit_player = mk_edit(
            parent,
            hi,
            font,
            &state.draft_player.clone(),
            ox + cx,
            oy + y,
            cw,
            ch,
            IDC_PLAYER_EDIT,
            0,
        );
        g!(state.edit_player);
        {
            let hint = wide("Auto from log filename");
            SendMessageW(
                state.edit_player,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }
        y += row;

        g!(mk_separator(
            parent,
            hi,
            font,
            ox + lx,
            oy + y,
            bx + bw - lx
        ));
        y += 14;

        // ── Log File ─────────────────────────────────────────────────────────
        g!(mk_label(
            parent,
            hi,
            font,
            "Log File:",
            ox + lx,
            oy + y,
            lw,
            ch
        ));
        state.edit_logfile = mk_edit(
            parent,
            hi,
            font,
            &state.draft_log_path.clone(),
            ox + cx,
            oy + y,
            cw2,
            ch,
            IDC_LOGFILE_EDIT,
            ES_READONLY,
        );
        g!(state.edit_logfile);
        g!(mk_button_ex(
            parent,
            hi,
            font,
            "Browse…",
            ox + bx,
            oy + y,
            bw,
            ch,
            IDC_LOGFILE_BROWSE
        ));
        y += row;

        // ── Server URL ───────────────────────────────────────────────────────
        g!(mk_label(
            parent,
            hi,
            font,
            "Server URL:",
            ox + lx,
            oy + y,
            lw,
            ch
        ));
        state.edit_url = mk_edit(
            parent,
            hi,
            font,
            &state.draft_server_url.clone(),
            ox + cx,
            oy + y,
            cw2,
            ch,
            IDC_URL_EDIT,
            0,
        );
        g!(state.edit_url);
        g!(mk_button_ex(
            parent,
            hi,
            font,
            "Test",
            ox + bx,
            oy + y,
            bw,
            ch,
            IDC_URL_TEST
        ));
        {
            let hint = wide("https://server:8766");
            SendMessageW(
                state.edit_url,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }
        y += row;

        // ── URL status ───────────────────────────────────────────────────────
        state.lbl_url_status = mk_child(
            parent,
            hi,
            font,
            "STATIC",
            "",
            ox + cx,
            oy + y,
            bx + bw - cx,
            ch,
            IDC_URL_STATUS,
            SS_LEFT,
        );
        g!(state.lbl_url_status);
        y += 26i32;

        g!(mk_separator(
            parent,
            hi,
            font,
            ox + lx,
            oy + y,
            bx + bw - lx
        ));
        y += 14;

        // ── Stream ID ────────────────────────────────────────────────────────
        g!(mk_label(
            parent,
            hi,
            font,
            "Stream ID:",
            ox + lx,
            oy + y,
            lw,
            ch
        ));
        state.lbl_streamid = mk_child(
            parent,
            hi,
            font,
            "STATIC",
            &state.stream_id_text.clone(),
            ox + cx,
            oy + y,
            cw2,
            ch,
            IDC_STREAMID_VALUE,
            SS_LEFT,
        );
        g!(state.lbl_streamid);
        state.btn_copy_streamid = mk_button_ex(
            parent,
            hi,
            font,
            "Copy ID",
            ox + bx,
            oy + y,
            bw,
            ch,
            IDC_COPY_STREAMID,
        );
        g!(state.btn_copy_streamid);
        let _ = EnableWindow(
            state.btn_copy_streamid,
            BOOL(if state.is_registered { 1 } else { 0 }),
        );
        y += row;

        // ── Server Password ──────────────────────────────────────────────────
        g!(mk_label(
            parent,
            hi,
            font,
            "Password:",
            ox + lx,
            oy + y,
            lw,
            ch
        ));
        state.edit_password = mk_edit(
            parent,
            hi,
            font,
            &state.draft_password.clone(),
            ox + cx,
            oy + y,
            cw,
            ch,
            IDC_PASSWORD_EDIT,
            ES_PASSWORD,
        );
        g!(state.edit_password);
        y += row;

        // ── Register / Unregister ────────────────────────────────────────────
        let reg_label = if state.is_registered {
            "Unregister"
        } else {
            "Register"
        };
        state.btn_register = mk_button_ex(
            parent,
            hi,
            font,
            reg_label,
            ox + cx,
            oy + y,
            110,
            ch,
            IDC_REGISTER_BTN,
        );
        g!(state.btn_register);
        y += row + 4;

        // ── Public streaming ─────────────────────────────────────────────────
        state.chk_public = mk_checkbox(
            parent,
            hi,
            font,
            "Allow public streaming",
            ox + cx,
            oy + y,
            cw,
            ch,
            IDC_PUBLIC_CHECK,
        );
        g!(state.chk_public);
        if state.draft_public {
            SendMessageW(
                state.chk_public,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row;

        // ── Remote logging toggle ────────────────────────────────────────────
        // Local processing (DPS meter, triggers, overlays) always runs off the
        // log tail; this only controls whether parsed events are also pushed
        // to the remote server.
        state.chk_remote_logging = mk_checkbox(
            parent,
            hi,
            font,
            "Enable remote logging (push to server)",
            ox + cx,
            oy + y,
            cw2,
            ch,
            IDC_REMOTE_LOGGING_CHECK,
        );
        g!(state.chk_remote_logging);
        if state.draft_remote_logging {
            SendMessageW(
                state.chk_remote_logging,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }

        // Set initial Register button state based on whether fields are filled.
        refresh_register_btn(state);
    }

    unsafe fn create_general_panel(
        state: &mut ConfigState,
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let ox = ta.left;
        let oy = ta.top;
        let lx = 12i32;
        let bx = 328i32;
        let bw = 80i32;
        let ch = 22i32;
        let y = 12i32;

        macro_rules! g {
            ($h:expr) => {
                state.general_controls.push($h)
            };
        }

        let box_x = ox + lx;
        let box_w = bx + bw - lx; // full content width, matches the Logging tab's boxes
        let inner = box_x + 16;

        // ── Spell icon import ────────────────────────────────────────────────
        let box_h = 24 + ch + 8;
        g!(mk_groupbox(
            parent,
            hi,
            font,
            "Spell Icon Import",
            box_x,
            oy + y,
            box_w,
            box_h,
        ));

        let ry = oy + y + 24;
        state.btn_import_spell_icons = mk_button_ex(
            parent,
            hi,
            font,
            "Import Spell Icons",
            inner,
            ry,
            160,
            ch,
            IDC_IMPORT_SPELL_ICONS,
        );
        g!(state.btn_import_spell_icons);
        state.lbl_import_spell_icons_status = mk_child(
            parent,
            hi,
            font,
            "STATIC",
            "",
            inner + 168,
            ry,
            box_w - 32 - 168,
            ch,
            0,
            SS_LEFT,
        );
        g!(state.lbl_import_spell_icons_status);
    }

    unsafe fn create_meter_panel(
        state: &mut ConfigState,
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let ox = ta.left;
        let oy = ta.top;
        let lx = 16i32;
        let lw = 140i32;
        let cx = 160i32;
        let cw = 100i32;
        let ch = 22i32;
        let row = 30i32;
        let mut y = 16i32;

        let cfg = state.handle.config.lock().unwrap().clone();

        state.meter_controls.push(mk_label(
            parent,
            hi,
            font,
            "Max rows:",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.edit_meter_max_rows = mk_edit(
            parent,
            hi,
            font,
            &cfg.meter_max_rows.to_string(),
            ox + cx,
            oy + y,
            cw,
            ch,
            IDC_METER_MAX_ROWS,
            ES_NUMBER,
        );
        state.meter_controls.push(state.edit_meter_max_rows);
        y += row;

        state.meter_controls.push(mk_label(
            parent,
            hi,
            font,
            "Idle hide (sec):",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.edit_meter_idle_secs = mk_edit(
            parent,
            hi,
            font,
            &cfg.meter_idle_secs.to_string(),
            ox + cx,
            oy + y,
            cw,
            ch,
            IDC_METER_IDLE_SECS,
            ES_NUMBER,
        );
        state.meter_controls.push(state.edit_meter_idle_secs);
        y += row;

        state.meter_controls.push(mk_label(
            parent,
            hi,
            font,
            "Font size:",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.edit_meter_font_size = mk_edit(
            parent,
            hi,
            font,
            &cfg.meter_font_size.to_string(),
            ox + cx,
            oy + y,
            cw,
            ch,
            IDC_METER_FONT_SIZE,
            ES_NUMBER,
        );
        state.meter_controls.push(state.edit_meter_font_size);
    }

    unsafe fn create_triggers_panel(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        _triggers: &TriggerConfig,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let ox = ta.left;
        let oy = ta.top;
        let pw = ta.right - ta.left;
        let ph = ta.bottom - ta.top;
        let btn_w = 90i32;
        let btn_h = 24i32;
        let bx = pw - btn_w - 4;
        let list_w = bx - 8;
        let list_h = ph - 8;

        mk_child(
            parent,
            hi,
            font,
            "LISTBOX",
            "",
            ox + 4,
            oy + 4,
            list_w,
            list_h,
            IDC_TRIGGER_LIST,
            LBS_NOTIFY | LBS_HASSTRINGS | WS_VSCROLL_VAL | WS_BORDER.0 | WS_TABSTOP.0,
        );

        let mut by = 4i32;
        let gap = btn_h + 4;
        mk_button_ex(
            parent,
            hi,
            font,
            "Add",
            ox + bx,
            oy + by,
            btn_w,
            btn_h,
            IDC_BTN_ADD,
        );
        by += gap;
        mk_button_ex(
            parent,
            hi,
            font,
            "Edit",
            ox + bx,
            oy + by,
            btn_w,
            btn_h,
            IDC_BTN_EDIT,
        );
        by += gap;
        mk_button_ex(
            parent,
            hi,
            font,
            "Delete",
            ox + bx,
            oy + by,
            btn_w,
            btn_h,
            IDC_BTN_DELETE,
        );
        by += gap + 8;
        mk_button_ex(
            parent,
            hi,
            font,
            "Move Up",
            ox + bx,
            oy + by,
            btn_w,
            btn_h,
            IDC_BTN_MOVE_UP,
        );
        by += gap;
        mk_button_ex(
            parent,
            hi,
            font,
            "Move Down",
            ox + bx,
            oy + by,
            btn_w,
            btn_h,
            IDC_BTN_MOVE_DOWN,
        );
        by += gap + 8;
        mk_button_ex(
            parent,
            hi,
            font,
            "Enable/Disable",
            ox + bx,
            oy + by,
            btn_w,
            btn_h,
            IDC_BTN_TOGGLE,
        );
    }

    /// Sounds tab: the relocated Sound enable/volume groupbox (moved here
    /// from General), an Active Package groupbox (combo + New/Rename/
    /// Delete/Export/Import), and a Sound Labels groupbox (listbox +
    /// Add/Edit/Delete) showing whichever package the combo currently
    /// selects. See `sound_packages` for the underlying label/package model.
    unsafe fn create_sounds_panel(
        state: &mut ConfigState,
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let box_x = ta.left + 8;
        let box_w = (ta.right - ta.left) - 16;
        let inner = box_x + 16;
        let ch = 22i32;
        let row = 30i32;
        let gap = 14i32;
        let mut y = ta.top + 8;

        macro_rules! s {
            ($h:expr) => {
                state.sounds_controls.push($h)
            };
        }

        // ── Sound (enable + volume) ─────────────────────────────────────
        let sound_box_h = 24 + ch * 2 + 8 + 6;
        s!(mk_groupbox(
            parent,
            hi,
            font,
            "Sound",
            box_x,
            y,
            box_w,
            sound_box_h
        ));
        let ry = y + 24;
        state.chk_sound_enabled = mk_checkbox(
            parent,
            hi,
            font,
            "Enable sounds",
            inner,
            ry,
            box_w - 32,
            ch,
            IDC_SOUND_ENABLED_CHECK,
        );
        s!(state.chk_sound_enabled);
        if state.cfg.sound_enabled {
            SendMessageW(
                state.chk_sound_enabled,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        let ry2 = ry + ch + 6;
        let label_w = 56i32;
        let pct_label_w = 44i32;
        let vol_gap = 8i32;
        s!(mk_label(
            parent, hi, font, "Volume:", inner, ry2, label_w, ch
        ));
        let track_x = inner + label_w;
        let track_w = box_w - 32 - label_w - pct_label_w - vol_gap;
        let pct = state.cfg.sound_volume.min(100) as i32;
        state.edit_sound_volume = mk_trackbar(
            parent,
            hi,
            font,
            track_x,
            ry2,
            track_w,
            ch,
            IDC_SOUND_VOLUME_SLIDER,
            pct,
        );
        s!(state.edit_sound_volume);
        state.lbl_sound_volume_value = mk_child(
            parent,
            hi,
            font,
            "STATIC",
            &format!("{pct}%"),
            track_x + track_w + vol_gap,
            ry2,
            pct_label_w,
            ch,
            0,
            SS_LEFT,
        );
        s!(state.lbl_sound_volume_value);
        y += sound_box_h + gap;

        // ── Active Package ───────────────────────────────────────────────
        let btn_h = 24i32;
        let pkg_box_h = 24 + row + btn_h + 12;
        s!(mk_groupbox(
            parent,
            hi,
            font,
            "Active Package",
            box_x,
            y,
            box_w,
            pkg_box_h
        ));
        let pkg_ry = y + 24;
        s!(mk_label(
            parent, hi, font, "Package:", inner, pkg_ry, 70, ch
        ));
        state.combo_sound_pkg = mk_combo(
            parent,
            hi,
            font,
            inner + 70,
            pkg_ry,
            box_w - 32 - 70,
            IDC_SOUND_PKG_COMBO,
        );
        s!(state.combo_sound_pkg);
        refresh_sound_packages_combo(state);

        let pkg_btn_y = pkg_ry + row;
        let pkg_btn_w = 76i32;
        let pkg_btn_gap = 3i32;
        let pkg_labels_ids = [
            ("New", IDC_SOUND_PKG_NEW),
            ("Rename", IDC_SOUND_PKG_RENAME),
            ("Delete", IDC_SOUND_PKG_DELETE),
            ("Export", IDC_SOUND_PKG_EXPORT),
            ("Import", IDC_SOUND_PKG_IMPORT),
        ];
        for (i, (label, id)) in pkg_labels_ids.iter().enumerate() {
            s!(mk_button_ex(
                parent,
                hi,
                font,
                label,
                inner + i as i32 * (pkg_btn_w + pkg_btn_gap),
                pkg_btn_y,
                pkg_btn_w,
                btn_h,
                *id,
            ));
        }
        y += pkg_box_h + gap;

        // ── Sound Labels ─────────────────────────────────────────────────
        let labels_box_h = (ta.bottom - ta.top) - (y - ta.top) - 8;
        s!(mk_groupbox(
            parent,
            hi,
            font,
            "Sound Labels",
            box_x,
            y,
            box_w,
            labels_box_h
        ));
        let list_btn_w = 90i32;
        let list_x = inner;
        let list_y = y + 24;
        let list_h = labels_box_h - 24 - 12;
        let list_w = box_w - 32 - list_btn_w - 8;
        state.sound_label_list = mk_child(
            parent,
            hi,
            font,
            "LISTBOX",
            "",
            list_x,
            list_y,
            list_w,
            list_h,
            IDC_SOUND_LABEL_LIST,
            LBS_NOTIFY | LBS_HASSTRINGS | WS_VSCROLL_VAL | WS_BORDER.0 | WS_TABSTOP.0,
        );
        s!(state.sound_label_list);
        let list_btn_x = list_x + list_w + 8;
        let mut lby = list_y;
        s!(mk_button_ex(
            parent,
            hi,
            font,
            "Add",
            list_btn_x,
            lby,
            list_btn_w,
            btn_h,
            IDC_SOUND_LABEL_ADD,
        ));
        lby += btn_h + 4;
        state.btn_sound_label_edit = mk_button_ex(
            parent,
            hi,
            font,
            "Edit",
            list_btn_x,
            lby,
            list_btn_w,
            btn_h,
            IDC_SOUND_LABEL_EDIT,
        );
        s!(state.btn_sound_label_edit);
        lby += btn_h + 4;
        state.btn_sound_label_delete = mk_button_ex(
            parent,
            hi,
            font,
            "Delete",
            list_btn_x,
            lby,
            list_btn_w,
            btn_h,
            IDC_SOUND_LABEL_DELETE,
        );
        s!(state.btn_sound_label_delete);

        rebuild_sound_label_list(state);
        refresh_sound_label_buttons(state);
    }

    unsafe fn create_appearance_panel(
        state: &mut ConfigState,
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let box_x = ta.left + 8;
        let box_w = (ta.right - ta.left) - 16;
        let inner = box_x + 16;
        let lw = 130i32;
        let cx = inner + lw + 4;
        let cw = 180i32;
        let ch = 22i32;
        let row = 30i32;
        let gap = 14i32;
        let mut y = ta.top + 8;

        macro_rules! a {
            ($h:expr) => {
                state.appearance_controls.push($h)
            };
        }

        // ── Alert overlay ────────────────────────────────────────────────
        let alert_rows = 6i32;
        let alert_box_h = 24 + (alert_rows - 1) * row + ch + 12;
        a!(mk_groupbox(
            parent,
            hi,
            font,
            "Alert Overlay",
            box_x,
            y,
            box_w,
            alert_box_h,
        ));
        let mut ry = y + 24;

        a!(mk_label(parent, hi, font, "Font:", inner, ry, lw, ch));
        state.font_combo = mk_combo(parent, hi, font, cx, ry, cw, IDC_FONT_COMBO);
        a!(state.font_combo);
        for name in FONT_NAMES {
            cb_add(state.font_combo, name);
        }
        let fi = FONT_NAMES
            .iter()
            .position(|&n| n == state.cfg.overlay_font)
            .unwrap_or(0);
        SendMessageW(state.font_combo, CB_SETCURSEL, WPARAM(fi), LPARAM(0));
        ry += row;

        a!(mk_label(
            parent,
            hi,
            font,
            "Start font size (pt):",
            inner,
            ry,
            lw,
            ch
        ));
        state.edit_start_font_size = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_start_font_size.to_string(),
            cx,
            ry,
            60,
            ch,
            IDC_START_FONT_SIZE,
        );
        a!(state.edit_start_font_size);
        ry += row;

        a!(mk_label(
            parent,
            hi,
            font,
            "Max font size (pt):",
            inner,
            ry,
            lw,
            ch
        ));
        state.edit_max_font_size = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_max_font_size.to_string(),
            cx,
            ry,
            60,
            ch,
            IDC_MAX_FONT_SIZE,
        );
        a!(state.edit_max_font_size);
        ry += row;

        a!(mk_label(
            parent,
            hi,
            font,
            "Fly-in speed (ms):",
            inner,
            ry,
            lw,
            ch
        ));
        state.edit_fly_ms = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_fly_ms.to_string(),
            cx,
            ry,
            60,
            ch,
            IDC_FLY_MS,
        );
        a!(state.edit_fly_ms);
        ry += row;

        a!(mk_label(
            parent,
            hi,
            font,
            "Hold time (secs):",
            inner,
            ry,
            lw,
            ch
        ));
        state.edit_hold_secs = mk_edit(
            parent,
            hi,
            font,
            &state.cfg.overlay_hold_secs.to_string(),
            cx,
            ry,
            60,
            ch,
            IDC_HOLD_SECS,
            0,
        );
        a!(state.edit_hold_secs);
        ry += row;

        a!(mk_label(
            parent,
            hi,
            font,
            "Opacity (0-255):",
            inner,
            ry,
            lw,
            ch
        ));
        state.edit_alpha = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_alpha.to_string(),
            cx,
            ry,
            60,
            ch,
            IDC_ALPHA_EDIT,
        );
        a!(state.edit_alpha);

        y += alert_box_h + gap;

        // ── History overlay ─────────────────────────────────────────────
        let hist_rows = 4i32;
        let hist_box_h = 24 + (hist_rows - 1) * row + ch + 12;
        a!(mk_groupbox(
            parent,
            hi,
            font,
            "History Overlay",
            box_x,
            y,
            box_w,
            hist_box_h,
        ));
        let mut ry = y + 24;

        a!(mk_label(
            parent,
            hi,
            font,
            "History font size (pt):",
            inner,
            ry,
            lw,
            ch
        ));
        state.edit_hist_font_size = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_history_font_size.to_string(),
            cx,
            ry,
            60,
            ch,
            IDC_HIST_FONT_SIZE,
        );
        a!(state.edit_hist_font_size);
        ry += row;

        a!(mk_label(
            parent,
            hi,
            font,
            "History idle hide (secs):",
            inner,
            ry,
            lw,
            ch
        ));
        state.edit_hist_idle = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_history_idle_secs.to_string(),
            cx,
            ry,
            60,
            ch,
            IDC_HIST_IDLE,
        );
        a!(state.edit_hist_idle);
        ry += row;

        a!(mk_label(
            parent,
            hi,
            font,
            "History max entries:",
            inner,
            ry,
            lw,
            ch
        ));
        state.edit_hist_max_entries = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_history_max_entries.to_string(),
            cx,
            ry,
            60,
            ch,
            IDC_HIST_MAX_ENTRIES,
        );
        a!(state.edit_hist_max_entries);
        ry += row;

        a!(mk_label(
            parent,
            hi,
            font,
            "History width (px):",
            inner,
            ry,
            lw,
            ch
        ));
        state.edit_hist_width = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_history_width.to_string(),
            cx,
            ry,
            60,
            ch,
            IDC_HIST_WIDTH,
        );
        a!(state.edit_hist_width);
    }

    /// Draws one bordered "window" section inside the Windows tab: a titled
    /// GROUPBOX containing an Enabled + Locked row and a Position X/Y row.
    /// Returns the four created control handles `(enabled, locked, edit_x,
    /// edit_y)` for the caller to store on `ConfigState`; the group box frame
    /// itself and every label are pushed straight into `controls`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn create_window_box(
        controls: &mut Vec<HWND>,
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        title: &str,
        box_x: i32,
        box_y: i32,
        box_w: i32,
        box_h: i32,
        enabled: bool,
        locked: bool,
        x_val: i32,
        y_val: i32,
        id_enabled: i32,
        id_locked: i32,
        id_x: i32,
        id_y: i32,
        id_reset: i32,
    ) -> (HWND, HWND, HWND, HWND) {
        let ch = 22i32;
        let row = 30i32;
        let inner = box_x + 16;
        let mut ry = box_y + 24;

        controls.push(mk_groupbox(
            parent, hi, font, title, box_x, box_y, box_w, box_h,
        ));

        let chk_enabled = mk_checkbox(parent, hi, font, "Enabled", inner, ry, 110, ch, id_enabled);
        controls.push(chk_enabled);
        if enabled {
            SendMessageW(chk_enabled, BM_SETCHECK, WPARAM(BST_CHECKED), LPARAM(0));
        }

        let chk_locked = mk_checkbox(
            parent,
            hi,
            font,
            "Locked (click-through)",
            inner + 150,
            ry,
            220,
            ch,
            id_locked,
        );
        controls.push(chk_locked);
        if locked {
            SendMessageW(chk_locked, BM_SETCHECK, WPARAM(BST_CHECKED), LPARAM(0));
        }
        ry += row;

        controls.push(mk_label(parent, hi, font, "Position:", inner, ry, 62, ch));
        controls.push(mk_label(parent, hi, font, "X", inner + 64, ry, 12, ch));
        let edit_x = mk_edit(
            parent,
            hi,
            font,
            &x_val.to_string(),
            inner + 78,
            ry,
            60,
            ch,
            id_x,
            0,
        );
        controls.push(edit_x);
        controls.push(mk_label(parent, hi, font, "Y", inner + 146, ry, 12, ch));
        let edit_y = mk_edit(
            parent,
            hi,
            font,
            &y_val.to_string(),
            inner + 160,
            ry,
            60,
            ch,
            id_y,
            0,
        );
        controls.push(edit_y);
        controls.push(mk_button_ex(
            parent,
            hi,
            font,
            "Reset",
            inner + 228,
            ry,
            90,
            ch,
            id_reset,
        ));

        (chk_enabled, chk_locked, edit_x, edit_y)
    }

    /// Windows tab — per-window enable/position/lock controls for the three
    /// HUD windows (alert overlay, history overlay, DPS meter), each in its
    /// own bordered group box so they're easy to compare at a glance,
    /// consolidated here instead of being split across the Overlays and DPS
    /// Meter tabs.
    unsafe fn create_windows_panel(
        state: &mut ConfigState,
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let box_x = ta.left + 8;
        let box_w = (ta.right - ta.left) - 16;
        // 24px title clearance + 2 rows of 30px + 14px bottom padding.
        let box_h = 92i32;
        let gap = 14i32;
        let mut y = ta.top + 8;

        let show_all_btn = mk_button_ex(
            parent,
            hi,
            font,
            "Show All Windows",
            box_x,
            y,
            160,
            26,
            IDC_SHOW_ALL_WINDOWS,
        );
        state.windows_controls.push(show_all_btn);
        state.windows_controls.push(mk_label(
            parent,
            hi,
            font,
            "unlocks + shows all three below, even if disabled or idle",
            box_x + 168,
            y + 5,
            box_w - 168,
            18,
        ));
        y += 26 + gap;

        let (h_en, h_lock, h_x, h_y) = create_window_box(
            &mut state.windows_controls,
            parent,
            hi,
            font,
            "Main Overlay",
            box_x,
            y,
            box_w,
            box_h,
            state.cfg.overlay_enabled,
            state.cfg.overlay_locked,
            state.cfg.overlay_x,
            state.cfg.overlay_y,
            IDC_OVERLAY_ENABLED,
            IDC_OVERLAY_LOCKED,
            IDC_OVERLAY_X,
            IDC_OVERLAY_Y,
            IDC_OVERLAY_RESET_POS,
        );
        state.chk_overlay_enabled = h_en;
        state.chk_overlay_locked = h_lock;
        state.edit_overlay_x = h_x;
        state.edit_overlay_y = h_y;
        y += box_h + gap;

        let (h_en, h_lock, h_x, h_y) = create_window_box(
            &mut state.windows_controls,
            parent,
            hi,
            font,
            "History Overlay",
            box_x,
            y,
            box_w,
            box_h,
            state.cfg.overlay_history_enabled,
            state.cfg.overlay_history_locked,
            state.cfg.overlay_history_x,
            state.cfg.overlay_history_y,
            IDC_HIST_ENABLED,
            IDC_HIST_LOCKED,
            IDC_HIST_X,
            IDC_HIST_Y,
            IDC_HIST_RESET_POS,
        );
        state.chk_hist_enabled = h_en;
        state.chk_hist_locked = h_lock;
        state.edit_hist_x = h_x;
        state.edit_hist_y = h_y;
        y += box_h + gap;

        let (h_en, h_lock, h_x, h_y) = create_window_box(
            &mut state.windows_controls,
            parent,
            hi,
            font,
            "DPS Meter",
            box_x,
            y,
            box_w,
            box_h,
            state.cfg.meter_enabled,
            state.cfg.meter_locked,
            state.cfg.meter_x,
            state.cfg.meter_y,
            IDC_METER_ENABLED,
            IDC_METER_LOCKED,
            IDC_METER_X,
            IDC_METER_Y,
            IDC_METER_RESET_POS,
        );
        state.chk_meter_enabled = h_en;
        state.chk_meter_locked = h_lock;
        state.edit_meter_x = h_x;
        state.edit_meter_y = h_y;
    }

    unsafe fn create_voice_panel(
        state: &mut ConfigState,
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let ox = ta.left;
        let oy = ta.top;
        let lx = 4i32;
        let lw = 140i32;
        let cx = lx + lw + 4;
        let cw = 200i32;
        let ch = 22i32;
        let row = 30i32;
        let mut y = 8i32;

        // ── Enable TTS ────────────────────────────────────────────────────
        state.chk_tts_enabled = mk_checkbox(
            parent,
            hi,
            font,
            "Enable Text-to-Speech",
            ox + cx,
            oy + y,
            cw + 40,
            ch,
            IDC_VOICE_TTS_ENABLED,
        );
        state.voice_controls.push(state.chk_tts_enabled);
        if state.cfg.tts_enabled {
            SendMessageW(
                state.chk_tts_enabled,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row;

        // ── Voice Speed ───────────────────────────────────────────────────
        state.voice_controls.push(mk_label(
            parent,
            hi,
            font,
            "Voice Speed:",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.combo_tts_speed = mk_combo(parent, hi, font, ox + cx, oy + y, cw, IDC_VOICE_SPEED);
        state.voice_controls.push(state.combo_tts_speed);
        cb_add(state.combo_tts_speed, "Normal (1x)");
        cb_add(state.combo_tts_speed, "Fast (1.2x)");
        cb_add(state.combo_tts_speed, "Faster (1.5x)");
        cb_add(state.combo_tts_speed, "Fastest (2x)");
        let speed_idx = match state.cfg.tts_speed {
            TtsSpeed::Normal => 0usize,
            TtsSpeed::Fast => 1usize,
            TtsSpeed::Faster => 2usize,
            TtsSpeed::Fastest => 3usize,
        };
        SendMessageW(
            state.combo_tts_speed,
            CB_SETCURSEL,
            WPARAM(speed_idx),
            LPARAM(0),
        );
        y += row;

        state
            .voice_controls
            .push(mk_separator(parent, hi, font, ox + lx, oy + y, cw + lw + 8));
        y += 12;

        // ── Audio Mode ────────────────────────────────────────────────────
        state.voice_controls.push(mk_label(
            parent,
            hi,
            font,
            "Audio Mode:",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));

        // Smart Priority (first in group — carries WS_GROUP)
        state.radio_tts_smart = mk_child(
            parent,
            hi,
            font,
            "BUTTON",
            "Smart Priority (Recommended)",
            ox + cx,
            oy + y,
            cw + 60,
            ch,
            IDC_VOICE_MODE_SMART,
            BS_AUTORADIOBUTTON | WS_GROUP_VAL | WS_TABSTOP.0,
        );
        state.voice_controls.push(state.radio_tts_smart);
        y += row;

        state.radio_tts_queue = mk_child(
            parent,
            hi,
            font,
            "BUTTON",
            "Queue All Messages",
            ox + cx,
            oy + y,
            cw + 60,
            ch,
            IDC_VOICE_MODE_QUEUE,
            BS_AUTORADIOBUTTON | WS_TABSTOP.0,
        );
        state.voice_controls.push(state.radio_tts_queue);
        y += row;

        state.radio_tts_interrupt = mk_child(
            parent,
            hi,
            font,
            "BUTTON",
            "Interrupt Constantly",
            ox + cx,
            oy + y,
            cw + 60,
            ch,
            IDC_VOICE_MODE_INTERRUPT,
            BS_AUTORADIOBUTTON | WS_TABSTOP.0,
        );
        state.voice_controls.push(state.radio_tts_interrupt);

        // Select current audio mode.
        let (sm, sq, si) = match state.cfg.tts_audio_mode {
            TtsAudioMode::SmartPriority => (BST_CHECKED, 0, 0),
            TtsAudioMode::QueueAll => (0, BST_CHECKED, 0),
            TtsAudioMode::InterruptConstantly => (0, 0, BST_CHECKED),
        };
        SendMessageW(state.radio_tts_smart, BM_SETCHECK, WPARAM(sm), LPARAM(0));
        SendMessageW(state.radio_tts_queue, BM_SETCHECK, WPARAM(sq), LPARAM(0));
        SendMessageW(
            state.radio_tts_interrupt,
            BM_SETCHECK,
            WPARAM(si),
            LPARAM(0),
        );
        y += row;

        state
            .voice_controls
            .push(mk_separator(parent, hi, font, ox + lx, oy + y, cw + lw + 8));
        y += 12;

        // ── Verbosity Filter ──────────────────────────────────────────────
        state.voice_controls.push(mk_label(
            parent,
            hi,
            font,
            "Verbosity Filter:",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));

        // First verbosity checkbox carries WS_GROUP to close the radio button group above.
        state.chk_tts_emergency = mk_child(
            parent,
            hi,
            font,
            "BUTTON",
            "Read Emergency Alerts",
            ox + cx,
            oy + y,
            cw + 40,
            ch,
            IDC_VOICE_READ_EMERGENCY,
            BS_AUTOCHECKBOX | WS_GROUP_VAL | WS_TABSTOP.0,
        );
        state.voice_controls.push(state.chk_tts_emergency);
        if state.cfg.tts_read_emergency {
            SendMessageW(
                state.chk_tts_emergency,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row;

        state.chk_tts_operational = mk_checkbox(
            parent,
            hi,
            font,
            "Read Operational Events",
            ox + cx,
            oy + y,
            cw + 40,
            ch,
            IDC_VOICE_READ_OPERATIONAL,
        );
        state.voice_controls.push(state.chk_tts_operational);
        if state.cfg.tts_read_operational {
            SendMessageW(
                state.chk_tts_operational,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row;

        state.chk_tts_ambient = mk_checkbox(
            parent,
            hi,
            font,
            "Read Ambient Notices",
            ox + cx,
            oy + y,
            cw + 40,
            ch,
            IDC_VOICE_READ_AMBIENT,
        );
        state.voice_controls.push(state.chk_tts_ambient);
        if state.cfg.tts_read_ambient {
            SendMessageW(
                state.chk_tts_ambient,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row;

        state
            .voice_controls
            .push(mk_separator(parent, hi, font, ox + lx, oy + y, cw + lw + 8));
        y += 12;

        // ── Voice Selector ────────────────────────────────────────────────
        state.voice_controls.push(mk_label(
            parent,
            hi,
            font,
            "Voice:",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.combo_tts_voice = mk_combo(
            parent,
            hi,
            font,
            ox + cx,
            oy + y,
            cw + 60,
            IDC_VOICE_VOICE_COMBO,
        );
        state.voice_controls.push(state.combo_tts_voice);

        // Enumerate installed SAPI voices (uses registry, no extra COM call needed).
        state.voice_names = crate::overlay::overlay::enumerate_tts_voices();
        let mut voice_sel = 0usize;
        for (i, (display, id)) in state.voice_names.iter().enumerate() {
            cb_add(state.combo_tts_voice, display);
            if !state.cfg.tts_voice.is_empty() && id == &state.cfg.tts_voice {
                voice_sel = i;
            }
        }
        SendMessageW(
            state.combo_tts_voice,
            CB_SETCURSEL,
            WPARAM(voice_sel),
            LPARAM(0),
        );

        let _ = y; // suppress unused warning
    }

    // ── Widget helpers ────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_child(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        class: &str,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
        extra: u32,
    ) -> HWND {
        let cw = wide(class);
        let tw = wide(text);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(cw.as_ptr()),
            PCWSTR(tw.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(extra),
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as isize as *mut c_void),
            hi,
            None,
        )
        .unwrap_or_default();
        SendMessageW(hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        hwnd
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_label(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> HWND {
        mk_child(parent, hi, font, "STATIC", text, x, y, w, h, 0, SS_LEFT)
    }

    unsafe fn mk_separator(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        x: i32,
        y: i32,
        w: i32,
    ) -> HWND {
        mk_child(parent, hi, font, "STATIC", "", x, y, w, 2, 0, SS_ETCHEDHORZ)
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_edit(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
        extra: u32,
    ) -> HWND {
        mk_child(
            parent,
            hi,
            font,
            "EDIT",
            text,
            x,
            y,
            w,
            h,
            id,
            ES_AUTOHSCROLL | WS_BORDER.0 | WS_TABSTOP.0 | extra,
        )
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_edit_num(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
    ) -> HWND {
        mk_edit(parent, hi, font, text, x, y, w, h, id, ES_NUMBER)
    }

    unsafe fn mk_combo(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        x: i32,
        y: i32,
        w: i32,
        id: i32,
    ) -> HWND {
        mk_child(
            parent,
            hi,
            font,
            "COMBOBOX",
            "",
            x,
            y,
            w,
            200,
            id,
            CBS_DROPDOWNLIST | CBS_HASSTRINGS | WS_TABSTOP.0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_icon_combo(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        x: i32,
        y: i32,
        w: i32,
        id: i32,
        item_count: i32,
        closed_h: i32,
    ) -> HWND {
        // Cap the dropdown at ICON_COMBO_MAX_ROWS rows and let the native
        // combo box scroll past that — sizing it to fit *every* item (the
        // previous approach) worked for a handful of built-ins but falls
        // apart once a large icon set (e.g. hundreds of extracted spell
        // icons) is dropped into icons/, requesting a dropdown taller than
        // any screen.
        //
        // WS_VSCROLL is required here — unlike a plain (non-owner-draw)
        // combo box, an owner-drawn dropdown (CBS_OWNERDRAWFIXED) does not
        // reliably grow a scrollbar on its own once the item count exceeds
        // the capped height; without this style the list just silently
        // truncates at ICON_COMBO_MAX_ROWS with no way to reach the rest.
        let dropdown_h = (item_count.clamp(1, ICON_COMBO_MAX_ROWS) * ICON_ITEM_H) + 4;
        let hwnd = mk_child(
            parent,
            hi,
            font,
            "COMBOBOX",
            "",
            x,
            y,
            w,
            dropdown_h,
            id,
            CBS_DROPDOWNLIST | CBS_HASSTRINGS | CBS_OWNERDRAWFIXED | WS_VSCROLL_VAL | WS_TABSTOP.0,
        );
        SendMessageW(
            hwnd,
            CB_SETITEMHEIGHT,
            WPARAM(0),
            LPARAM(ICON_ITEM_H as isize),
        );
        // For CBS_OWNERDRAWFIXED, wParam 0 above sets every *list* row's
        // height, but the closed/collapsed display field defaults to that
        // same taller row height too unless told otherwise — wParam -1
        // targets just the selection field. Without this, the icon combo
        // renders visibly taller than every other row in the dialog
        // (ICON_ITEM_H vs. the shared row height), eating into the gap
        // before the row underneath it.
        SendMessageW(
            hwnd,
            CB_SETITEMHEIGHT,
            WPARAM(usize::MAX),
            LPARAM(closed_h as isize),
        );
        hwnd
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_button_ex(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
    ) -> HWND {
        mk_child(
            parent,
            hi,
            font,
            "BUTTON",
            text,
            x,
            y,
            w,
            h,
            id,
            BS_PUSHBUTTON | WS_TABSTOP.0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_default_button(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
    ) -> HWND {
        mk_child(
            parent,
            hi,
            font,
            "BUTTON",
            text,
            x,
            y,
            w,
            h,
            id,
            BS_DEFPUSHBUTTON | WS_TABSTOP.0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_checkbox(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
    ) -> HWND {
        mk_child(
            parent,
            hi,
            font,
            "BUTTON",
            text,
            x,
            y,
            w,
            h,
            id,
            BS_AUTOCHECKBOX | WS_TABSTOP.0,
        )
    }

    /// Titled bordered frame (native Win32 GROUPBOX). Purely decorative — the
    /// controls that visually sit "inside" it are ordinary siblings placed at
    /// overlapping coordinates, not actual children of this window.
    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_groupbox(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> HWND {
        mk_child(parent, hi, font, "BUTTON", text, x, y, w, h, 0, BS_GROUPBOX)
    }

    /// Horizontal trackbar (native Win32 slider), ranged 0-100 and initialised
    /// to `pos`. Used by the General tab's overall sound volume control.
    #[allow(clippy::too_many_arguments)]
    unsafe fn mk_trackbar(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
        pos: i32,
    ) -> HWND {
        let hwnd = mk_child(
            parent,
            hi,
            font,
            "msctls_trackbar32",
            "",
            x,
            y,
            w,
            h,
            id,
            TBS_HORZ | TBS_AUTOTICKS | WS_TABSTOP.0,
        );
        SendMessageW(
            hwnd,
            TBM_SETRANGE,
            WPARAM(1),
            LPARAM((100i32 << 16) as isize),
        );
        SendMessageW(hwnd, TBM_SETPOS, WPARAM(1), LPARAM(pos as isize));
        hwnd
    }

    unsafe fn insert_tab(tab: HWND, idx: i32, text: &str) {
        let tw: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
        let mut item = windows::Win32::UI::Controls::TCITEMW {
            mask: windows::Win32::UI::Controls::TCIF_TEXT,
            pszText: windows::core::PWSTR(tw.as_ptr() as *mut u16),
            cchTextMax: tw.len() as i32,
            ..Default::default()
        };
        SendMessageW(
            tab,
            TCM_INSERTITEMW,
            WPARAM(idx as usize),
            LPARAM(&mut item as *mut _ as isize),
        );
    }

    unsafe fn cb_add(hwnd: HWND, text: &str) {
        let w = wide(text);
        SendMessageW(hwnd, CB_ADDSTRING, WPARAM(0), LPARAM(w.as_ptr() as isize));
    }

    unsafe fn get_text(hwnd: HWND) -> String {
        let len = GetWindowTextLengthW(hwnd) as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..len])
    }

    #[allow(dead_code)]
    unsafe fn set_wnd_text(hwnd: HWND, text: &str) {
        let w = wide(text);
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0u16)).collect()
    }

    fn msgbox(hwnd: HWND, title: &str, msg: &str, flags: MESSAGEBOX_STYLE) -> i32 {
        let tw = wide(title);
        let mw = wide(msg);
        unsafe { MessageBoxW(hwnd, PCWSTR(mw.as_ptr()), PCWSTR(tw.as_ptr()), flags).0 }
    }

    /// Open the system colour-picker dialog and return the selected colour as `#RRGGBB`.
    /// `initial` is the previously stored `#RRGGBB` string (or empty for black).
    unsafe fn pick_color(owner: HWND, initial: &str) -> Option<String> {
        let init_ref = hex_to_colorref(initial).unwrap_or(0);
        let mut custom = [COLORREF(0u32); 16];
        let mut cc = CHOOSECOLORW {
            lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: owner,
            rgbResult: COLORREF(init_ref),
            lpCustColors: custom.as_mut_ptr(),
            Flags: CC_RGBINIT | CHOOSECOLOR_FLAGS(0x00000100u32), // CC_FULLOPEN
            ..Default::default()
        };
        if ChooseColorW(&mut cc).as_bool() {
            let r = cc.rgbResult.0 & 0xFF;
            let g = (cc.rgbResult.0 >> 8) & 0xFF;
            let b = (cc.rgbResult.0 >> 16) & 0xFF;
            Some(format!("#{:02X}{:02X}{:02X}", r, g, b))
        } else {
            None
        }
    }

    /// Parse `#RRGGBB` → COLORREF (0x00BBGGRR).
    fn hex_to_colorref(s: &str) -> Option<u32> {
        let s = s.trim_start_matches('#');
        if s.len() == 6 {
            let r = u32::from_str_radix(&s[0..2], 16).ok()?;
            let g = u32::from_str_radix(&s[2..4], 16).ok()?;
            let b = u32::from_str_radix(&s[4..6], 16).ok()?;
            Some(r | (g << 8) | (b << 16))
        } else {
            None
        }
    }

    // ── Colour swatches (action editor) ───────────────────────────────────────

    /// Default swatch colours when a picker's hex string is empty, matching
    /// the alert overlay's own fallback colours (`overlay.rs`'s
    /// `DEFAULT_TEXT_RGB`/`DEFAULT_BORDER_RGB`).
    const DEFAULT_TEXT_RGB: (u8, u8, u8) = (255, 255, 255);
    const DEFAULT_BORDER_RGB: (u8, u8, u8) = (0, 0, 0);
    const DEFAULT_ICON_SWATCH_RGB: (u8, u8, u8) = (140, 140, 150);

    /// Build (and own) a solid brush for a colour swatch, falling back to
    /// `default_rgb` when `hex` is empty/unparseable. Caller is responsible
    /// for deleting the previous brush before replacing it, and for deleting
    /// the final one on window destruction.
    unsafe fn make_swatch_brush(hex: &str, default_rgb: (u8, u8, u8)) -> HBRUSH {
        let colorref = hex_to_colorref(hex).unwrap_or_else(|| {
            let (r, g, b) = default_rgb;
            r as u32 | ((g as u32) << 8) | ((b as u32) << 16)
        });
        CreateSolidBrush(COLORREF(colorref))
    }

    // ── Logging tab helpers (ported from the old settings_win.rs) ────────────

    unsafe fn combo_text(hwnd: HWND) -> String {
        let idx = SendMessageW(hwnd, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
        if idx < 0 {
            return String::new();
        }
        let len = SendMessageW(hwnd, CB_GETLBTEXTLEN, WPARAM(idx as usize), LPARAM(0)).0 as usize;
        let mut buf = vec![0u16; len + 1];
        SendMessageW(
            hwnd,
            CB_GETLBTEXT,
            WPARAM(idx as usize),
            LPARAM(buf.as_mut_ptr() as isize),
        );
        String::from_utf16_lossy(&buf[..len])
    }

    unsafe fn combo_find(hwnd: HWND, text: &str) -> i32 {
        let w = wide(text);
        SendMessageW(
            hwnd,
            CB_FINDSTRINGEXACT,
            WPARAM(usize::MAX),
            LPARAM(w.as_ptr() as isize),
        )
        .0 as i32
    }

    unsafe fn refresh_register_btn(state: &ConfigState) {
        if state.is_registered {
            return; // always enabled so the user can unregister
        }
        let url_ok = GetWindowTextLengthW(state.edit_url) > 0;
        let player_ok = GetWindowTextLengthW(state.edit_player) > 0;
        let _ = EnableWindow(
            state.btn_register,
            BOOL(if url_ok && player_ok { 1 } else { 0 }),
        );
    }

    fn copy_to_clipboard(text: &str) {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };
        const CF_UNICODETEXT: u32 = 13;
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
        let byte_count = wide.len() * 2;
        unsafe {
            let Ok(hglob) = GlobalAlloc(GMEM_MOVEABLE, byte_count) else {
                return;
            };
            let ptr = GlobalLock(hglob) as *mut u16;
            if ptr.is_null() {
                return;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hglob);
            if OpenClipboard(HWND::default()).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(hglob.0));
            let _ = CloseClipboard();
        }
    }

    /// Open a native "Save As" picker restricted to `.zip`, prefilled with
    /// `{default_name}.zip`, returning the chosen absolute path.
    fn pick_save_zip_file(default_name: &str) -> Option<String> {
        let filter: Vec<u16> = "Zip files\0*.zip\0\0".encode_utf16().collect();
        let mut buf = vec![0u16; 1024];
        let default_wide = wide(&format!("{default_name}.zip"));
        let n = default_wide.len().min(buf.len() - 1);
        buf[..n].copy_from_slice(&default_wide[..n]);
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
            nMaxFile: buf.len() as u32,
            Flags: OFN_OVERWRITEPROMPT,
            ..Default::default()
        };
        let ok = unsafe { GetSaveFileNameW(&mut ofn) };
        if ok.as_bool() {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        } else {
            None
        }
    }

    /// Open a native file picker restricted to `.zip`, returning the chosen absolute path.
    fn pick_zip_file() -> Option<String> {
        let filter: Vec<u16> = "Zip files\0*.zip\0\0".encode_utf16().collect();
        let mut buf = vec![0u16; 1024];
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
            nMaxFile: buf.len() as u32,
            Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST,
            ..Default::default()
        };
        let ok = unsafe { GetOpenFileNameW(&mut ofn) };
        if ok.as_bool() {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        } else {
            None
        }
    }

    fn pick_log_file() -> Option<String> {
        let filter: Vec<u16> = "Log files\0*.txt\0\0".encode_utf16().collect();
        let mut buf = vec![0u16; 1024];
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
            nMaxFile: buf.len() as u32,
            Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST,
            ..Default::default()
        };
        let ok = unsafe { GetOpenFileNameW(&mut ofn) };
        if ok.as_bool() {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        } else {
            None
        }
    }

    fn player_from_path(path: &str) -> Option<String> {
        let stem = std::path::Path::new(path).file_stem()?.to_str()?;
        let parts: Vec<&str> = stem.split('_').collect();
        if parts.len() >= 3 {
            Some(parts[1].to_string())
        } else {
            None
        }
    }

    fn server_from_path(path: &str) -> Option<String> {
        let stem = std::path::Path::new(path).file_stem()?.to_str()?;
        let parts: Vec<&str> = stem.split('_').collect();
        if parts.len() >= 3 {
            Some(parts[2..].join("_"))
        } else {
            None
        }
    }

    // ── Logging tab: background URL test ──────────────────────────────────────

    enum UrlTestResult {
        Connected { requires_password: bool },
        Failed(String),
    }

    fn test_url(url: &str) -> UrlTestResult {
        let health = format!("{}/health", url.trim_end_matches('/'));
        match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(6))
            .build()
            .and_then(|c| c.get(&health).send())
        {
            Ok(resp) => {
                let json = resp.json::<serde_json::Value>().ok();
                let is_froklog = json
                    .as_ref()
                    .and_then(|v| v.get("ok")?.as_bool())
                    .unwrap_or(false);
                if !is_froklog {
                    return UrlTestResult::Failed(
                        "Not a froklog server (wrong URL or port?)".into(),
                    );
                }
                let requires = json
                    .and_then(|v| v.get("requires_password")?.as_bool())
                    .unwrap_or(false);
                UrlTestResult::Connected {
                    requires_password: requires,
                }
            }
            Err(e) => UrlTestResult::Failed(e.to_string()),
        }
    }

    // ── Logging tab: background register ────────────────────────────────────────

    enum RegisterResult {
        Ok {
            stream_id: String,
            stream_token: String,
            view_token: String,
        },
        Err(String),
    }

    fn patch_public_stream(url: &str, stream_id: &str, stream_token: &str, public_stream: bool) {
        let endpoint = format!("{}/stream/{}", url.trim_end_matches('/'), stream_id);
        let body = serde_json::json!({ "public_stream": public_stream });
        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        else {
            return;
        };
        let _ = client
            .patch(&endpoint)
            .bearer_auth(stream_token)
            .json(&body)
            .send();
    }

    fn do_register(
        url: &str,
        player: &str,
        server: &str,
        game: &str,
        password: &str,
        public_stream: bool,
    ) -> RegisterResult {
        let endpoint = format!("{}/stream", url.trim_end_matches('/'));
        let body = serde_json::json!({ "player": player, "server": server, "game": game, "public_stream": public_stream });

        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => return RegisterResult::Err(e.to_string()),
        };

        let mut req = client.post(&endpoint).json(&body);
        if !password.is_empty() {
            req = req.bearer_auth(password);
        }

        let resp = match req.send() {
            Ok(r) => r,
            Err(e) => return RegisterResult::Err(format!("Request failed: {e}")),
        };

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return RegisterResult::Err(
                "Server requires a password — fill in the Password field.".into(),
            );
        }
        if !status.is_success() {
            return RegisterResult::Err(format!("Server returned {status}"));
        }

        let json: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(e) => return RegisterResult::Err(format!("Bad response: {e}")),
        };

        if let Some(err) = json.get("error").and_then(|e| e.as_str()) {
            return RegisterResult::Err(format!("Server error: {err}"));
        }

        let field = |k: &str| -> Result<String, String> {
            json[k]
                .as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| format!("missing {k} in server response"))
        };

        match (
            field("stream_id"),
            field("stream_token"),
            field("view_token"),
        ) {
            (Ok(id), Ok(tok), Ok(vtok)) => RegisterResult::Ok {
                stream_id: id,
                stream_token: tok,
                view_token: vtok,
            },
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => RegisterResult::Err(e),
        }
    }
}

// ── Public re-export ──────────────────────────────────────────────────────────

#[cfg(feature = "tray")]
pub use overlay_config::open_settings;
