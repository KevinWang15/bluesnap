//! Demo mode support for the recording scripts in `demo/`: with
//! `BLUESNAP_DEMO_LAYOUT` naming a directory, each window writes where its
//! controls are, by name, to `<window>.json` there, so scripts can click
//! "the Blur button" rather than coordinates that change with the UI.
//! Positions are in the window's logical pixels; `scale` converts to device
//! pixels. Written only when something moved.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

fn dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("BLUESNAP_DEMO_LAYOUT").map(PathBuf::from)).as_ref()
}

pub fn enabled() -> bool {
    dir().is_some()
}

/// A control's bounds: (x, y, width, height).
pub type Bounds = (f32, f32, f32, f32);

/// Writes `window`'s controls (name → bounds) and display scale.
pub fn report(window: &str, scale: f32, items: impl IntoIterator<Item = (String, Bounds)>) {
    let Some(dir) = dir() else { return };
    let controls: serde_json::Map<String, Value> =
        items.into_iter().map(|(name, (x, y, w, h))| (name, json!([x, y, w, h]))).collect();
    let text = json!({ "scale": scale, "controls": controls }).to_string();
    static LAST: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let mut last = LAST.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if last.get(window) == Some(&text) {
        return;
    }
    let path = dir.join(format!("{window}.json"));
    let tmp = path.with_extension("json.tmp");
    // Written whole and renamed, so a script never reads half a file.
    if std::fs::write(&tmp, &text).and_then(|()| std::fs::rename(&tmp, &path)).is_ok() {
        last.insert(window.to_string(), text);
    }
}
