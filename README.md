# bluesnap

A fast screenshot tool for Windows and macOS, written in Rust with [GPUI](https://gpui.rs).

Press the hotkey, then:

- move the mouse: the window under the cursor is highlighted (blue border, size label)
- click: select that window, or drag: select a region
- drag the 8 handles to resize the selection, or drag inside it to move it
- annotate with rectangle, ellipse, arrow, pen, mosaic and text. Click an existing
  annotation to select it, then drag it, drag its handles, change its size/color in
  the style bar, or press Delete. Ctrl/Cmd+Z undoes.
- the text-extraction button (the "T" in a frame) recognizes the text in the selection,
  copies it to the clipboard as plain text and closes. Mosaicked areas stay unreadable
  because recognition runs on the image as shown.
- ✓ (or Enter, Ctrl/Cmd+C, double-click) copies to the clipboard, the download icon
  (or Ctrl/Cmd+S) saves a PNG, ✕ (or Esc) cancels. Right-click goes back to window
  picking, or exits when nothing is selected.

Default hotkey: **Ctrl+Alt+A** on Windows, **Option+Cmd+A** on macOS. It is set in
`config.toml`, created on first run in the OS config directory
(`%APPDATA%\bluesnap\` on Windows, `~/Library/Application Support/bluesnap/` on macOS).

## Layout

```
crates/
  core/       bluesnap-core: no OS or UI code. Editor state machine (picking, selection,
              annotations, undo, text editing), shapes and hit testing, panel placement,
              tiny-skia rasterizer used for both the live preview and the export.
  platform/   bluesnap-platform: every OS API call, behind the `Platform` trait.
              windows.rs  GDI capture, EnumWindows + DWM window bounds, topmost overlay
              macos.rs    CoreGraphics capture and window list, AppKit overlay setup
              linux.rs    stub (builds, capture returns an error)
  ocr/        bluesnap-ocr: embedded PP-OCRv6 small models run by tract (pure Rust).
  app/        bluesnap: the GPUI frontend. Hotkey, overlay window, toolbar, clipboard, save.
third_party/xattr   patched copy of a gpui dependency (see below)
```

Text recognition is per OS: macOS uses the built-in Vision framework (Simplified and
Traditional Chinese plus English), Windows uses the embedded PP-OCRv6 small model
(Simplified/Traditional Chinese, English, Japanese and 46 Latin-script languages).
The models are only compiled into non-macOS builds; they add about 38 MB to the
binary (about 28 MB compressed).

The capture happens before the overlay window opens. The overlay shows the frozen
screenshot, so the image is exactly what was on screen when the hotkey was pressed.

Adding Linux means implementing `Platform` in `crates/platform/src/linux.rs`
(X11 `GetImage` + `_NET_CLIENT_LIST_STACKING`, or the XDG screenshot portal on Wayland).

## Building

```
cargo run --release -p bluesnap
```

- **Windows**: MSVC toolchain. Release builds of gpui compile shaders with `fxc.exe`
  from the Windows SDK (set `GPUI_FXC_PATH` if it isn't found).
- **macOS**: Xcode (gpui compiles Metal shaders). On first capture macOS asks for
  Screen Recording permission; until it's granted captures contain only the wallpaper.
  The app runs without a Dock icon.
- **Linux**: builds for development (needs `libxkbcommon-x11-dev` and friends), but
  capturing isn't implemented.

`cargo test --workspace` runs the editor, geometry, layout and rendering tests.
`RUST_LOG=bluesnap=debug` logs capture and overlay timings.

## Performance

`cargo bench -p bluesnap-core` measures the hot paths (4K screen, full-screen
selection, 20 annotations). On a 20-thread x86 Linux machine:

| benchmark | time |
|---|---|
| capture buffer → GPU image + editor copy | 3.3 ms |
| frame while drawing with the pen | 0.6 ms |
| frame while moving the selection | ~0 (no re-render) |
| frame while typing text | 0.7 ms |
| export (copy/save) | 7 ms |
| first mosaic stroke | 3.7 ms |

Annotations are split into a cached layer of finished shapes and a small layer for
the shape being edited; both are clipped to the selection on the GPU, so only the
edited shape is re-rasterized and uploaded per mouse move. With
`RUST_LOG=bluesnap=debug` the app logs a timeline from hotkey to first presented
frame (65–115 ms under WSLg, most of it gpui window creation and the first frame).

Text recognition with the embedded model (Windows) takes about 0.5 s for an
800×450 selection of dense text and 2 s for a full 1080p screen, plus about 0.4 s
on first use. While warm it holds several hundred MB, so it unloads after a minute.

## Known limitations

- Only the display under the cursor is captured.
- No tray icon; quit by ending the process.
- macOS capture uses `CGDisplayCreateImage`, which Apple deprecated in favor of
  ScreenCaptureKit (much slower for a single frame). If a future macOS removes it,
  `macos.rs` is the only file to change.
- On Linux/X11, the `global-hotkey` crate misses a hotkey press made within about a
  second of closing the overlay. Windows and macOS use different hotkey backends.
- `third_party/xattr` patches xattr 0.2.3 (pulled in by gpui 0.2.2), which fails to
  compile on Linux against current `libc`. It is unused on Windows and macOS builds.
