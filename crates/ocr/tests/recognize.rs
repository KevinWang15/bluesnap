//! End-to-end check that the embedded models load and read mixed scripts.

#[test]
fn reads_chinese_japanese_and_latin_text() {
    let image = image::open(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/multilingual.png")).unwrap().to_rgba8();
    let lines = bluesnap_ocr::recognize(image.width(), image.height(), image.as_raw(), 1.0).unwrap();
    let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    for expected in [
        "Hello, bluesnap! Screenshot OCR test 2026.",
        "今天天气很好，我们去公园散步吧。",
        "繁體中文：這是一個測試。",
        "日本語のテキスト認識テストです。",
        "Größe, café, naïve, déjà vu — français",
    ] {
        assert!(text.contains(&expected), "missing {expected:?} in {text:?}");
    }
    // Boxes are in image pixels: the first line sits near the top-left.
    let first = lines.iter().find(|l| l.text.starts_with("Hello")).unwrap();
    assert!(first.x < 60.0 && first.y < 60.0 && first.width > 300.0);
}

#[test]
fn small_chinese_strip_stays_legible_under_high_dpi_scale() {
    // Crop the first three lines (English + Simplified + Traditional Chinese),
    // shrink to ~12-14 px UI text height, and pass `scale = 2.0` as on a 200%
    // Windows monitor: recognition must not crush the already-small glyphs.
    let full = image::open(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/multilingual.png")).unwrap().to_rgba8();
    let strip = image::imageops::crop_imm(&full, 0, 0, full.width(), 250).to_image();
    let small = image::imageops::resize(
        &strip,
        strip.width() * 2 / 5,
        strip.height() * 2 / 5,
        image::imageops::FilterType::Triangle,
    );
    let lines = bluesnap_ocr::recognize(small.width(), small.height(), small.as_raw(), 2.0).unwrap();
    let joined: String = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
    for expected in ["Screenshot OCR test 2026", "今天天气很好", "我們去公園散步", "繁體中文", "這是一個測試"] {
        // Accept exact Simplified/Traditional variants from the fixture.
        if expected == "我們去公園散步" {
            assert!(joined.contains("我们去公园散步"), "missing Chinese phrase in {joined:?}");
        } else {
            assert!(joined.contains(expected), "missing {expected:?} in {joined:?}");
        }
    }
}
