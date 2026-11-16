"""The demo scenes: one function per GIF in docs/demo/, each showing one feature
with the real app.

Positions come from demo/desktop.py's layout (what's on the fake screen) and
from the app's control reports (`rec.press("Tool(Redact)")`), not from fixed
coordinates, so scenes survive UI changes.
"""

import subprocess
import time

import desktop
from record import App, Rec, ai_settings, run, wait_window


def scene(name, layout, ai=None):
    """A fresh app with its capture open, and a recorder for `name`."""
    app = App(layout, ai=ai)
    return app, Rec(app, name)


def pick(rec, rect, caption=None, at=(0.62, 0.45)):
    """Hovers a window and clicks it, like a user picking it."""
    if caption:
        rec.caption(caption)
    x, y, w, h = rect
    rec.goto((x + w * at[0], y + h * at[1]), 700)
    rec.pause(400)
    rec.click()
    rec.include(rect)
    rec.pause(300)


def spot(layout, prefix):
    """The first spot whose name starts with `prefix`."""
    return next(r for name, r in layout.spots.items() if name.startswith(prefix))


def pick_window(layout):
    app, rec = scene("pick-window", layout)
    chat, term, docs = layout.windows
    rec.caption("Hover to pick a window")
    rec.mouse((1300, 1300))
    rec.pause(400)
    for win in (term, docs, chat):
        rec.goto((win[0] + win[2] * 0.55, win[1] + win[3] * 0.5), 800)
        rec.pause(650)
    rec.caption("Click to select it")
    rec.click()
    rec.cursor("arrow")
    rec.goto(rec.control("Done"), 700)
    rec.pause(1400)
    rec.save(crop=(0, 0, desktop.W, desktop.H), width=1000)
    app.close()


def select_region(layout):
    app, rec = scene("select-region", layout)
    rec.caption("Drag out any region")
    rec.mouse((1240, 140))
    rec.pause(300)
    rec.drag((1330, 70), (2470, 690), 900)
    rec.pause(400)
    rec.caption("Adjust it with the handles")
    rec.cursor("arrow")
    x, y, w, h = rec.rect("selection")
    rec.drag((x + w, y + h), (x + w - 200, y + h + 220), 800)
    rec.pause(300)
    rec.caption("…or drag it somewhere else")
    x, y, w, h = rec.rect("selection")
    rec.drag((x + w * 0.5, y + h * 0.4), (x + w * 0.5 - 60, y + h * 0.4 + 480), 900)
    rec.pause(1200)
    rec.save(crop=(0, 0, desktop.W, desktop.H), width=1000)
    app.close()


def loupe(layout):
    app, rec = scene("loupe", layout)
    rec.caption("A magnifier shows the exact pixels and their color")
    bars = [layout.spots[f"docs:bar{i}"] for i in range(6)]
    rec.mouse((bars[0][0] - 120, bars[0][1] - 100))
    rec.pause(300)
    for bar in bars[:4]:
        rec.goto((bar[0] + bar[2] * 0.5, bar[1] + 30), 600)
        rec.pause(450)
    rec.caption("Press I to copy the color")
    rec.key("i", 700)
    rec.pause(900)
    x, y, w, h = desktop.DOCS
    rec.save(crop=(x - 60, y - 40, w + 120, h + 80), width=900)
    app.close()


def shapes(layout):
    app, rec = scene("shapes", layout)
    pick(rec, desktop.CHAT, "Select a window, then annotate")
    card = layout.spots["line:Use the company card: 4539 1488 0343 6467,"]
    time_ = layout.spots["msg:Priya Raman:10:07"]
    rec.caption("Rectangles and ellipses")
    rec.press("Tool(Rect)")
    rec.drag((card[0] - 14, card[1] - 10), (card[0] + card[2] + 14, card[1] + 80), 700)
    rec.press("Tool(Ellipse)")
    rec.drag((time_[0] + 80, time_[1] - 12), (time_[0] + 330, time_[1] + 50), 700)
    rec.caption("Arrows")
    rec.press("Tool(Arrow)")
    rec.drag((card[0] + card[2] + 120, card[1] - 90), (card[0] + card[2] + 24, card[1] + 6), 700)
    rec.caption("The pen, in any color")
    rec.press("Tool(Pen)")
    rec.press("style:color:3")
    pw = layout.spots["line:to Harbor-Lights-2026"]
    rec.drag((pw[0], pw[1] + 38), (pw[0] + pw[2], pw[1] + 38), 800,
             path=[(pw[0] + pw[2] * 0.25, pw[1] + 44), (pw[0] + pw[2] * 0.5, pw[1] + 34), (pw[0] + pw[2] * 0.75, pw[1] + 44)])
    rec.pause(1500)
    rec.save()
    app.close()


