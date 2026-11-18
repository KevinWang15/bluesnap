fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=src/window_capture.m");
        cc::Build::new()
            .file("src/window_capture.m")
            .flag("-fobjc-arc")
            .compile("bluesnap_window_capture");
    }
}
