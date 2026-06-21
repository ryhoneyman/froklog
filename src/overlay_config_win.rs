/// Overlay & Trigger configuration window.
///
/// Opened from the tray menu ("Overlay Settings…").
/// Split into two tabs via a Win32 TabControl:
///   Tab 0 — Triggers  : listbox of triggers + add/edit/delete/enable controls
///   Tab 1 — Appearance: font, size, transparency, idle-hide timeout
///
/// Trigger editing opens a child modal dialog (TriggerEditDialog).
#[cfg(feature = "tray")]
pub mod overlay_config {
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{GetStockObject, COLOR_BTNFACE, DEFAULT_GUI_FONT, HBRUSH};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
        IsDialogMessageW, LoadCursorW, MessageBoxW, PostQuitMessage, RegisterClassExW,
        SendMessageW, SetWindowLongPtrW, SetWindowTextW, TranslateMessage, CB_ADDSTRING,
        CB_GETCURSEL, CB_SETCURSEL, CREATESTRUCTW, GWLP_USERDATA, HMENU, IDC_ARROW, LB_ADDSTRING,
        LB_GETCURSEL, MB_ICONWARNING, MB_OK, MB_YESNO, MESSAGEBOX_STYLE, MSG, SM_CXSCREEN,
        SM_CYSCREEN, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
        WM_NOTIFY, WM_SETFONT, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW,
        WS_EX_DLGMODALFRAME, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    };

    use crate::config::Config;
    use crate::tray::tray::AppHandle;
    use crate::triggers::engine::{ChainStepDef, TriggerConfig, TriggerDef};

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

    // Bottom buttons
    const IDC_SAVE: i32 = 220;
    const IDC_CANCEL: i32 = 221;

    // Trigger edit dialog
    const IDC_EDIT_NAME: i32 = 300;
    const IDC_EDIT_PATTERN: i32 = 301;
    const IDC_EDIT_ICON: i32 = 302;
    const IDC_EDIT_ICON_BROWSE: i32 = 303;
    const IDC_EDIT_MESSAGE: i32 = 304;
    const IDC_EDIT_SOUND: i32 = 305;
    const IDC_EDIT_SOUND_BROWSE: i32 = 306;
    const IDC_EDIT_ENABLED: i32 = 307;
    const IDC_EDIT_CHAINED: i32 = 308;
    const IDC_STEP_LIST: i32 = 309;
    const IDC_STEP_ADD: i32 = 310;
    const IDC_STEP_EDIT: i32 = 311;
    const IDC_STEP_DELETE: i32 = 312;
    const IDC_EDIT_OK: i32 = 313;
    const IDC_EDIT_CANCEL: i32 = 314;

    // Step edit dialog
    const IDC_STEP_TYPE: i32 = 400;
    const IDC_STEP_PATTERN: i32 = 401;
    const IDC_STEP_DELAY: i32 = 402;
    const IDC_STEP_ICON: i32 = 403;
    const IDC_STEP_ICON_BROWSE: i32 = 404;
    const IDC_STEP_MESSAGE: i32 = 405;
    const IDC_STEP_SOUND: i32 = 406;
    const IDC_STEP_SOUND_BROWSE: i32 = 407;
    const IDC_STEP_OK: i32 = 408;
    const IDC_STEP_CANCEL: i32 = 409;

    // Win32 control style / message constants (defined locally as plain u32
    // to avoid newtype-wrapping issues in the windows crate).
    const SS_LEFT: u32 = 0x0000_0000;
    const SS_RIGHT: u32 = 0x0000_0002;
    const SS_ETCHEDHORZ: u32 = 0x0000_0010;
    const BS_PUSHBUTTON: u32 = 0x0000_0000;
    const BS_DEFPUSHBUTTON: u32 = 0x0000_0001;
    const BS_AUTOCHECKBOX: u32 = 0x0000_0003;
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
    // Tab control messages (TCM_FIRST = 0x1300).
    const TCM_INSERTITEMW: u32 = 0x133E;
    const TCM_GETCURSEL: u32 = 0x130B;
    const TCM_SETCURSEL: u32 = 0x130C;
    const TCM_ADJUSTRECT: u32 = 0x1328;
    const LB_SETCURSEL: u32 = 0x0186;
    const LB_RESETCONTENT: u32 = 0x0184;

    const CLASS_OVERLAY_CFG: &str = "FroklogOverlayCfg\0";
    const CLASS_TRIGGER_EDIT: &str = "FroklogTriggerEdit\0";
    const CLASS_STEP_EDIT: &str = "FroklogStepEdit\0";

    // ── Common font names for the combo ───────────────────────────────────────

    const FONT_NAMES: &[&str] = &[
        "Segoe UI",
        "Arial",
        "Consolas",
        "Courier New",
        "Tahoma",
        "Verdana",
        "Times New Roman",
    ];

    // ── Step type labels ──────────────────────────────────────────────────────

    const STEP_TYPES: &[&str] = &[
        "Match (start pattern)",
        "Delay (timer)",
        "Complete (pattern)",
        "Cancel (pattern)",
    ];

    // ── Main dialog state ─────────────────────────────────────────────────────

    struct ConfigState {
        handle: Arc<AppHandle>,
        triggers: TriggerConfig,
        cfg: Config,
        // Controls
        tab_hwnd: HWND,
        trigger_list: HWND,
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
        // Tab panels
        triggers_panel: HWND,
        appearance_panel: HWND,
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
        let cfg = handle.config.lock().unwrap().clone();
        let triggers = TriggerConfig::load();

        let state = Box::new(ConfigState {
            handle: Arc::clone(&handle),
            triggers,
            cfg,
            tab_hwnd: HWND::default(),
            trigger_list: HWND::default(),
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
            triggers_panel: HWND::default(),
            appearance_panel: HWND::default(),
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
            let h = 560i32;
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

        // Ensure the overlay_config_open flag is cleared.
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
                // Tab switch.
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
                let new_def = TriggerDef::Simple {
                    name: "New Trigger".to_string(),
                    enabled: true,
                    pattern: String::new(),
                    icon: String::new(),
                    message: String::new(),
                    sound: None,
                };
                let idx = state.triggers.triggers.len();
                state.triggers.triggers.push(new_def);
                rebuild_trigger_list(state);
                // Select the new entry and open editor.
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
                let name = state.triggers.triggers[sel as usize].name().to_string();
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
                t.set_enabled(!t.enabled());
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
        // Collect appearance fields.
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
        let idle: u32 = get_text(state.edit_idle).parse().unwrap_or(6).max(1);
        let max_entries: usize = get_text(state.edit_max_entries)
            .parse()
            .unwrap_or(8)
            .max(1)
            .min(20);
        let overlay_enabled =
            SendMessageW(state.chk_overlay_enabled, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                == BST_CHECKED;

        // Update live config.
        {
            let mut cfg = state.handle.config.lock().unwrap();
            cfg.overlay_font = font_name;
            cfg.overlay_font_size = font_size;
            cfg.overlay_alpha = alpha;
            cfg.overlay_idle_secs = idle;
            cfg.overlay_max_entries = max_entries;
            cfg.overlay_enabled = overlay_enabled;
            cfg.save();
        }

        // Save triggers.
        state.triggers.save();

        // Signal the engine to reload.
        if let Some(engine) = state.handle.trigger_engine.lock().unwrap().as_ref() {
            engine.reload(&state.triggers);
        }

        // Restart engine so overlay picks up new settings.
        state.handle.restart.store(true, Ordering::Relaxed);

        let _ = DestroyWindow(hwnd);
    }

    // ── Tab switching ─────────────────────────────────────────────────────────

    unsafe fn switch_tab(state: &mut ConfigState, tab: i32) {
        state.current_tab = tab;
        windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.triggers_panel,
            if tab == 0 {
                windows::Win32::UI::WindowsAndMessaging::SW_SHOW
            } else {
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE
            },
        );
        windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.appearance_panel,
            if tab == 1 {
                windows::Win32::UI::WindowsAndMessaging::SW_SHOW
            } else {
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE
            },
        );
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
            let label = format!("[{}] {}", if t.enabled() { "✓" } else { " " }, t.name());
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
        let edited = open_trigger_editor(parent, original);
        if let Some(def) = edited {
            state.triggers.triggers[sel as usize] = def;
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
        edit_pattern: HWND,
        edit_icon: HWND,
        edit_message: HWND,
        edit_sound: HWND,
        chk_enabled: HWND,
        chk_chained: HWND,
        step_list: HWND,
        btn_step_add: HWND,
        btn_step_edit: HWND,
        btn_step_delete: HWND,
        // Simple-mode controls (hidden when chained).
        simple_group: HWND,
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
            edit_pattern: HWND::default(),
            edit_icon: HWND::default(),
            edit_message: HWND::default(),
            edit_sound: HWND::default(),
            chk_enabled: HWND::default(),
            chk_chained: HWND::default(),
            step_list: HWND::default(),
            btn_step_add: HWND::default(),
            btn_step_edit: HWND::default(),
            btn_step_delete: HWND::default(),
            simple_group: HWND::default(),
        });
        let state_ptr = Box::into_raw(state);

        let w = 480i32;
        let h = 500i32;
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

        // Modal-ish: pump messages until the window is destroyed.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &mut msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Recover result from heap-allocated state (already dropped in WM_DESTROY).
        // We stash the result in a thread-local before dropping.
        TRIGGER_EDIT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static TRIGGER_EDIT_RESULT: std::cell::RefCell<Option<TriggerDef>> = std::cell::RefCell::new(None);
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
                // Store result before drop.
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
            IDC_EDIT_CHAINED => {
                // Toggle between simple and chained mode.
                let is_chained = SendMessageW(state.chk_chained, BM_GETCHECK, WPARAM(0), LPARAM(0))
                    .0 as usize
                    == BST_CHECKED;
                update_chained_visibility(state, is_chained);
            }

            IDC_EDIT_ICON_BROWSE => {
                if let Some(p) = pick_file("Image files\0*.png;*.ico;*.bmp\0\0") {
                    set_wnd_text(state.edit_icon, &p);
                }
            }

            IDC_EDIT_SOUND_BROWSE => {
                if let Some(p) = pick_file("Sound files\0*.wav;*.mp3\0\0") {
                    set_wnd_text(state.edit_sound, &p);
                }
            }

            IDC_STEP_ADD => {
                // Open step editor with a blank Delay step.
                let blank = ChainStepDef::Delay {
                    delay_secs: 5.0,
                    icon: String::new(),
                    message: String::new(),
                    sound: None,
                };
                if let Some(step) = open_step_editor(hwnd, blank) {
                    // Add to step list in the current def.
                    push_step_to_def(state, step);
                    rebuild_step_list(state);
                }
            }

            IDC_STEP_EDIT => {
                let sel =
                    SendMessageW(state.step_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel < 0 {
                    return;
                }
                if let Some(step) = get_step_at(state, sel as usize) {
                    if let Some(edited) = open_step_editor(hwnd, step) {
                        set_step_at(state, sel as usize, edited);
                        rebuild_step_list(state);
                    }
                }
            }

            IDC_STEP_DELETE => {
                let sel =
                    SendMessageW(state.step_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel >= 0 {
                    delete_step_at(state, sel as usize);
                    rebuild_step_list(state);
                }
            }

            IDC_STEP_LIST if notif == LBN_DBLCLK => {
                let sel =
                    SendMessageW(state.step_list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel >= 0 {
                    if let Some(step) = get_step_at(state, sel as usize) {
                        if let Some(edited) = open_step_editor(hwnd, step) {
                            set_step_at(state, sel as usize, edited);
                            rebuild_step_list(state);
                        }
                    }
                }
            }

            IDC_EDIT_OK => {
                // Collect the edited trigger and store as result.
                let name = get_text(state.edit_name);
                let enabled = SendMessageW(state.chk_enabled, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
                    as usize
                    == BST_CHECKED;
                let is_chained = SendMessageW(state.chk_chained, BM_GETCHECK, WPARAM(0), LPARAM(0))
                    .0 as usize
                    == BST_CHECKED;

                let def = if is_chained {
                    // Reconstruct from the def's step list.
                    let steps = match &state.def {
                        TriggerDef::Chained { steps, .. } => steps.clone(),
                        _ => Vec::new(),
                    };
                    TriggerDef::Chained {
                        name,
                        enabled,
                        steps,
                    }
                } else {
                    let pattern = get_text(state.edit_pattern);
                    let icon = get_text(state.edit_icon);
                    let message = get_text(state.edit_message);
                    let sound_s = get_text(state.edit_sound);
                    let sound = if sound_s.is_empty() {
                        None
                    } else {
                        Some(sound_s)
                    };
                    TriggerDef::Simple {
                        name,
                        enabled,
                        pattern,
                        icon,
                        message,
                        sound,
                    }
                };

                state.result = Some(def);
                let _ = DestroyWindow(hwnd);
            }

            IDC_EDIT_CANCEL => {
                let _ = DestroyWindow(hwnd);
            }

            _ => {}
        }
    }

    // ── Step list helpers ─────────────────────────────────────────────────────

    unsafe fn rebuild_step_list(state: &TriggerEditState) {
        SendMessageW(
            state.step_list,
            windows::Win32::UI::WindowsAndMessaging::LB_RESETCONTENT,
            WPARAM(0),
            LPARAM(0),
        );
        if let TriggerDef::Chained { steps, .. } = &state.def {
            for step in steps {
                let label = step_label(step);
                let lw = wide(&label);
                SendMessageW(
                    state.step_list,
                    LB_ADDSTRING,
                    WPARAM(0),
                    LPARAM(lw.as_ptr() as isize),
                );
            }
        }
    }

    fn step_label(step: &ChainStepDef) -> String {
        match step {
            ChainStepDef::Match {
                pattern, message, ..
            } => {
                format!("[Match] {} → {}", pattern, message)
            }
            ChainStepDef::Delay {
                delay_secs,
                message,
                ..
            } => {
                format!("[Delay {:.1}s] {}", delay_secs, message)
            }
            ChainStepDef::Complete {
                complete, message, ..
            } => {
                format!("[Complete] {} → {}", complete, message)
            }
            ChainStepDef::Cancel { cancel } => {
                format!("[Cancel] {}", cancel)
            }
        }
    }

    fn get_step_at(state: &TriggerEditState, idx: usize) -> Option<ChainStepDef> {
        match &state.def {
            TriggerDef::Chained { steps, .. } => steps.get(idx).cloned(),
            _ => None,
        }
    }

    fn set_step_at(state: &mut TriggerEditState, idx: usize, step: ChainStepDef) {
        if let TriggerDef::Chained { steps, .. } = &mut state.def {
            if idx < steps.len() {
                steps[idx] = step;
            }
        }
    }

    fn delete_step_at(state: &mut TriggerEditState, idx: usize) {
        if let TriggerDef::Chained { steps, .. } = &mut state.def {
            if idx < steps.len() {
                steps.remove(idx);
            }
        }
    }

    fn push_step_to_def(state: &mut TriggerEditState, step: ChainStepDef) {
        // If currently a Simple, convert to Chained with the step as the only additional step.
        match &mut state.def {
            TriggerDef::Chained { steps, .. } => steps.push(step),
            TriggerDef::Simple {
                name,
                enabled,
                pattern,
                icon,
                message,
                sound,
            } => {
                let start = ChainStepDef::Match {
                    pattern: pattern.clone(),
                    icon: icon.clone(),
                    message: message.clone(),
                    sound: sound.clone(),
                };
                let new_def = TriggerDef::Chained {
                    name: name.clone(),
                    enabled: *enabled,
                    steps: vec![start, step],
                };
                state.def = new_def;
            }
        }
    }

    unsafe fn update_chained_visibility(state: &TriggerEditState, is_chained: bool) {
        // Show/hide simple-only controls.
        let show_simple = if is_chained {
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE
        } else {
            windows::Win32::UI::WindowsAndMessaging::SW_SHOW
        };
        let show_chained = if is_chained {
            windows::Win32::UI::WindowsAndMessaging::SW_SHOW
        } else {
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE
        };
        windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.simple_group, show_simple);
        windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.step_list, show_chained);
        windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_step_add, show_chained);
        windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_step_edit, show_chained);
        windows::Win32::UI::WindowsAndMessaging::ShowWindow(state.btn_step_delete, show_chained);
    }

    // ── Step editor dialog ────────────────────────────────────────────────────

    struct StepEditState {
        step: ChainStepDef,
        result: Option<ChainStepDef>,
        type_combo: HWND,
        edit_pattern: HWND,
        edit_delay: HWND,
        edit_icon: HWND,
        edit_message: HWND,
        edit_sound: HWND,
    }

    unsafe fn open_step_editor(parent: HWND, step: ChainStepDef) -> Option<ChainStepDef> {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hinstance = HINSTANCE(hmodule.0);
        let class_w: Vec<u16> = CLASS_STEP_EDIT.encode_utf16().collect();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(step_edit_proc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_w.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 as usize + 1) as *mut c_void),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let state = Box::new(StepEditState {
            step,
            result: None,
            type_combo: HWND::default(),
            edit_pattern: HWND::default(),
            edit_delay: HWND::default(),
            edit_icon: HWND::default(),
            edit_message: HWND::default(),
            edit_sound: HWND::default(),
        });
        let state_ptr = Box::into_raw(state);

        let w = 420i32;
        let h = 300i32;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let title = wide("Edit Step");
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
        .expect("CreateWindowExW step edit");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &mut msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        STEP_EDIT_RESULT.with(|cell| cell.borrow_mut().take())
    }

    thread_local! {
        static STEP_EDIT_RESULT: std::cell::RefCell<Option<ChainStepDef>> = std::cell::RefCell::new(None);
    }

    unsafe extern "system" fn step_edit_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut StepEditState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let state = &mut *ptr;
            create_step_edit_controls(hwnd, state);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut StepEditState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                handle_step_edit_command(hwnd, state, id);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(result) = state.result.take() {
                    STEP_EDIT_RESULT.with(|cell| {
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

    unsafe fn handle_step_edit_command(hwnd: HWND, state: &mut StepEditState, id: i32) {
        match id {
            IDC_STEP_ICON_BROWSE => {
                if let Some(p) = pick_file("Image files\0*.png;*.ico;*.bmp\0\0") {
                    set_wnd_text(state.edit_icon, &p);
                }
            }
            IDC_STEP_SOUND_BROWSE => {
                if let Some(p) = pick_file("Sound files\0*.wav;*.mp3\0\0") {
                    set_wnd_text(state.edit_sound, &p);
                }
            }
            IDC_STEP_OK => {
                let type_idx =
                    SendMessageW(state.type_combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
                let icon = get_text(state.edit_icon);
                let message = get_text(state.edit_message);
                let sound_s = get_text(state.edit_sound);
                let sound = if sound_s.is_empty() {
                    None
                } else {
                    Some(sound_s)
                };
                let pattern = get_text(state.edit_pattern);
                let delay: f64 = get_text(state.edit_delay).parse().unwrap_or(5.0);

                let step = match type_idx {
                    0 => ChainStepDef::Match {
                        pattern,
                        icon,
                        message,
                        sound,
                    },
                    1 => ChainStepDef::Delay {
                        delay_secs: delay,
                        icon,
                        message,
                        sound,
                    },
                    2 => ChainStepDef::Complete {
                        complete: pattern,
                        icon,
                        message,
                        sound,
                    },
                    _ => ChainStepDef::Cancel { cancel: pattern },
                };
                state.result = Some(step);
                let _ = DestroyWindow(hwnd);
            }
            IDC_STEP_CANCEL => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
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

        // Tab control.
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

        // Get the tab display area (inset from the tab control).
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

        // Triggers panel.
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
        create_triggers_panel(state.triggers_panel, hi, font, &state.triggers, ta);
        // Store sub-control handles back into state.
        // We need to look them up by ID since we called create_triggers_panel with a sub-parent.
        state.trigger_list = GetDlgItem(state.triggers_panel, IDC_TRIGGER_LIST).unwrap_or_default();
        state.btn_edit = GetDlgItem(state.triggers_panel, IDC_BTN_EDIT).unwrap_or_default();
        state.btn_delete = GetDlgItem(state.triggers_panel, IDC_BTN_DELETE).unwrap_or_default();
        state.btn_move_up = GetDlgItem(state.triggers_panel, IDC_BTN_MOVE_UP).unwrap_or_default();
        state.btn_move_down =
            GetDlgItem(state.triggers_panel, IDC_BTN_MOVE_DOWN).unwrap_or_default();
        state.btn_toggle = GetDlgItem(state.triggers_panel, IDC_BTN_TOGGLE).unwrap_or_default();
        rebuild_trigger_list(state);
        refresh_trigger_buttons(state);

        // Appearance panel (hidden initially).
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
        create_appearance_panel(state, state.appearance_panel, hi, font, ta);
        windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.appearance_panel,
            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
        );

        // Save / Cancel buttons at bottom.
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
        triggers: &TriggerConfig,
        ta: windows::Win32::Foundation::RECT,
    ) {
        let pw = ta.right - ta.left;
        let ph = ta.bottom - ta.top;
        let btn_w = 90i32;
        let btn_h = 24i32;
        let bx = pw - btn_w - 4;
        let list_w = bx - 8;
        let list_h = ph - 8;

        // Listbox.
        mk_child(
            parent,
            hi,
            font,
            "LISTBOX",
            "",
            4,
            4,
            list_w,
            list_h,
            IDC_TRIGGER_LIST,
            LBS_NOTIFY | LBS_HASSTRINGS | WS_VSCROLL_VAL | WS_BORDER.0 | WS_TABSTOP.0,
        );

        // Buttons on the right.
        let mut by = 4i32;
        let gap = btn_h + 4;
        mk_button_ex(parent, hi, font, "Add", bx, by, btn_w, btn_h, IDC_BTN_ADD);
        by += gap;
        mk_button_ex(parent, hi, font, "Edit", bx, by, btn_w, btn_h, IDC_BTN_EDIT);
        by += gap;
        mk_button_ex(
            parent,
            hi,
            font,
            "Delete",
            bx,
            by,
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
            bx,
            by,
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
            bx,
            by,
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
            bx,
            by,
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
        let lx = 4i32;
        let lw = 120i32;
        let cx = lx + lw + 4;
        let cw = 180i32;
        let ch = 22i32;
        let row = 30i32;
        let mut y = 8i32;

        // Overlay enabled checkbox.
        state.chk_overlay_enabled = mk_checkbox(
            parent,
            hi,
            font,
            "Enable overlay window",
            cx,
            y,
            cw + 80,
            ch,
            IDC_OVERLAY_ENABLED,
        );
        if state.cfg.overlay_enabled {
            SendMessageW(
                state.chk_overlay_enabled,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row;

        mk_label(parent, hi, font, "Font:", lx, y, lw, ch);
        state.font_combo = mk_combo(parent, hi, font, cx, y, cw, IDC_FONT_COMBO);
        for name in FONT_NAMES {
            cb_add(state.font_combo, name);
        }
        let fi = FONT_NAMES
            .iter()
            .position(|&n| n == state.cfg.overlay_font)
            .unwrap_or(0);
        SendMessageW(state.font_combo, CB_SETCURSEL, WPARAM(fi), LPARAM(0));
        y += row;

        mk_label(parent, hi, font, "Font size (pt):", lx, y, lw, ch);
        state.edit_font_size = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_font_size.to_string(),
            cx,
            y,
            60,
            ch,
            IDC_FONT_SIZE,
        );
        y += row;

        mk_label(parent, hi, font, "Opacity (0-255):", lx, y, lw, ch);
        state.edit_alpha = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_alpha.to_string(),
            cx,
            y,
            60,
            ch,
            IDC_ALPHA_EDIT,
        );
        y += row;

        mk_label(parent, hi, font, "Idle hide (secs):", lx, y, lw, ch);
        state.edit_idle = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_idle_secs.to_string(),
            cx,
            y,
            60,
            ch,
            IDC_IDLE_EDIT,
        );
        y += row;

        mk_label(parent, hi, font, "Max entries:", lx, y, lw, ch);
        state.edit_max_entries = mk_edit_num(
            parent,
            hi,
            font,
            &state.cfg.overlay_max_entries.to_string(),
            cx,
            y,
            60,
            ch,
            IDC_MAX_ENTRIES,
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
        let cw = 260i32;
        let cw2 = 180i32;
        let bx = cx + cw2 + 4;
        let bw = 80i32;
        let ch = 22i32;
        let row = 30i32;
        let mut y = 10i32;

        let (name, enabled, is_chained) = match &state.def {
            TriggerDef::Simple { name, enabled, .. } => (name.as_str(), *enabled, false),
            TriggerDef::Chained { name, enabled, .. } => (name.as_str(), *enabled, true),
        };

        // Name.
        mk_label(hwnd, hi, font, "Name:", lx, y, lw, ch);
        state.edit_name = mk_edit(hwnd, hi, font, name, cx, y, cw, ch, IDC_EDIT_NAME, 0);
        y += row;

        // Enabled.
        state.chk_enabled =
            mk_checkbox(hwnd, hi, font, "Enabled", cx, y, 100, ch, IDC_EDIT_ENABLED);
        if enabled {
            SendMessageW(
                state.chk_enabled,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        // Chained toggle.
        state.chk_chained = mk_checkbox(
            hwnd,
            hi,
            font,
            "Multi-step (chained)",
            cx + 110,
            y,
            160,
            ch,
            IDC_EDIT_CHAINED,
        );
        if is_chained {
            SendMessageW(
                state.chk_chained,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row;

        mk_separator(hwnd, hi, font, lx, y, bx + bw - lx);
        y += 14;

        // Simple-mode fields (hidden when chained).
        // We wrap them in a static "group" container so we can show/hide together.
        let group_h = row * 4;
        state.simple_group = mk_child(
            hwnd,
            hi,
            font,
            "STATIC",
            "",
            lx,
            y,
            bx + bw - lx,
            group_h,
            0,
            0,
        );

        // Pattern.
        mk_label(
            state.simple_group,
            hi,
            font,
            "Pattern (regex):",
            0,
            0,
            lw,
            ch,
        );
        let (pat, ico, msg, snd) = match &state.def {
            TriggerDef::Simple {
                pattern,
                icon,
                message,
                sound,
                ..
            } => (
                pattern.as_str(),
                icon.as_str(),
                message.as_str(),
                sound.as_deref().unwrap_or(""),
            ),
            TriggerDef::Chained { steps, .. } => {
                if let Some(ChainStepDef::Match {
                    pattern,
                    icon,
                    message,
                    sound,
                }) = steps.first()
                {
                    (
                        pattern.as_str(),
                        icon.as_str(),
                        message.as_str(),
                        sound.as_deref().unwrap_or(""),
                    )
                } else {
                    ("", "", "", "")
                }
            }
        };
        state.edit_pattern = mk_edit(
            state.simple_group,
            hi,
            font,
            pat,
            cx - lx,
            0,
            cw,
            ch,
            IDC_EDIT_PATTERN,
            0,
        );
        {
            let hint = wide("e.g. You have slain (.+)!");
            SendMessageW(
                state.edit_pattern,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }

        // Icon.
        mk_label(state.simple_group, hi, font, "Icon:", 0, row, lw, ch);
        state.edit_icon = mk_edit(
            state.simple_group,
            hi,
            font,
            ico,
            cx - lx,
            row,
            cw2,
            ch,
            IDC_EDIT_ICON,
            0,
        );
        mk_button_ex(
            state.simple_group,
            hi,
            font,
            "Browse…",
            bx - lx,
            row,
            bw,
            ch,
            IDC_EDIT_ICON_BROWSE,
        );
        {
            let hint = wide("heal / damage / path\\to\\icon.png");
            SendMessageW(
                state.edit_icon,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }

        // Message.
        mk_label(state.simple_group, hi, font, "Message:", 0, row * 2, lw, ch);
        state.edit_message = mk_edit(
            state.simple_group,
            hi,
            font,
            msg,
            cx - lx,
            row * 2,
            cw,
            ch,
            IDC_EDIT_MESSAGE,
            0,
        );
        {
            let hint = wide("Slain: {1}  (use {1},{2}… for capture groups)");
            SendMessageW(
                state.edit_message,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(hint.as_ptr() as isize),
            );
        }

        // Sound.
        mk_label(state.simple_group, hi, font, "Sound:", 0, row * 3, lw, ch);
        state.edit_sound = mk_edit(
            state.simple_group,
            hi,
            font,
            snd,
            cx - lx,
            row * 3,
            cw2,
            ch,
            IDC_EDIT_SOUND,
            0,
        );
        mk_button_ex(
            state.simple_group,
            hi,
            font,
            "Browse…",
            bx - lx,
            row * 3,
            bw,
            ch,
            IDC_EDIT_SOUND_BROWSE,
        );

        y += group_h + 8;

        // Chained step list.
        let list_h = 120i32;
        state.step_list = mk_child(
            hwnd,
            hi,
            font,
            "LISTBOX",
            "",
            lx,
            y,
            bx - lx - 4,
            list_h,
            IDC_STEP_LIST,
            LBS_NOTIFY | LBS_HASSTRINGS | WS_VSCROLL_VAL | WS_BORDER.0 | WS_TABSTOP.0,
        );
        state.btn_step_add = mk_button_ex(hwnd, hi, font, "Add Step", bx, y, bw, ch, IDC_STEP_ADD);
        state.btn_step_edit = mk_button_ex(
            hwnd,
            hi,
            font,
            "Edit Step",
            bx,
            y + row,
            bw,
            ch,
            IDC_STEP_EDIT,
        );
        state.btn_step_delete = mk_button_ex(
            hwnd,
            hi,
            font,
            "Delete Step",
            bx,
            y + row * 2,
            bw,
            ch,
            IDC_STEP_DELETE,
        );
        y += list_h + 8;

        // Populate steps if chained.
        rebuild_step_list(state);
        update_chained_visibility(state, is_chained);

        // OK / Cancel.
        let bw2 = 80i32;
        let right = bx + bw;
        mk_button_ex(
            hwnd,
            hi,
            font,
            "Cancel",
            right - bw2,
            y,
            bw2,
            ch,
            IDC_EDIT_CANCEL,
        );
        mk_default_button(
            hwnd,
            hi,
            font,
            "OK",
            right - bw2 * 2 - 8,
            y,
            bw2,
            ch,
            IDC_EDIT_OK,
        );
    }

    // ── Step edit controls ────────────────────────────────────────────────────

    unsafe fn create_step_edit_controls(hwnd: HWND, state: &mut StepEditState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        let lx = 10i32;
        let lw = 90i32;
        let cx = lx + lw + 6;
        let cw = 220i32;
        let cw2 = 150i32;
        let bx = cx + cw2 + 4;
        let bw = 80i32;
        let ch = 22i32;
        let row = 30i32;
        let mut y = 10i32;

        // Step type.
        mk_label(hwnd, hi, font, "Step type:", lx, y, lw, ch);
        state.type_combo = mk_combo(hwnd, hi, font, cx, y, cw, IDC_STEP_TYPE);
        for t in STEP_TYPES {
            cb_add(state.type_combo, t);
        }
        let type_idx = match &state.step {
            ChainStepDef::Match { .. } => 0,
            ChainStepDef::Delay { .. } => 1,
            ChainStepDef::Complete { .. } => 2,
            ChainStepDef::Cancel { .. } => 3,
        };
        SendMessageW(state.type_combo, CB_SETCURSEL, WPARAM(type_idx), LPARAM(0));
        y += row;

        // Pattern / delay depending on type.
        mk_label(hwnd, hi, font, "Pattern/regex:", lx, y, lw, ch);
        let pat = match &state.step {
            ChainStepDef::Match { pattern, .. }
            | ChainStepDef::Complete {
                complete: pattern, ..
            }
            | ChainStepDef::Cancel { cancel: pattern } => pattern.as_str(),
            ChainStepDef::Delay { .. } => "",
        };
        state.edit_pattern = mk_edit(hwnd, hi, font, pat, cx, y, cw, ch, IDC_STEP_PATTERN, 0);
        y += row;

        mk_label(hwnd, hi, font, "Delay (secs):", lx, y, lw, ch);
        let delay_str = match &state.step {
            ChainStepDef::Delay { delay_secs, .. } => format!("{}", delay_secs),
            _ => String::new(),
        };
        state.edit_delay = mk_edit(hwnd, hi, font, &delay_str, cx, y, 80, ch, IDC_STEP_DELAY, 0);
        y += row;

        mk_label(hwnd, hi, font, "Icon:", lx, y, lw, ch);
        let ico = match &state.step {
            ChainStepDef::Match { icon, .. }
            | ChainStepDef::Delay { icon, .. }
            | ChainStepDef::Complete { icon, .. } => icon.as_str(),
            ChainStepDef::Cancel { .. } => "",
        };
        state.edit_icon = mk_edit(hwnd, hi, font, ico, cx, y, cw2, ch, IDC_STEP_ICON, 0);
        mk_button_ex(
            hwnd,
            hi,
            font,
            "Browse…",
            bx,
            y,
            bw,
            ch,
            IDC_STEP_ICON_BROWSE,
        );
        y += row;

        mk_label(hwnd, hi, font, "Message:", lx, y, lw, ch);
        let msg = match &state.step {
            ChainStepDef::Match { message, .. }
            | ChainStepDef::Delay { message, .. }
            | ChainStepDef::Complete { message, .. } => message.as_str(),
            ChainStepDef::Cancel { .. } => "",
        };
        state.edit_message = mk_edit(hwnd, hi, font, msg, cx, y, cw, ch, IDC_STEP_MESSAGE, 0);
        y += row;

        mk_label(hwnd, hi, font, "Sound:", lx, y, lw, ch);
        let snd = match &state.step {
            ChainStepDef::Match { sound, .. }
            | ChainStepDef::Delay { sound, .. }
            | ChainStepDef::Complete { sound, .. } => sound.as_deref().unwrap_or(""),
            ChainStepDef::Cancel { .. } => "",
        };
        state.edit_sound = mk_edit(hwnd, hi, font, snd, cx, y, cw2, ch, IDC_STEP_SOUND, 0);
        mk_button_ex(
            hwnd,
            hi,
            font,
            "Browse…",
            bx,
            y,
            bw,
            ch,
            IDC_STEP_SOUND_BROWSE,
        );
        y += row + 8;

        let right = bx + bw;
        let bw2 = 80i32;
        mk_button_ex(
            hwnd,
            hi,
            font,
            "Cancel",
            right - bw2,
            y,
            bw2,
            ch,
            IDC_STEP_CANCEL,
        );
        mk_default_button(
            hwnd,
            hi,
            font,
            "OK",
            right - bw2 * 2 - 8,
            y,
            bw2,
            ch,
            IDC_STEP_OK,
        );
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
        mk_child(parent, hi, font, "STATIC", text, x, y, w, h, 0, SS_RIGHT)
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

    // Tab insert helper.
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

    // Combo add.
    unsafe fn cb_add(hwnd: HWND, text: &str) {
        let w = wide(text);
        SendMessageW(hwnd, CB_ADDSTRING, WPARAM(0), LPARAM(w.as_ptr() as isize));
    }

    // Text helpers.
    unsafe fn get_text(hwnd: HWND) -> String {
        let len = GetWindowTextLengthW(hwnd) as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..len])
    }

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

    fn pick_file(filter: &str) -> Option<String> {
        let filter_w: Vec<u16> = filter.encode_utf16().collect();
        let mut buf = vec![0u16; 1024];
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            lpstrFilter: PCWSTR(filter_w.as_ptr()),
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
}

// ── Public re-export ──────────────────────────────────────────────────────────

#[cfg(feature = "tray")]
pub use overlay_config::open_overlay_config;
