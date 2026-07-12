/// Overlay & Trigger configuration window.
///
/// Opened from the tray menu ("Overlay Settings…").
/// Split into two tabs via a Win32 TabControl:
///   Tab 0 — Triggers  : listbox of triggers + add/edit/delete/enable controls
///   Tab 1 — Appearance: font, size, transparency, idle-hide timeout
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
        CreateSolidBrush, DeleteObject, FillRect, GetStockObject, SetBkMode, SetTextColor,
        TextOutW, COLOR_BTNFACE, DEFAULT_GUI_FONT, HBRUSH, HDC, HGDIOBJ, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Controls::Dialogs::{
        ChooseColorW, CC_RGBINIT, CHOOSECOLORW, CHOOSECOLOR_FLAGS,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
        IsDialogMessageW, LoadCursorW, MessageBoxW, PostQuitMessage, RegisterClassExW,
        SendMessageW, SetWindowLongPtrW, SetWindowTextW, TranslateMessage, WindowFromPoint,
        CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CREATESTRUCTW, GWLP_USERDATA, HMENU, IDC_ARROW,
        LB_ADDSTRING, LB_GETCURSEL, MB_ICONWARNING, MB_YESNO, MESSAGEBOX_STYLE, MSG, SM_CXSCREEN,
        SM_CYSCREEN, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
        WM_NOTIFY, WM_SETFONT, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW,
        WS_EX_DLGMODALFRAME, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    };

    use crate::config::{Config, TtsAudioMode, TtsSpeed};
    use crate::tray::tray::AppHandle;
    use crate::triggers::engine::{
        Action, Condition, ConditionLogic, MatchType, TriggerConfig, TriggerDef, VarOp,
        VoicePriority,
    };

    // ── Control IDs ───────────────────────────────────────────────────────────

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
    const IDC_FONT_SIZE: i32 = 211;
    const IDC_ALPHA_EDIT: i32 = 212;
    const IDC_IDLE_EDIT: i32 = 213;
    const IDC_MAX_ENTRIES: i32 = 214;
    const IDC_OVERLAY_ENABLED: i32 = 215;
    const IDC_OVERLAY_X: i32 = 216;
    const IDC_OVERLAY_Y: i32 = 217;

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
    #[allow(dead_code)]
    const IDC_ACTION_SOUND_BROWSE: i32 = 506;
    const IDC_ACTION_VAR_NAME: i32 = 507;
    const IDC_ACTION_VAR_VALUE: i32 = 508;
    const IDC_ACTION_OK: i32 = 509;
    const IDC_ACTION_CANCEL: i32 = 510;
    const IDC_ACTION_MSG_COLOR_BTN: i32 = 511;
    const IDC_ACTION_ICON_COLOR_BTN: i32 = 512;
    // Voice Alert action fields
    const IDC_ACTION_TTS_TEXT: i32 = 513;
    const IDC_ACTION_PRIORITY_EMERGENCY: i32 = 514;
    const IDC_ACTION_PRIORITY_OPERATIONAL: i32 = 515;
    const IDC_ACTION_PRIORITY_AMBIENT: i32 = 516;

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

    // Win32 control style / message constants.
    const SS_LEFT: u32 = 0x0000_0000;
    const SS_ETCHEDHORZ: u32 = 0x0000_0010;
    const BS_PUSHBUTTON: u32 = 0x0000_0000;
    const BS_DEFPUSHBUTTON: u32 = 0x0000_0001;
    const BS_AUTOCHECKBOX: u32 = 0x0000_0003;
    const BS_AUTORADIOBUTTON: u32 = 0x0000_0009;
    const WS_GROUP_VAL: u32 = 0x0002_0000;
    const CBS_DROPDOWNLIST: u32 = 0x0000_0003;
    const CBS_HASSTRINGS: u32 = 0x0000_0200;
    const ES_AUTOHSCROLL: u32 = 0x0000_0080;
    const ES_NUMBER: u32 = 0x0000_2000;
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
    const TCM_INSERTITEMW: u32 = 0x133E;
    const TCM_GETCURSEL: u32 = 0x130B;
    const TCM_ADJUSTRECT: u32 = 0x1328;
    const LB_SETCURSEL: u32 = 0x0186;
    const LB_RESETCONTENT: u32 = 0x0184;
    const CBS_OWNERDRAWFIXED: u32 = 0x0010;
    const WM_DRAWITEM: u32 = 0x002B;
    const WM_MEASUREITEM: u32 = 0x002C;
    const WM_MOUSEWHEEL: u32 = 0x020A;
    const CB_SETITEMHEIGHT: u32 = 0x0153;
    const ODS_SELECTED: u32 = 0x0001;
    const SWATCH_SIZE: i32 = 12;
    const SWATCH_PAD: i32 = 4;
    const ICON_ITEM_H: i32 = 20;

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
        let swatch = crate::assets::assets::icon_swatch_color;
        let mut items = vec![
            IconItem {
                key: String::new(),
                label: "(none)".into(),
                color: 0x00888888,
            },
            IconItem {
                key: "heal".into(),
                label: "Heal".into(),
                color: swatch("heal"),
            },
            IconItem {
                key: "damage".into(),
                label: "Damage".into(),
                color: swatch("damage"),
            },
            IconItem {
                key: "warn".into(),
                label: "Warning".into(),
                color: swatch("warn"),
            },
            IconItem {
                key: "spell".into(),
                label: "Spell".into(),
                color: swatch("spell"),
            },
        ];

        // PNG/JPG files from the icons directory.
        if let Ok(dir) = std::fs::read_dir(crate::assets::assets::icons_dir()) {
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

        items.push(IconItem {
            key: "colorbox".into(),
            label: "Color Box".into(),
            color: swatch("colorbox"),
        });
        items
    }

    fn find_icon_index(items: &[IconItem], key: &str) -> usize {
        items.iter().position(|it| it.key == key).unwrap_or(0)
    }

    unsafe fn draw_icon_combo_item(dis: &DrawItemStruct, icon_items: &[IconItem]) {
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

    const FONT_NAMES: &[&str] = &[
        "Segoe UI",
        "Arial",
        "Consolas",
        "Courier New",
        "Tahoma",
        "Verdana",
        "Times New Roman",
    ];

    // ── Main dialog state ─────────────────────────────────────────────────────

    struct ConfigState {
        handle: Arc<AppHandle>,
        triggers: TriggerConfig,
        cfg: Config,
        tab_hwnd: HWND,
        trigger_list: HWND,
        btn_add: HWND,
        btn_edit: HWND,
        btn_delete: HWND,
        btn_move_up: HWND,
        btn_move_down: HWND,
        btn_toggle: HWND,
        font_combo: HWND,
        edit_font_size: HWND,
        edit_alpha: HWND,
        edit_idle: HWND,
        edit_max_entries: HWND,
        chk_overlay_enabled: HWND,
        edit_overlay_x: HWND,
        edit_overlay_y: HWND,
        triggers_panel: HWND,
        appearance_panel: HWND,
        appearance_controls: Vec<HWND>,
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

    pub fn open_overlay_config(handle: Arc<AppHandle>) {
        std::thread::Builder::new()
            .name("froklog-overlay-cfg".into())
            .spawn(move || run_config_thread(handle))
            .expect("spawn overlay config thread");
    }

    fn run_config_thread(handle: Arc<AppHandle>) {
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
            edit_font_size: HWND::default(),
            edit_alpha: HWND::default(),
            edit_idle: HWND::default(),
            edit_max_entries: HWND::default(),
            chk_overlay_enabled: HWND::default(),
            edit_overlay_x: HWND::default(),
            edit_overlay_y: HWND::default(),
            triggers_panel: HWND::default(),
            appearance_panel: HWND::default(),
            appearance_controls: Vec::new(),
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

            let w = 560i32;
            // create_controls lays out with a 560px client area; add ~50px for
            // the title bar + WS_EX_DLGMODALFRAME borders so nothing is clipped.
            let h = 610i32;
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            let x = (sw - w) / 2;
            let y = (sh - h) / 2;

            let title = wide("Overlay & Triggers");
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
            .expect("CreateWindowExW overlay config");

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if !IsDialogMessageW(hwnd, &mut msg).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        unsafe { windows::Win32::System::Com::CoUninitialize() };
        handle.overlay_config_open.store(false, Ordering::Relaxed);
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

    // ── Command handler ───────────────────────────────────────────────────────

    unsafe fn handle_command(hwnd: HWND, state: &mut ConfigState, wparam: WPARAM, _lparam: LPARAM) {
        let id = (wparam.0 & 0xFFFF) as i32;
        let notif = (wparam.0 >> 16) & 0xFFFF;

        match id {
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
        let font_idx =
            SendMessageW(state.font_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
        let font_name = if font_idx < FONT_NAMES.len() {
            FONT_NAMES[font_idx].to_string()
        } else {
            "Segoe UI".to_string()
        };

        let font_size: u32 = get_text(state.edit_font_size)
            .parse()
            .unwrap_or(14)
            .max(8)
            .min(72);
        let alpha: u8 = get_text(state.edit_alpha)
            .parse::<u32>()
            .unwrap_or(200)
            .min(255) as u8;
        let idle: u32 = get_text(state.edit_idle).parse().unwrap_or(6);
        let max_entries: usize = get_text(state.edit_max_entries)
            .parse()
            .unwrap_or(8)
            .max(1)
            .min(20);
        let overlay_enabled =
            SendMessageW(state.chk_overlay_enabled, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED;

        let overlay_x: i32 = get_text(state.edit_overlay_x).parse().unwrap_or(-1);
        let overlay_y: i32 = get_text(state.edit_overlay_y).parse().unwrap_or(-1);

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

        {
            let mut cfg = state.handle.config.lock().unwrap();
            cfg.overlay_font = font_name;
            cfg.overlay_font_size = font_size;
            cfg.overlay_alpha = alpha;
            cfg.overlay_idle_secs = idle;
            cfg.overlay_max_entries = max_entries;
            cfg.overlay_enabled = overlay_enabled;
            cfg.overlay_x = overlay_x;
            cfg.overlay_y = overlay_y;
            cfg.tts_enabled = tts_enabled;
            cfg.tts_speed = tts_speed;
            cfg.tts_audio_mode = tts_audio_mode;
            cfg.tts_read_emergency = tts_read_emergency;
            cfg.tts_read_operational = tts_read_operational;
            cfg.tts_read_ambient = tts_read_ambient;
            cfg.tts_voice = tts_voice;
            cfg.save();
        }

        state.triggers.save();

        if let Some(engine) = state.handle.trigger_engine.lock().unwrap().as_ref() {
            engine.reload(&state.triggers);
        }

        state.handle.restart.store(true, Ordering::Relaxed);

        let _ = DestroyWindow(hwnd);
    }

    // ── Tab switching ─────────────────────────────────────────────────────────

    unsafe fn switch_tab(state: &mut ConfigState, tab: i32) {
        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOW};
        state.current_tab = tab;

        let show_hide = |show: bool| if show { SW_SHOW } else { SW_HIDE };

        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.triggers_panel,
            show_hide(tab == 0),
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.appearance_panel,
            show_hide(tab == 1),
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.voice_panel,
            show_hide(tab == 2),
        );

        let tr_show = show_hide(tab == 0);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.trigger_list, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_add, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_edit, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_delete, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_move_up, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_move_down, tr_show);
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_toggle, tr_show);

        let ap_show = show_hide(tab == 1);
        for i in 0..state.appearance_controls.len() {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                state.appearance_controls[i],
                ap_show,
            );
        }

        let vp_show = show_hide(tab == 2);
        for i in 0..state.voice_controls.len() {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                state.voice_controls[i],
                vp_show,
            );
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

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &mut msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        TRIGGER_EDIT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static TRIGGER_EDIT_RESULT: std::cell::RefCell<Option<TriggerDef>> =
            std::cell::RefCell::new(None);
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
                    delay_secs: 0.0,
                    sound: None,
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
                ..
            } => {
                if *delay_secs > 0.0 {
                    format!("[overlay/{icon}] +{delay_secs:.1}s  {message}")
                } else {
                    format!("[overlay/{icon}]  {message}")
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

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &mut msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        COND_EDIT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static COND_EDIT_RESULT: std::cell::RefCell<Option<Condition>> =
            std::cell::RefCell::new(None);
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
        // sound options — built-in presets + user .wav/.mp3 files from sounds/
        sound_options: Vec<(String, String)>,
        // current colors as "#RRGGBB" strings; empty = default/none
        msg_color: String,
        icon_color: String,
        // controls
        type_combo: HWND, // 0=Overlay 1=StoreVar 2=VoiceAlert
        icon_combo: HWND,
        btn_icon_color: HWND, // color picker for icon (only active with "colorbox")
        edit_message: HWND,
        btn_msg_color: HWND, // color picker for message text color
        edit_delay: HWND,
        sound_combo: HWND,
        edit_var_name: HWND,
        edit_var_value: HWND,
        // voice alert controls
        edit_tts_text: HWND,
        radio_priority_emergency: HWND,
        radio_priority_operational: HWND,
        radio_priority_ambient: HWND,
        // visibility groups
        overlay_controls: Vec<HWND>,
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

        let (init_msg_color, init_icon_color) = match &action {
            Action::Overlay {
                message_color,
                color,
                ..
            } => (message_color.clone(), color.clone()),
            _ => (String::new(), String::new()),
        };
        let state = Box::new(ActionEditState {
            action,
            result: None,
            icon_items: build_icon_items(),
            sound_options: crate::assets::assets::build_sound_options(),
            msg_color: init_msg_color,
            icon_color: init_icon_color,
            type_combo: HWND::default(),
            icon_combo: HWND::default(),
            btn_icon_color: HWND::default(),
            edit_message: HWND::default(),
            btn_msg_color: HWND::default(),
            edit_delay: HWND::default(),
            sound_combo: HWND::default(),
            edit_var_name: HWND::default(),
            edit_var_value: HWND::default(),
            edit_tts_text: HWND::default(),
            radio_priority_emergency: HWND::default(),
            radio_priority_operational: HWND::default(),
            radio_priority_ambient: HWND::default(),
            overlay_controls: Vec::new(),
            var_controls: Vec::new(),
            tts_controls: Vec::new(),
        });
        let state_ptr = Box::into_raw(state);

        let w = 460i32;
        let h = 300i32;
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

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &mut msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        ACTION_EDIT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static ACTION_EDIT_RESULT: std::cell::RefCell<Option<Action>> =
            std::cell::RefCell::new(None);
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
            draw_icon_combo_item(dis, &state.icon_items);
            return LRESULT(1);
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
                    IDC_ACTION_ICON if notif == CBN_SELCHANGE => {
                        let sel = SendMessageW(state.icon_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0))
                            .0 as usize;
                        let is_colorbox = state
                            .icon_items
                            .get(sel)
                            .map(|it| it.key == "colorbox")
                            .unwrap_or(false);
                        let _ = EnableWindow(
                            state.btn_icon_color,
                            BOOL(if is_colorbox { 1 } else { 0 }),
                        );
                    }
                    IDC_ACTION_MSG_COLOR_BTN => {
                        if let Some(c) = pick_color(hwnd, &state.msg_color) {
                            state.msg_color = c;
                        }
                    }
                    IDC_ACTION_ICON_COLOR_BTN => {
                        if let Some(c) = pick_color(hwnd, &state.icon_color) {
                            state.icon_color = c;
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
                            let delay: f64 = get_text(state.edit_delay)
                                .parse::<f64>()
                                .unwrap_or(0.0)
                                .max(0.0);
                            let snd_idx =
                                SendMessageW(state.sound_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0))
                                    .0 as usize;
                            let sound = state.sound_options.get(snd_idx).and_then(|(k, _)| {
                                if k.is_empty() {
                                    None
                                } else {
                                    Some(k.clone())
                                }
                            });
                            Action::Overlay {
                                icon,
                                color,
                                message: get_text(state.edit_message),
                                message_color: state.msg_color.clone(),
                                delay_secs: delay,
                                sound,
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

    unsafe fn create_action_edit_controls(hwnd: HWND, state: &mut ActionEditState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        // Layout constants.
        let lx = 10i32;
        let lw = 110i32;
        let cx = lx + lw + 6; // start of input controls
        let color_btn_w = 26i32; // small "…" color picker button
        let right_margin = 10i32;
        // edit width: window client ≈ 460 − 2×frame. Use fixed right edge.
        let right_edge = 450i32;
        let edit_w = right_edge - cx - right_margin - 4 - color_btn_w; // ~300
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
        let type_idx = match &state.action {
            Action::Overlay { .. } => 0usize,
            Action::StoreVar { .. } => 1usize,
            Action::VoiceAlert { .. } => 2usize,
        };
        SendMessageW(state.type_combo, CB_SETCURSEL, WPARAM(type_idx), LPARAM(0));
        y += row;

        mk_separator(hwnd, hi, font, lx, y, right_edge);
        y += 12;

        // ── Overlay fields ────────────────────────────────────────────────
        let fields_y = y;

        // Icon row: [Icon:] [icon_combo] [◉ color btn — only active for colorbox]
        let il = mk_label(hwnd, hi, font, "Icon:", lx, y, lw, ch);
        state.icon_combo = mk_icon_combo(hwnd, hi, font, cx, y, edit_w, IDC_ACTION_ICON);
        for item in &state.icon_items {
            cb_add(state.icon_combo, &item.label);
        }
        let ico_key = match &state.action {
            Action::Overlay { icon, .. } => icon.as_str(),
            _ => "",
        };
        let ico_idx = find_icon_index(&state.icon_items, ico_key);
        SendMessageW(state.icon_combo, CB_SETCURSEL, WPARAM(ico_idx), LPARAM(0));
        let is_colorbox = state
            .icon_items
            .get(ico_idx)
            .map(|it| it.key == "colorbox")
            .unwrap_or(false);
        state.btn_icon_color = mk_button_ex(
            hwnd,
            hi,
            font,
            "\u{25A3}",
            cbx,
            y,
            color_btn_w,
            ch,
            IDC_ACTION_ICON_COLOR_BTN,
        );
        let _ = EnableWindow(state.btn_icon_color, BOOL(if is_colorbox { 1 } else { 0 }));
        state.overlay_controls.push(il);
        state.overlay_controls.push(state.icon_combo);
        state.overlay_controls.push(state.btn_icon_color);
        y += row;

        // Message row: [Message:] [message_edit] [◉ color btn]
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
        state.btn_msg_color = mk_button_ex(
            hwnd,
            hi,
            font,
            "\u{25A3}",
            cbx,
            y,
            color_btn_w,
            ch,
            IDC_ACTION_MSG_COLOR_BTN,
        );
        state.overlay_controls.push(ml);
        state.overlay_controls.push(state.edit_message);
        state.overlay_controls.push(state.btn_msg_color);
        y += row;

        // Sound row: [Sound:] [sound_combo]
        let sl = mk_label(hwnd, hi, font, "Sound:", lx, y, lw, ch);
        state.sound_combo = mk_combo(
            hwnd,
            hi,
            font,
            cx,
            y,
            edit_w + 4 + color_btn_w,
            IDC_ACTION_SOUND,
        );
        for (_, label) in &state.sound_options {
            cb_add(state.sound_combo, label);
        }
        let cur_sound = match &state.action {
            Action::Overlay { sound, .. } => sound.clone(),
            _ => None,
        };
        let snd_key = cur_sound.as_deref().unwrap_or("");
        let snd_idx = state
            .sound_options
            .iter()
            .position(|(k, _)| k == snd_key)
            .unwrap_or(0);
        SendMessageW(state.sound_combo, CB_SETCURSEL, WPARAM(snd_idx), LPARAM(0));
        state.overlay_controls.push(sl);
        state.overlay_controls.push(state.sound_combo);
        y += row;

        // Delay row: [Delay (sec):] [delay_edit]
        let dl = mk_label(hwnd, hi, font, "Delay (sec):", lx, y, lw, ch);
        let delay_s = match &state.action {
            Action::Overlay { delay_secs, .. } => {
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
        state.overlay_controls.push(dl);
        state.overlay_controls.push(state.edit_delay);
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
        for &h in &state.overlay_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_o);
        }
        for &h in &state.var_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_v);
        }
        for &h in &state.tts_controls {
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(h, show_t);
        }
    }

    // ── Control creation — main config window ─────────────────────────────────

    unsafe fn create_controls(hwnd: HWND, state: &mut ConfigState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        let win_w = 560i32;
        let win_h = 560i32;
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
        insert_tab(state.tab_hwnd, 0, "Triggers");
        insert_tab(state.tab_hwnd, 1, "Appearance");
        insert_tab(state.tab_hwnd, 2, "Voice");

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

    unsafe fn create_appearance_panel(
        state: &mut ConfigState,
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let ox = ta.left;
        let oy = ta.top;
        let lx = 4i32;
        let lw = 120i32;
        let cx = lx + lw + 4;
        let cw = 180i32;
        let ch = 22i32;
        let row = 30i32;
        let mut y = 8i32;

        state.chk_overlay_enabled = mk_checkbox(
            parent,
            hi,
            font,
            "Enable overlay window",
            ox + cx,
            oy + y,
            cw + 80,
            ch,
            IDC_OVERLAY_ENABLED,
        );
        state.appearance_controls.push(state.chk_overlay_enabled);
        if state.cfg.overlay_enabled {
            SendMessageW(
                state.chk_overlay_enabled,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row;

        state.appearance_controls.push(mk_label(
            parent,
            hi,
            font,
            "Font:",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.font_combo = mk_combo(parent, hi, font, ox + cx, oy + y, cw, IDC_FONT_COMBO);
        state.appearance_controls.push(state.font_combo);
        for name in FONT_NAMES {
            cb_add(state.font_combo, name);
        }
        let fi = FONT_NAMES
            .iter()
            .position(|&n| n == state.cfg.overlay_font)
            .unwrap_or(0);
        SendMessageW(state.font_combo, CB_SETCURSEL, WPARAM(fi), LPARAM(0));
        y += row;

        state.appearance_controls.push(mk_label(
            parent,
            hi,
            font,
            "Font size (pt):",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.edit_font_size = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_font_size.to_string(),
            ox + cx,
            oy + y,
            60,
            ch,
            IDC_FONT_SIZE,
        );
        state.appearance_controls.push(state.edit_font_size);
        y += row;

        state.appearance_controls.push(mk_label(
            parent,
            hi,
            font,
            "Opacity (0-255):",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.edit_alpha = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_alpha.to_string(),
            ox + cx,
            oy + y,
            60,
            ch,
            IDC_ALPHA_EDIT,
        );
        state.appearance_controls.push(state.edit_alpha);
        y += row;

        state.appearance_controls.push(mk_label(
            parent,
            hi,
            font,
            "Idle hide (secs):",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.edit_idle = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_idle_secs.to_string(),
            ox + cx,
            oy + y,
            60,
            ch,
            IDC_IDLE_EDIT,
        );
        state.appearance_controls.push(state.edit_idle);
        y += row;

        state.appearance_controls.push(mk_label(
            parent,
            hi,
            font,
            "Max entries:",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.edit_max_entries = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_max_entries.to_string(),
            ox + cx,
            oy + y,
            60,
            ch,
            IDC_MAX_ENTRIES,
        );
        state.appearance_controls.push(state.edit_max_entries);
        y += row;

        state.appearance_controls.push(mk_label(
            parent,
            hi,
            font,
            "Position X:",
            ox + lx,
            oy + y,
            lw,
            ch,
        ));
        state.edit_overlay_x = mk_edit(
            parent,
            hi,
            font,
            &state.cfg.overlay_x.to_string(),
            ox + cx,
            oy + y,
            70,
            ch,
            IDC_OVERLAY_X,
            0,
        );
        state.appearance_controls.push(state.edit_overlay_x);
        state.appearance_controls.push(mk_label(
            parent,
            hi,
            font,
            "Y:",
            ox + cx + 76,
            oy + y,
            20,
            ch,
        ));
        state.edit_overlay_y = mk_edit(
            parent,
            hi,
            font,
            &state.cfg.overlay_y.to_string(),
            ox + cx + 100,
            oy + y,
            70,
            ch,
            IDC_OVERLAY_Y,
            0,
        );
        state.appearance_controls.push(state.edit_overlay_y);
        state.appearance_controls.push(mk_label(
            parent,
            hi,
            font,
            "(−1 = auto-centre)",
            ox + cx + 176,
            oy + y,
            130,
            ch,
        ));
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

        mk_separator(parent, hi, font, ox + lx, oy + y, cw + lw + 8);
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

        mk_separator(parent, hi, font, ox + lx, oy + y, cw + lw + 8);
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

        mk_separator(parent, hi, font, ox + lx, oy + y, cw + lw + 8);
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
    ) {
        mk_child(parent, hi, font, "STATIC", "", x, y, w, 2, 0, SS_ETCHEDHORZ);
    }

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

    unsafe fn mk_icon_combo(
        parent: HWND,
        hi: HINSTANCE,
        font: windows::Win32::Graphics::Gdi::HGDIOBJ,
        x: i32,
        y: i32,
        w: i32,
        id: i32,
    ) -> HWND {
        let hwnd = mk_child(
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
            CBS_DROPDOWNLIST | CBS_HASSTRINGS | CBS_OWNERDRAWFIXED | WS_TABSTOP.0,
        );
        SendMessageW(
            hwnd,
            CB_SETITEMHEIGHT,
            WPARAM(0),
            LPARAM(ICON_ITEM_H as isize),
        );
        hwnd
    }

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
}

// ── Public re-export ──────────────────────────────────────────────────────────

#[cfg(feature = "tray")]
pub use overlay_config::open_overlay_config;
