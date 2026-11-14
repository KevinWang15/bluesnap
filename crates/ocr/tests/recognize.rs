//! End-to-end check that the embedded models load and read mixed scripts.

#[test]
fn reads_chinese_japanese_and_latin_text() {
    let image = image::open(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/multilingual.png")).unwrap().to_rgba8();
    let lines = bluesnap_ocr::recognize(image.width(), image.height(), image.as_raw()).unwrap();
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
