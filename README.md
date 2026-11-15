# bluesnap

A fast screenshot tool for Windows and macOS, written in Rust with [GPUI](https://gpui.rs).

Press the hotkey, then:

- move the mouse: the window under the cursor is highlighted (blue border, size label)
- click: select that window, or drag: select a region
- drag the 8 handles to resize the selection, or drag inside it to move it
- annotate with rectangle, ellipse, arrow, pen, highlighter, counter, redact and
  text. Click an existing
  annotation to select it, then drag it, drag its handles, change its size/color in
  the style bar, or press Delete. Ctrl/Cmd+Z undoes.
- the **highlighter** draws translucent strokes (yellow by default) that don't get
  darker where they cross; hold Shift for a straight line, handy for a line of
  text. A ring under the pointer shows its width.
- the **counter** places numbered markers with a click (drag to position before
  releasing): 1, 2, 3 … or A, B, C …, I, II, III … or a, b, c …, chosen in its
  style bar for the whole sequence. Deleting one renumbers the rest; drag a
  marker to move it. Labels are dark on light colors.
- the Redact tool's style bar picks how to hide things: paint with a **brush** (a
  ring under the pointer shows its size) or drag an **area**; then **mosaic**,
  **blur** or a solid **gray** block. The slider sets the mosaic block size or
  the blur strength, and dragging it updates the selected redaction live (one
  undo step per drag). Select an existing redaction to change its effect or
  strength; area redactions can be resized with their handles. Effects always
  read the original screenshot, and mosaic blocks line up across strokes.
- with an AI model set up (see [AI redaction](#ai-redaction)), the Redact bar has
  an **Auto** button: it finds passwords, keys, tokens, contact details, IDs and
  card numbers in the selection and covers each with an area redaction in the
  current effect, in about a second. One undo removes them all.
- the "To text" button (left of Save; text lines inside scan corners) recognizes the text in the selection,
  copies it to the clipboard as plain text and closes. Redacted areas stay unreadable
  because recognition runs on the image as shown.
- the scrolling-screenshot button (a page with a down arrow) captures more than fits
  on screen: the overlay closes, a blue frame marks the selection and a bar shows
  progress. Scroll the window under the frame (mouse wheel or trackpad; input passes
  through the frame) and bluesnap captures it about 10 times a second, stitching the
  frames into one tall image. Sticky headers and footers appear once. The bar
  next to the region has Edit, Save, ✕ and ✓: ✓ (or the hotkey again) copies the
  image, Save writes a PNG, ✕ cancels, and Edit opens it in a window with the
  same tools as the capture toolbar (shapes, pen, highlighter, counter, redact,
  text, undo, To text,
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
Take Screenshot, Edit Settings…, AI Redaction… and Quit; on Windows, left-clicking the tray icon
also takes a screenshot.

Default hotkey: **Ctrl+Alt+A** on Windows, **Ctrl+Cmd+A** on macOS. It is set in
`config.toml`, created on first run in the OS config directory
(`%APPDATA%\bluesnap\` on Windows, `~/Library/Application Support/bluesnap/` on macOS).

## AI redaction

Optional, and off until set up in **AI Redaction…** in the tray / menu bar menu.
Two endpoints can be configured, each with a URL, token and model; either or both:

- an **LLM** behind an OpenAI-compatible Responses API (`/v1/responses`);
- a **decision model** behind a Decisions API (`/v1/decisions`).

Each section has a Test button. The settings are saved to `ai.json` beside
`config.toml`, readable only by the user (it holds the tokens).

Text is recognized on the computer with the same OCR as To text; only the
recognized text is sent to the endpoints, never the image. The lines are sent in
reading order, 8 per LLM request and one per decision request, all in parallel (up
to 16 at a time per model; busy responses are retried). Each finding is then
placed on the screen: the line's words are matched to the clusters of ink in the
pixels, so boxes fit the actual glyphs in any font, descenders included.

How this was chosen: 12 synthetic screenshots (settings page, terminal, chat,
Chinese form and chat, config file, email, bank page, code diff, admin table, and
two with nothing sensitive) with 48 known sensitive spans, scored by the share of
spans covered and the share of redacted area that is sensitive:

| approach | found | precision | time |
|---|---|---|---|
| screenshot image to the LLM, asked for boxes | 8–12% | — | 27 s |
| OCR lines to the LLM, reasoning on | 100% | 92% | 2–4 s |
| **OCR lines to the LLM, no reasoning, 8 lines per request** | 96–100% | 95% | 0.4 s |
| OCR lines to the decision model, one per line | 96% | 69% | 0.5 s |
| **both: decision model adds lines it is sure about** | 98–100% | 93% | 0.4 s |

(Times are for the AI requests after OCR, with DeepSeek V4.1 Flash and d3-ultra.)
The image route places boxes far from the text; local OCR boxes are exact, so no
extra model is downloaded. The decision model alone redacts whole lines minus a
short label, so it works best next to an LLM.

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
  ai/         bluesnap-ai: optional AI endpoints (settings, Responses / Decisions API
              calls) and finding sensitive text in OCR lines for Auto redaction.
  app/        bluesnap: the GPUI frontend.
              editor_view.rs  the editing view, on two surfaces: the full-screen capture
                              overlay and a document window (scrolling screenshots)
              capture.rs      hotkey -> capture -> overlay; document.rs opens a document
              scrolling.rs    scrolling capture session (dim layer, bar, stitching loop)
              toolbar.rs      one table of toolbar actions; toolbars are lists of them
              viewport.rs, gpu_image.rs  image <-> window mapping, tiled GPU images
              output.rs       copy / save / cancel
              ai_settings.rs  the AI Redaction window; text_field.rs  its text inputs
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
| first mosaic stroke | 1.0 ms |
| frame while dragging the blur slider (1200×800 area) | 14 ms |
| blur of 1000 rows of a 1600×30000 scrolling screenshot | 9 ms |

Annotations are split into a cached layer of finished shapes and a small layer for
the shape being edited; both are clipped to the selection on the GPU, so only the
edited shape is re-rasterized and uploaded per mouse move. With
`RUST_LOG=bluesnap=debug` the app logs a timeline from hotkey to first presented
frame (65–115 ms under WSLg, most of it gpui window creation and the first frame).

Redaction effects are computed in 256×256 tiles, only under redactions, in
parallel, and cached for the four most recent effect settings, so slider drags
and very tall images stay responsive.

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
