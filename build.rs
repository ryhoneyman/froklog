fn main() {
    // Compile the glibc compat shim only on Linux targets.
    // Functions are declared __attribute__((weak)) in the C source so native
    // glibc symbols (present in glibc >= 2.38) take precedence automatically.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        cc::Build::new()
            .file("compat/glibc_compat.c")
            .compile("glibc_compat");
    }

    // Embed the frog icon as froklog.exe's file icon. No-ops when not
    // targeting Windows (native Linux builds, other binaries' host checks).
    //
    // Two .rc source files exist because `embed_resource`'s two Windows
    // resource-compiler backends disagree on what directory a quoted ICON
    // path is relative to: GNU windres (mingw target) runs with the crate
    // root as its cwd, so the path needs the "assets/" prefix; llvm-rc
    // (msvc target, via cargo-xwin) instead runs with the .rc file's own
    // directory as its cwd (embed_resource sets this explicitly before
    // invoking it), so the same prefix resolves to the nonexistent
    // assets/assets/froklog.ico. One shared .rc can't satisfy both.
    let rc_path = if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        "assets/froklog-msvc.rc"
    } else {
        "assets/froklog.rc"
    };
    embed_resource::compile_for(rc_path, ["froklog"], embed_resource::NONE)
        .manifest_optional()
        .unwrap();

    // Slint UI sources — only compiled when the `tray` feature (which pulls
    // in the `slint` dependency) is enabled. Each entry point is compiled
    // separately and included via its own generated file (see
    // `src/bin/*/main.rs`'s explicit `include!`) rather than
    // `slint::include_modules!()`, since that macro assumes a single
    // generated-module env var and we have more than one .slint entry point
    // sharing this one build script. The six add/edit dialogs (trigger,
    // condition, action, color picker, sound label, log profile) used to be
    // separate entry points here too; they're now `ui/panels/*.slint`
    // components `import`ed into settings_shell.slint's embedded drawer, so
    // they no longer need their own `compile()` call — same as
    // `ui/tabs/*.slint`.
    if std::env::var("CARGO_FEATURE_TRAY").is_ok() {
        slint_build::compile("ui/spike_overlay.slint").unwrap();
        slint_build::compile("ui/settings_shell.slint").unwrap();
        slint_build::compile("ui/overlay_alert.slint").unwrap();
        slint_build::compile("ui/overlay_history.slint").unwrap();
        slint_build::compile("ui/overlay_dps.slint").unwrap();
        slint_build::compile("ui/overlay_merged.slint").unwrap();
    }
}
