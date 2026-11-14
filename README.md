# bluesnap

A fast screenshot tool for Windows and macOS, written in Rust with [GPUI](https://gpui.rs).

Press the hotkey, then:

- move the mouse: the window under the cursor is highlighted (blue border, size label)
- click: select that window, or drag: select a region
- drag the 8 handles to resize the selection, or drag inside it to move it
- annotate with rectangle, ellipse, arrow, pen, mosaic and text. Click an existing
  annotation to select it, then drag it, drag its handles, change its size/color in
  the style bar, or press Delete. Ctrl/Cmd+Z undoes.
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
  app/        bluesnap: the GPUI frontend. Hotkey, overlay window, toolbar, clipboard, save.
third_party/xattr   patched copy of a gpui dependency (see below)
```

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
