fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // SetDefaultDllDirectories() in main() runs only AFTER the loader has resolved
        // static imports. This makes the loader itself search System32 only for the
        // EXE's static imports (LOAD_LIBRARY_SEARCH_SYSTEM32 = 0x800), so non-KnownDLLs
        // like dwmapi.dll / uxtheme.dll can't be planted next to DAWStream.exe.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
        }
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon.ico");
        res.compile().expect("failed to embed Windows icon resource");
    }
}
