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
    embed_resource::compile_for("assets/froklog.rc", ["froklog"], embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
