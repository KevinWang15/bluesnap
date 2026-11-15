# bluesnap

A fast screenshot tool for Windows and macOS, written in Rust with [GPUI](https://gpui.rs).

Press the hotkey, then:

- move the mouse: the window under the cursor is highlighted (blue border, size label)
- click: select that window, or drag: select a region
- drag the 8 handles to resize the selection, or drag inside it to move it
- annotate with rectangle, ellipse, arrow, pen, mosaic and text. Click an existing
  annotation to select it, then drag it, drag its handles, change its size/color in
  the style bar, or press Delete. Ctrl/Cmd+Z undoes.
- the "To text" button (left of Save; text lines inside scan corners) recognizes the text in the selection,
  copies it to the clipboard as plain text and closes. Mosaicked areas stay unreadable
  because recognition runs on the image as shown.
- the scrolling-screenshot button (a page with a down arrow) captures more than fits
  on screen: the overlay closes, a blue frame marks the selection and a bar shows
  progress. Scroll the window under the frame (mouse wheel or trackpad; input passes
  through the frame) and bluesnap captures it about 10 times a second, stitching the
  frames into one tall image. Sticky headers and footers appear once. The bar
  next to the region has Edit, Save, ✕ and ✓: ✓ (or the hotkey again) copies the
  image, Save writes a PNG, ✕ cancels, and Edit opens it in a window with the
  same tools as the capture toolbar (shapes, pen, mosaic, text, undo, To text,
  Save, ✕, ✓), scrolled with the mouse wheel. Scroll at a moderate
  pace: if a frame shares nothing with the previous one the bar says so, and
  scrolling back a little picks it up again. Content redrawn at fractional pixel
  offsets (display scaling, trackpads) is matched with sub-pixel accuracy. Only
  the part that scrolls ends up in
  the image: fixed side panels and scrollbars (including macOS overlay scrollbars)
  are left out. Annotations aren't carried over, and the image stops at 30,000 px.
- ✓ (or Enter, Ctrl/Cmd+C, double-click) copies to the clipboard, the download icon
  (or Ctrl/Cmd+S) saves a PNG, ✕ (or Esc) cancels. Right-click goes back to window
  picking, or exits when nothing is selected.

bluesnap lives in the system tray (Windows) or menu bar (macOS). Its menu has
Take Screenshot, Edit Settings… and Quit; on Windows, left-clicking the tray icon
also takes a screenshot.

Default hotkey: **Ctrl+Alt+A** on Windows, **Ctrl+Cmd+A** on macOS. It is set in
`config.toml`, created on first run in the OS config directory
(`%APPDATA%\bluesnap\` on Windows, `~/Library/Application Support/bluesnap/` on macOS).

## Layout

```
crates/
  core/       bluesnap-core: no OS or UI code. Editor state machine (picking, selection,
              annotations, undo, text editing), shapes and hit testing, panel placement,
              tiny-skia rasterizer used for both the live preview and the export,
              scrolling-screenshot stitcher.
  platform/   bluesnap-platform: every OS API call, behind the `Platform` trait.
              windows.rs  GDI capture, EnumWindows + DWM window bounds, topmost overlay
              macos.rs    CoreGraphics capture and window list, AppKit overlay setup
              linux.rs    stub (builds, capture returns an error)
  ocr/        bluesnap-ocr: embedded PP-OCRv6 small models run by tract (pure Rust).
  app/        bluesnap: the GPUI frontend.
              editor_view.rs  the editing view, on two surfaces: the full-screen capture
                              overlay and a document window (scrolling screenshots)
              capture.rs      hotkey -> capture -> overlay; document.rs opens a document
              scrolling.rs    scrolling capture session (dim layer, bar, stitching loop)
              toolbar.rs      one table of toolbar actions; toolbars are lists of them
              viewport.rs, gpu_image.rs  image <-> window mapping, tiled GPU images
              output.rs       copy / save / cancel
packaging/          app icons (generated, source in packaging/icon), macOS Info.plist
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
- **macOS**: Xcode (gpui compiles Metal shaders). CI builds `bluesnap.app`
  (menu bar app, no Dock icon) and uploads it zipped.

### macOS Screen Recording permission

macOS shows only the wallpaper (or plain gray) in place of windows until bluesnap
has Screen Recording permission, and on macOS 15+ it ignores that permission for
unsigned or ad-hoc signed apps. bluesnap shows a banner with an "Open Settings"
button when the permission is missing. To make the permission stick, sign the app
with a real identity:

- CI: add the repository secrets `MACOS_CERTIFICATE_P12` (base64 of a `.p12` with a
  "Developer ID Application" or "Apple Development" certificate and its key),
  `MACOS_CERTIFICATE_PASSWORD` and `MACOS_SIGNING_IDENTITY` (e.g.
  `Developer ID Application: Your Name (TEAMID)`).
- Locally: create an "Apple Development" certificate (a free Apple ID works:
  Xcode → Settings → Accounts → Manage Certificates → +), then
  `codesign --force --sign "Apple Development: you@example.com (TEAMID)" bluesnap.app`.

Then open the app, enable it under System Settings → Privacy & Security → Screen &
System Audio Recording, and restart it from the menu bar icon (Quit, then reopen).
If an old entry gets in the way: `tccutil reset ScreenCapture com.kevinwang15.bluesnap`.
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

On macOS, opening the capture overlay starts warming Vision in the background so
its model loading usually finishes before you use To text.

Text recognition with the embedded model (Windows) takes about 0.5 s for an
800×450 selection of dense text and 2 s for a full 1080p screen, plus about 0.4 s
on first use. While warm it holds several hundred MB, so it unloads after a minute.

## Known limitations

- Only the display under the cursor is captured.
- macOS capture uses `CGDisplayCreateImage`, which Apple deprecated in favor of
  ScreenCaptureKit (much slower for a single frame). If a future macOS removes it,
  `macos.rs` is the only file to change.
- On Linux/X11, the `global-hotkey` crate misses a hotkey press made within about a
  second of closing the overlay. Windows and macOS use different hotkey backends.
- `third_party/xattr` patches xattr 0.2.3 (pulled in by gpui 0.2.2), which fails to
  compile on Linux against current `libc`. It is unused on Windows and macOS builds.