def text_styles(layout):
    app, rec = scene("text-styles", layout)
    pick(rec, desktop.DOCS)
    rec.caption("Text in seven styles")
    rec.press("Tool(Text)")
    rec.press("style:text-style")
    rec.pause(1100)
    head = layout.spots["docs:h:What's new"]
    for style, dy, words in [("RoundedBoxed", -380, "Ship it!"), ("Outlined", -230, "New in 2.4")]:
        rec.press(f"menu:{style}")
        rec.cursor("cross")
        rec.click((head[0] + 760, head[1] + dy))
        rec.type(words, 90)
        rec.key("Escape", 300)
        rec.press("style:text-style")
        rec.pause(600)
    rec.press("menu:MonospacedBoxed")
    rec.cursor("cross")
    rec.click((head[0] + 330, head[1] + 4))
    rec.type("v2.4.0", 90)
    rec.key("Escape", 1500)
    rec.save()
    app.close()


def highlighter(layout):
    app, rec = scene("highlighter", layout)
    pick(rec, desktop.DOCS)
    rec.caption("The highlighter: hold Shift for a straight line")
    rec.press("Tool(Highlighter)")
    rec.cursor("cross")
    line = layout.spots["docs:h:What's new"]
    sub = layout.spots["docs:bar0"]
    # The subtitle line, straight.
    y = sub[1] - 160
    subprocess.run(["xdotool", "keydown", "shift"], check=True)
    rec.drag((desktop.DOCS[0] + 50, y), (desktop.DOCS[0] + 640, y + 8), 900)
    subprocess.run(["xdotool", "keyup", "shift"], check=True)
    rec.caption("…or freehand, with a chisel tip")
    rec.press("style:size:Large")
    rec.cursor("cross")
    x0 = desktop.DOCS[0] + 520
    rec.drag((x0, line[1] - 300), (x0 + 380, line[1] - 300), 900,
             path=[(x0 + 120, line[1] - 260), (x0 + 60, line[1] - 200), (x0 + 260, line[1] - 220), (x0 + 330, line[1] - 300)])
    rec.pause(1500)
    rec.save()
    app.close()


def counter(layout):
    app, rec = scene("counter", layout)
    pick(rec, desktop.DOCS)
    rec.caption("Counters number the steps as you click")
    rec.press("Tool(Counter)")
    rec.cursor("cross")
    for i in (0, 2, 3, 5):
        bar = layout.spots[f"docs:bar{i}"]
        rec.click((bar[0] + bar[2] / 2, bar[1] - 34), 500)
    rec.pause(500)
    rec.caption("1, 2, 3 or A, B, C or I, II, III")
    for fmt in ("UpperAlpha", "Roman", "Decimal"):
        rec.press(f"style:numbering:{fmt}")
        rec.pause(700)
    rec.pause(900)
    rec.save()
    app.close()


