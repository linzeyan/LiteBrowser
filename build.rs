fn main() {
    // Compile the Slint UI on every host so the markup is validated in CI and locally, even though
    // the Rust UI code only builds on Windows (it needs WebView2).
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI build failed");

    // The icon, version info and DPI/UTF-8 manifest are a Windows-only resource.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/litebrowser.rc");
        println!("cargo:rerun-if-changed=assets/app.manifest");
        println!("cargo:rerun-if-changed=assets/icon.ico");
        embed_resource::compile("assets/litebrowser.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embedding Windows resources failed");
    }
}
