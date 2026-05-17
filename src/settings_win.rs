/// Native Win32 settings dialog for the froklog tray application.
///
/// Only compiled when the `tray` feature is active.
/// On Windows the full dialog runs on a dedicated thread with its own message loop.
/// On non-Windows platforms (dev builds) open_settings is a no-op stub.

// ── Windows implementation ────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        GetStockObject, COLOR_BTNFACE, DEFAULT_GUI_FONT, HBRUSH, HGDIOBJ,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, LoadCursorW,
        MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW,
        SetWindowLongPtrW, SetWindowTextW, TranslateMessage, CB_ADDSTRING, CB_FINDSTRINGEXACT,
        CB_GETCURSEL, CB_GETLBTEXT, CB_GETLBTEXTLEN, CB_SETCURSEL, CREATESTRUCTW, GWLP_USERDATA,
        HMENU, IDC_ARROW, MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_YESNO, MESSAGEBOX_STYLE, MSG,
        SM_CXSCREEN, SM_CYSCREEN, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
        WM_CREATE, WM_DESTROY, WM_SETFONT, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD,
        WS_EX_APPWINDOW, WS_EX_DLGMODALFRAME, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    };

    use crate::tray::tray::AppHandle;

    // ── Control IDs ───────────────────────────────────────────────────────────

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
    const IDC_SAVE_BTN: i32 = 113;
    const IDC_CANCEL_BTN: i32 = 114;

    // Async result messages posted from background threads.
    const WM_URL_TEST_DONE: u32 = WM_APP + 1;
    const WM_REGISTER_DONE: u32 = WM_APP + 2;

    // Win32 control style constants (from WinUser.h / CommCtrl.h).
    const SS_LEFT: u32 = 0x0000_0000;
    const SS_RIGHT: u32 = 0x0000_0002;
    const BS_PUSHBUTTON: u32 = 0x0000_0000;
    const BS_AUTOCHECKBOX: u32 = 0x0000_0003;
    const CBS_DROPDOWNLIST: u32 = 0x0000_0003;
    const CBS_HASSTRINGS: u32 = 0x0000_0200;
    const ES_AUTOHSCROLL: u32 = 0x0000_0080;
    const ES_READONLY: u32 = 0x0000_0800;
    const BM_SETCHECK: u32 = 0x00F1;
    const BM_GETCHECK: u32 = 0x00F0;
    const BST_CHECKED: usize = 1;
    const CBN_SELCHANGE: usize = 1;
    const EN_CHANGE: usize = 0x0300;

    // ── Static data ───────────────────────────────────────────────────────────

    const GAMES: &[&str] = &["Everquest Legends"];
    const SERVERS: &[&str] = &["Test"];
    const CLASS_NAME: &str = "FroklogSettings\0";

    // ── State ─────────────────────────────────────────────────────────────────

    struct SettingsState {
        handle: Arc<AppHandle>,
        // Controls (populated in WM_CREATE)
        combo_game: HWND,
        combo_server: HWND,
        edit_player: HWND,
        edit_logfile: HWND,
        edit_url: HWND,
        lbl_url_status: HWND,
        lbl_streamid: HWND,
        edit_password: HWND,
        btn_register: HWND,
        chk_public: HWND,
        // Initial draft values (loaded from config at open)
        draft_log_path: String,
        draft_server_url: String,
        draft_player: String,
        draft_server: String,
        draft_game: String,
        draft_password: String,
        draft_public: bool,
        is_registered: bool,
        stream_id_text: String,
        // Whether the user has manually touched these fields (controls auto-fill)
        player_user_set: bool,
        server_user_set: bool,
    }

    // ── Public entry point ────────────────────────────────────────────────────

    pub fn open_settings(handle: Arc<AppHandle>) {
        std::thread::Builder::new()
            .name("froklog-settings".into())
            .spawn(move || run_settings_thread(handle))
            .expect("spawn settings thread");
    }

    // ── Thread entry ──────────────────────────────────────────────────────────

    fn run_settings_thread(handle: Arc<AppHandle>) {
        let cfg = handle.config.lock().unwrap().clone();

        let state = Box::new(SettingsState {
            handle: Arc::clone(&handle),
            combo_game: HWND::default(),
            combo_server: HWND::default(),
            edit_player: HWND::default(),
            edit_logfile: HWND::default(),
            edit_url: HWND::default(),
            lbl_url_status: HWND::default(),
            lbl_streamid: HWND::default(),
            edit_password: HWND::default(),
            btn_register: HWND::default(),
            chk_public: HWND::default(),
            draft_log_path: cfg.log_path.clone().unwrap_or_default(),
            draft_server_url: cfg.server_url.clone().unwrap_or_default(),
            draft_player: cfg.effective_player_name(),
            draft_server: cfg
                .server_name
                .clone()
                .or_else(|| cfg.server_name_from_log())
                .unwrap_or_else(|| SERVERS[0].to_string()),
            draft_game: cfg.game.clone().unwrap_or_else(|| GAMES[0].to_string()),
            draft_password: cfg.stream_password.clone().unwrap_or_default(),
            draft_public: cfg.public_stream,
            is_registered: cfg.is_registered(),
            stream_id_text: cfg
                .stream_id
                .clone()
                .unwrap_or_else(|| "Not registered".into()),
            player_user_set: cfg.player_name.is_some(),
            server_user_set: cfg.server_name.is_some(),
        });
        let state_ptr = Box::into_raw(state);

        unsafe {
            let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let hinstance = HINSTANCE(hmodule.0);
            let class_w: Vec<u16> = CLASS_NAME.encode_utf16().collect();

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(settings_wnd_proc),
                hInstance: hinstance,
                lpszClassName: PCWSTR(class_w.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                // COLOR_BTNFACE (15) + 1 cast to HBRUSH — standard dialog background.
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 as usize + 1) as *mut c_void),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let w = 490i32;
            let h = 430i32;
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            let x = (sw - w) / 2;
            let y = (sh - h) / 2;

            let title = wide("froklog Settings");
            CreateWindowExW(
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

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        handle.settings_open.store(false, Ordering::Relaxed);
    }

    // ── Window procedure ──────────────────────────────────────────────────────

    unsafe extern "system" fn settings_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut SettingsState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let state = &mut *ptr;
            create_controls(hwnd, state);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_COMMAND => {
                handle_command(hwnd, state, wparam);
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
                        let mut cfg = state.handle.config.lock().unwrap();
                        cfg.stream_id = Some(stream_id);
                        cfg.stream_token = Some(stream_token);
                        cfg.view_token = Some(view_token);
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

    unsafe fn handle_command(hwnd: HWND, state: &mut SettingsState, wparam: WPARAM) {
        let id = loword(wparam.0) as i32;
        let notif = hiword(wparam.0);

        if id == IDC_PLAYER_EDIT && notif == EN_CHANGE {
            state.player_user_set = true;
        }
        if id == IDC_SERVER_COMBO && notif == CBN_SELCHANGE {
            state.server_user_set = true;
        }

        match id {
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
                        set_wnd_text(state.lbl_streamid, "Not registered");
                        set_wnd_text(state.btn_register, "Register");
                    }
                } else {
                    let url = get_text(state.edit_url);
                    let player = get_text(state.edit_player);
                    let server = combo_text(state.combo_server);
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
                    let is_public = SendMessageW(state.chk_public, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
                        == BST_CHECKED;
                    let hwnd_usize = hwnd.0 as usize;
                    std::thread::spawn(move || unsafe {
                        let result = do_register(&url, &player, &server, &password, is_public);
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

            IDC_SAVE_BTN => match validate_and_save(state) {
                Ok(()) => {
                    let _ = DestroyWindow(hwnd);
                }
                Err(e) => {
                    msgbox(hwnd, "Validation error", &e, MB_ICONWARNING | MB_OK);
                }
            },

            IDC_CANCEL_BTN => {
                let _ = DestroyWindow(hwnd);
            }

            _ => {}
        }
    }

    // ── Validate & save ───────────────────────────────────────────────────────

    unsafe fn validate_and_save(state: &mut SettingsState) -> Result<(), String> {
        let player = get_text(state.edit_player);
        if !player.is_empty() && !player.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err("Player name may only contain letters A–Z.".into());
        }
        let log_path = get_text(state.edit_logfile);
        let server_url = get_text(state.edit_url);
        let password = get_text(state.edit_password);
        let game = combo_text(state.combo_game);
        let server = combo_text(state.combo_server);
        let public = SendMessageW(state.chk_public, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as usize
            == BST_CHECKED;

        let mut cfg = state.handle.config.lock().unwrap();
        let was_ready = cfg.is_ready();

        // Snapshot registration credentials before mutation so we can PATCH the
        // server if public_stream changed without requiring a re-registration.
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
        cfg.save();

        if cfg.is_ready() || was_ready {
            state
                .handle
                .restart
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        drop(cfg);

        if let Some((url, id, tok)) = patch_creds {
            std::thread::spawn(move || {
                patch_public_stream(&url, &id, &tok, public);
            });
        }

        Ok(())
    }

    // ── Control creation ──────────────────────────────────────────────────────

    unsafe fn create_controls(hwnd: HWND, state: &mut SettingsState) {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hi = HINSTANCE(hmodule.0);
        let font = GetStockObject(DEFAULT_GUI_FONT);

        // Layout constants.
        let lx = 12i32; // label left
        let lw = 86i32; // label width
        let cx = 104i32; // control left
        let cw = 264i32; // control width (full, no side button)
        let cw2 = 188i32; // control width when a side button follows
        let bx = 298i32; // side button left
        let bw = 78i32; // side button width
        let ch = 22i32; // control height
        let row = 32i32; // row stride
        let mut y = 12i32;

        // ── Game ─────────────────────────────────────────────────────────────
        mk_label(hwnd, hi, font, "Game:", lx, y, lw, ch);
        state.combo_game = mk_combo(hwnd, hi, font, cx, y, cw, IDC_GAME_COMBO);
        for g in GAMES {
            cb_add(state.combo_game, g);
        }
        cb_sel(
            state.combo_game,
            GAMES
                .iter()
                .position(|&g| g == state.draft_game)
                .unwrap_or(0),
        );
        y += row;

        // ── Server ───────────────────────────────────────────────────────────
        mk_label(hwnd, hi, font, "Server:", lx, y, lw, ch);
        state.combo_server = mk_combo(hwnd, hi, font, cx, y, cw, IDC_SERVER_COMBO);
        for s in SERVERS {
            cb_add(state.combo_server, s);
        }
        cb_sel(
            state.combo_server,
            SERVERS
                .iter()
                .position(|&s| s == state.draft_server)
                .unwrap_or(0),
        );
        y += row;

        // ── Player ───────────────────────────────────────────────────────────
        mk_label(hwnd, hi, font, "Player:", lx, y, lw, ch);
        state.edit_player = mk_edit(
            hwnd,
            hi,
            font,
            &state.draft_player.clone(),
            cx,
            y,
            cw,
            ch,
            IDC_PLAYER_EDIT,
            0,
        );
        y += row;

        // ── Log File ─────────────────────────────────────────────────────────
        mk_label(hwnd, hi, font, "Log File:", lx, y, lw, ch);
        state.edit_logfile = mk_edit(
            hwnd,
            hi,
            font,
            &state.draft_log_path.clone(),
            cx,
            y,
            cw2,
            ch,
            IDC_LOGFILE_EDIT,
            ES_READONLY,
        );
        mk_button(hwnd, hi, font, "Browse…", bx, y, bw, ch, IDC_LOGFILE_BROWSE);
        y += row;

        // ── Server URL ───────────────────────────────────────────────────────
        mk_label(hwnd, hi, font, "Server URL:", lx, y, lw, ch);
        state.edit_url = mk_edit(
            hwnd,
            hi,
            font,
            &state.draft_server_url.clone(),
            cx,
            y,
            cw2,
            ch,
            IDC_URL_EDIT,
            0,
        );
        mk_button(hwnd, hi, font, "Test", bx, y, bw, ch, IDC_URL_TEST);
        y += row;

        // ── URL status ───────────────────────────────────────────────────────
        state.lbl_url_status = mk_static(
            hwnd,
            hi,
            font,
            "",
            cx,
            y,
            cw + bw + (bx - cx - cw2),
            ch,
            IDC_URL_STATUS,
            SS_LEFT,
        );
        y += 28i32;

        // ── Stream ID ────────────────────────────────────────────────────────
        mk_label(hwnd, hi, font, "Stream ID:", lx, y, lw, ch);
        state.lbl_streamid = mk_static(
            hwnd,
            hi,
            font,
            &state.stream_id_text.clone(),
            cx,
            y,
            cw,
            ch,
            IDC_STREAMID_VALUE,
            SS_LEFT,
        );
        y += row;

        // ── Server Password ──────────────────────────────────────────────────
        mk_label(hwnd, hi, font, "Password:", lx, y, lw, ch);
        state.edit_password = mk_edit(
            hwnd,
            hi,
            font,
            &state.draft_password.clone(),
            cx,
            y,
            cw,
            ch,
            IDC_PASSWORD_EDIT,
            0,
        );
        y += row;

        // ── Register / Unregister ────────────────────────────────────────────
        let reg_label = if state.is_registered {
            "Unregister"
        } else {
            "Register"
        };
        state.btn_register = mk_button(hwnd, hi, font, reg_label, cx, y, 110, ch, IDC_REGISTER_BTN);
        y += row + 4;

        // ── Public streaming ─────────────────────────────────────────────────
        state.chk_public = mk_checkbox(
            hwnd,
            hi,
            font,
            "Allow public streaming",
            cx,
            y,
            cw,
            ch,
            IDC_PUBLIC_CHECK,
        );
        if state.draft_public {
            SendMessageW(
                state.chk_public,
                BM_SETCHECK,
                WPARAM(BST_CHECKED),
                LPARAM(0),
            );
        }
        y += row + 8;

        // ── Save / Cancel ────────────────────────────────────────────────────
        let right = bx + bw; // right edge of all controls
        let bw2 = 80i32;
        mk_button(
            hwnd,
            hi,
            font,
            "Cancel",
            right - bw2,
            y,
            bw2,
            ch,
            IDC_CANCEL_BTN,
        );
        mk_button(
            hwnd,
            hi,
            font,
            "Save",
            right - bw2 * 2 - 8,
            y,
            bw2,
            ch,
            IDC_SAVE_BTN,
        );
    }

    // ── Control factories ─────────────────────────────────────────────────────

    unsafe fn mk_child(
        parent: HWND,
        hi: HINSTANCE,
        font: HGDIOBJ,
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
        font: HGDIOBJ,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> HWND {
        mk_child(parent, hi, font, "STATIC", text, x, y, w, h, 0, SS_RIGHT)
    }

    unsafe fn mk_static(
        parent: HWND,
        hi: HINSTANCE,
        font: HGDIOBJ,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
        style: u32,
    ) -> HWND {
        mk_child(parent, hi, font, "STATIC", text, x, y, w, h, id, style)
    }

    unsafe fn mk_edit(
        parent: HWND,
        hi: HINSTANCE,
        font: HGDIOBJ,
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

    unsafe fn mk_combo(
        parent: HWND,
        hi: HINSTANCE,
        font: HGDIOBJ,
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

    unsafe fn mk_button(
        parent: HWND,
        hi: HINSTANCE,
        font: HGDIOBJ,
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

    unsafe fn mk_checkbox(
        parent: HWND,
        hi: HINSTANCE,
        font: HGDIOBJ,
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

    // ── Combo helpers ─────────────────────────────────────────────────────────

    unsafe fn cb_add(hwnd: HWND, text: &str) {
        let w = wide(text);
        SendMessageW(hwnd, CB_ADDSTRING, WPARAM(0), LPARAM(w.as_ptr() as isize));
    }

    unsafe fn cb_sel(hwnd: HWND, idx: usize) {
        SendMessageW(hwnd, CB_SETCURSEL, WPARAM(idx), LPARAM(0));
    }

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

    // ── Text helpers ──────────────────────────────────────────────────────────

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
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn loword(x: usize) -> usize {
        x & 0xffff
    }
    fn hiword(x: usize) -> usize {
        (x >> 16) & 0xffff
    }

    // ── Message box ───────────────────────────────────────────────────────────

    fn msgbox(hwnd: HWND, title: &str, msg: &str, flags: MESSAGEBOX_STYLE) -> i32 {
        let tw = wide(title);
        let mw = wide(msg);
        unsafe { MessageBoxW(hwnd, PCWSTR(mw.as_ptr()), PCWSTR(tw.as_ptr()), flags).0 }
    }

    // ── File picker ───────────────────────────────────────────────────────────

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

    // ── Filename parsing ──────────────────────────────────────────────────────

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

    // ── Background: URL test ──────────────────────────────────────────────────

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

    // ── Background: register ──────────────────────────────────────────────────

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

    fn do_register(url: &str, player: &str, server: &str, password: &str, public_stream: bool) -> RegisterResult {
        let endpoint = format!("{}/stream", url.trim_end_matches('/'));
        let body = serde_json::json!({ "player": player, "server": server, "public_stream": public_stream });

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

// ── Non-Windows stub ──────────────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
mod win {
    use crate::tray::tray::AppHandle;
    use std::sync::Arc;
    pub fn open_settings(_handle: Arc<AppHandle>) {}
}

// ── Public re-export ──────────────────────────────────────────────────────────

pub use win::open_settings;
