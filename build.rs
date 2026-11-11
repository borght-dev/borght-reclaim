fn main() {
    println!("cargo:rerun-if-changed=assets/reclaim.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    winresource::WindowsResource::new()
        .set_icon("assets/reclaim.ico")
        .set("ProductName", "Borght Reclaim")
        .set("FileDescription", "Borght Reclaim")
        .set("LegalCopyright", "Copyright (c) 2026 Koen van der Borght and contributors")
        .compile()
        .expect("the application icon is embedded");
}