def redact(layout):
    app, rec = scene("redact", layout)
    pick(rec, desktop.CHAT)
    rec.press("Tool(Redact)")
    rec.cursor("cross")
    rec.caption("Redact with a mosaic brush…")
    pw = layout.spots["line:to Harbor-Lights-2026"]
    rec.drag((pw[0] + 40, pw[1] + 16), (pw[0] + pw[2], pw[1] + 16), 800)
    rec.caption("…or blur an area, as strong as you like")
    rec.press("style:area:Rect")
    rec.press("style:effect:Blur")
    rec.cursor("cross")
    card = layout.spots["line:Use the company card: 4539 1488 0343 6467,"]
    rec.drag((card[0] + 330, card[1] - 6), (card[0] + card[2] + 10, card[1] + 74), 700)
    x, y, w, h = rec.rect("style:slider")
    rec.cursor("arrow")
    rec.drag((x + w * 0.35, y + h / 2), (x + w * 0.95, y + h / 2), 900)
    rec.caption("…or cover it with a solid block")
    rec.press("style:effect:Gray")
    rec.cursor("cross")
    phone = layout.spots["line:anything breaks: (415) 555-0198"]
    rec.drag((phone[0] + 300, phone[1] - 4), (phone[0] + phone[2] + 10, phone[1] + 36), 700)
    rec.pause(1500)
    rec.save()
    app.close()


def auto_redact(layout):
    ai = ai_settings()
    if not ai:
        print("  skipped: set BLUESNAP_DEMO_AI_URL, _TOKEN and _LLM and/or _DECISIONS")
        return
    app, rec = scene("auto-redact", layout, ai=ai)
    pick(rec, desktop.CHAT)
    rec.caption("Auto finds what to hide, with the AI model you set up")
    rec.press("Tool(Redact)")
    rec.press("style:effect:Gray")
    rec.press("style:auto")
    rec.goto((desktop.CHAT[0] + 900, desktop.CHAT[1] + 260), 500)
    rec.wait_until(lambda: app.logged("auto redact:"), timeout=90, every=300)
    rec.pause(2200)
    rec.save()
    app.close()


def scrolling(layout):
    app, rec = scene("scrolling", layout)
    x, y, w, h = desktop.DOCS_CONTENT
    rec.caption("Scrolling screenshots: select, then press the scroll button")
    rec.drag((x + 20, y + 10), (x + w - 20, y + h - 40), 700)
    rec.press("Scrolling")
    # The overlay closes; the bar and the frame around the region stay.
    bar = None
    for _ in range(50):
        try:
            _, controls = app.controls("scrolling-bar")
            bar = controls
            break
        except RuntimeError:
            time.sleep(0.2)
    rec.caption("Scroll the page: it's captured and stitched as you go")
    offset = 0
    while offset < 1500:
        offset += 60
        app.scroll_file.write_text(str(offset))
        rec.pause(120)
    rec.pause(600)
    rec.caption("Edit opens it in a window with the same tools")
    bar_win = app.wait_window(lambda win: (win.w, win.h) != (desktop.W, desktop.H) and win.h < 200 and win.w > 400)
    rec.win = bar_win
    rec.press("Edit", window="scrolling-bar")
    editor = app.wait_window(lambda win: "scrolling screenshot" in win.name)
    rec.win = editor
    rec.pause(800)
    for _ in range(6):
        run("xdotool", "mousemove", editor.x + editor.w // 2, editor.y + editor.h // 2)
        run("xdotool", "click", "5")
        rec.pause(160)
    rec.pause(1200)
    rec.save(crop=(0, 0, desktop.W, desktop.H), width=1000)
    app.close()


def to_text(layout):
    app, rec = scene("to-text", layout)
    pick(rec, desktop.TERMINAL, "To text copies the text in the selection")
    rec.press("ToText")
    rec.wait_until(lambda: app.logged("copied") or app.logged("characters"), timeout=60, every=250)
    text = subprocess.run(["xclip", "-o", "-selection", "clipboard"], capture_output=True, text=True).stdout
    lines = [l for l in text.splitlines() if l.strip()]
    rec.caption(f"Copied {len(lines)} lines: “{lines[0][:28]}…”" if lines else "Copied")
    rec.frames[-1]["caption"] = rec.text
    rec.pause(2000)
    x, y, w, h = desktop.TERMINAL
    rec.save(crop=(x - 80, y - 60, w + 160, h + 220), width=900)
    app.close()


ALL = {
    "pick-window": pick_window,
    "select-region": select_region,
    "loupe": loupe,
    "shapes": shapes,
    "text-styles": text_styles,
    "highlighter": highlighter,
    "counter": counter,
    "redact": redact,
    "auto-redact": auto_redact,
    "scrolling": scrolling,
    "to-text": to_text,
}
