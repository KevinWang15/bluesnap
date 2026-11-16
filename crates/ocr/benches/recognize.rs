//! Text recognition on a realistic screenshot: the demo recordings' fake
//! desktop (demo/desktop.py), 2560×1440 at 2× scale with a chat app, a
//! terminal and a docs window, read at its scale. `cargo bench -p bluesnap-ocr`.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

fn crop(image: &image::RgbaImage, (x, y, w, h): (u32, u32, u32, u32)) -> image::RgbaImage {
    image::imageops::crop_imm(image, x, y, w, h).to_image()
}

fn recognize(c: &mut Criterion) {
    let screen = image::open(concat!(env!("CARGO_MANIFEST_DIR"), "/benches/fixtures/desktop.png")).unwrap().to_rgba8();
    let cases = [
        // A paragraph-sized selection: four chat messages.
        ("selection_800x450", crop(&screen, (430, 280, 800, 450))),
        ("chat_window_1180x880", crop(&screen, (100, 110, 1180, 880))),
        ("full_screen_2560x1440", screen.clone()),
    ];
    let mut g = c.benchmark_group("ocr");
    g.sample_size(10);
    // The first call loads the models; measured on its own below.
    bluesnap_ocr::recognize(8, 8, &[255; 8 * 8 * 4], 1.0).unwrap();
    for (name, image) in &cases {
        let lines = bluesnap_ocr::recognize(image.width(), image.height(), image.as_raw(), 2.0).unwrap();
        eprintln!("{name}: {} lines", lines.len());
        g.bench_function(*name, |b| b.iter(|| black_box(bluesnap_ocr::recognize(image.width(), image.height(), image.as_raw(), 2.0).unwrap())));
    }
    g.finish();
}

criterion_group!(benches, recognize);
criterion_main!(benches);
