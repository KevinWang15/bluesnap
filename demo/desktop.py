"""The fake desktop the demos capture: a wallpaper with a chat app, a terminal
and a docs window, drawn at 2x (2560x1440 pixels = 1280x720 logical).

`build(out)` writes desktop.png and page.png (the docs window's long page, for
scrolling screenshots) and returns the layout: window rectangles and where
interesting things (a card number, a chart bar, ...) were drawn, so scenes can
aim at them by name. Rectangles are (x, y, w, h) in screen pixels.
"""

from pathlib import Path

import numpy as np

from PIL import Image, ImageDraw, ImageFilter, ImageFont

W, H = 2560, 1440
SCALE = 2

SANS = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
MONO = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"


def font(path, size):
    return ImageFont.truetype(path, size)


CHAT = (100, 110, 1180, 880)
TERMINAL = (1380, 110, 1080, 560)
DOCS = (1380, 730, 1080, 600)
TITLE = 64  # title bar height
# The docs window's content area, where the long page scrolls.
DOCS_CONTENT = (DOCS[0], DOCS[1] + TITLE, DOCS[2], DOCS[3] - TITLE)


class Layout:
    def __init__(self):
        self.windows = [CHAT, TERMINAL, DOCS]  # topmost first (they don't overlap)
        self.spots = {}

    def mark(self, name, box):
        self.spots[name] = tuple(int(v) for v in box)


def wallpaper():
    top, bottom = np.array([28, 35, 74], float), np.array([88, 48, 120], float)
    t = np.linspace(0, 1, H)[:, None, None]
    img = Image.fromarray(np.broadcast_to(top + (bottom - top) * t, (H, W, 3)).astype(np.uint8))
    glow = Image.new("RGB", (W, H), (0, 0, 0))
    d = ImageDraw.Draw(glow)
    d.ellipse((1500, -300, 2900, 900), fill=(40, 110, 200))
    d.ellipse((-400, 700, 1100, 1900), fill=(150, 60, 140))
    glow = glow.filter(ImageFilter.GaussianBlur(220))
    return Image.blend(img, Image.composite(glow, img, Image.new("L", (W, H), 255)), 0.45)


def window(img, box, title, dark=False):
    """A window frame with a shadow and title bar; returns a draw for its content."""
    x, y, w, h = box
    shadow = Image.new("L", (W, H), 0)
    ImageDraw.Draw(shadow).rounded_rectangle((x + 6, y + 22, x + w + 6, y + h + 28), 26, fill=150)
    shadow = shadow.filter(ImageFilter.GaussianBlur(28))
    img.paste((0, 0, 0), (0, 0), shadow)
    d = ImageDraw.Draw(img)
    body = (30, 31, 40) if dark else (255, 255, 255)
    bar = (44, 45, 56) if dark else (236, 236, 240)
    d.rounded_rectangle((x, y, x + w, y + h), 22, fill=body)
    d.rounded_rectangle((x, y, x + w, y + TITLE), 22, fill=bar)
    d.rectangle((x, y + TITLE - 22, x + w, y + TITLE), fill=bar)
    for i, c in enumerate([(255, 95, 87), (254, 188, 46), (40, 200, 64)]):
        cx = x + 34 + i * 38
        d.ellipse((cx - 12, y + 20, cx + 12, y + 44), fill=c)
    f = font(BOLD, 26)
    tw = d.textlength(title, font=f)
    d.text((x + (w - tw) / 2, y + 17), title, font=f, fill=(200, 200, 210) if dark else (60, 60, 70))
    return d


def chat(img, layout):
    x, y, w, h = CHAT
    d = window(img, CHAT, "Team Chat")
    side = 300
    d.rectangle((x, y + TITLE, x + side, y + h - 22), fill=(245, 245, 249))
    d.rounded_rectangle((x, y + h - 44, x + side, y + h), 22, fill=(245, 245, 249))
    d.rectangle((x + 22, y + h - 44, x + side, y + h), fill=(245, 245, 249))
    fs, fb = font(SANS, 26), font(BOLD, 26)
    d.text((x + 32, y + TITLE + 30), "Acme Inc.", font=font(BOLD, 30), fill=(30, 30, 40))
    channels = ["# general", "# team-ops", "# design", "# releases"]
    for i, ch in enumerate(channels):
        cy = y + TITLE + 100 + i * 52
        if ch == "# team-ops":
            d.rounded_rectangle((x + 16, cy - 8, x + side - 16, cy + 40), 10, fill=(22, 119, 255))
        d.text((x + 32, cy), ch, font=fb if ch == "# team-ops" else fs, fill=(255, 255, 255) if ch == "# team-ops" else (90, 90, 100))
    d.text((x + 32, y + TITLE + 330), "Direct messages", font=font(BOLD, 22), fill=(140, 140, 150))
    for i, (name, color) in enumerate([("Lena Fischer", (236, 112, 99)), ("Daniel Osei", (93, 173, 226)), ("Priya Raman", (88, 214, 141))]):
        cy = y + TITLE + 380 + i * 52
        d.ellipse((x + 32, cy + 4, x + 54, cy + 26), fill=color)
        d.text((x + 66, cy), name, font=fs, fill=(70, 70, 80))

    left = x + side + 40
    d.text((left, y + TITLE + 28), "# team-ops", font=font(BOLD, 32), fill=(30, 30, 40))
    d.line((x + side, y + TITLE + 88, x + w, y + TITLE + 88), fill=(232, 232, 238), width=2)
    messages = [
        ("Lena Fischer", (236, 112, 99), "10:02", ["Can someone pay the hosting invoice?", "It bounced again."]),
        ("Daniel Osei", (93, 173, 226), "10:04", ["Use the company card: 4539 1488 0343 6467,", "exp 09/28, CVV 731"]),
        ("Lena Fischer", (236, 112, 99), "10:05", ["Thanks! Also the VPN password changed", "to Harbor-Lights-2026"]),
        ("Priya Raman", (88, 214, 141), "10:07", ["Standup moved to 10:30. Call me if", "anything breaks: (415) 555-0198"]),
    ]
    cy = y + TITLE + 120
    for name, color, time, lines in messages:
        d.ellipse((left, cy, left + 60, cy + 60), fill=color)
        initials = "".join(p[0] for p in name.split())
        f = font(BOLD, 24)
        d.text((left + 30 - d.textlength(initials, font=f) / 2, cy + 16), initials, font=f, fill=(255, 255, 255))
        d.text((left + 84, cy), name, font=fb, fill=(30, 30, 40))
        d.text((left + 84 + d.textlength(name, font=fb) + 16, cy + 3), time, font=font(SANS, 22), fill=(150, 150, 160))
        for i, line in enumerate(lines):
            ly = cy + 40 + i * 36
            d.text((left + 84, ly), line, font=fs, fill=(55, 55, 65))
            layout.mark(f"line:{line}", (left + 84, ly, d.textlength(line, font=fs), 32))
        layout.mark(f"msg:{name}:{time}", (left, cy, w - side - 80, 40 + len(lines) * 36))
        cy += 64 + len(lines) * 36
    # The composer.
    d.rounded_rectangle((left, y + h - 120, x + w - 40, y + h - 44), 16, outline=(220, 220, 228), width=2, fill=(250, 250, 252))
    d.text((left + 24, y + h - 98), "Message #team-ops", font=fs, fill=(170, 170, 180))
    layout.mark("chat:messages", (left - 10, y + TITLE + 100, w - side - 60, cy - (y + TITLE + 100)))


def terminal(img, layout):
    x, y, w, h = TERMINAL
    d = window(img, TERMINAL, "dev@build-07: ~/api", dark=True)
    fm = font(MONO, 25)
    # Fake keys, split so secret scanners don't flag this file.
    lines = [
        ("$ ", "cat .env", (130, 200, 255)),
        ("", "NODE_ENV=production", (210, 210, 220)),
        ("", "OPENAI_API_KEY=sk-" "proj-Qm3vT8xLw2Zr9KpY4nB7cD1e", (210, 210, 220)),
        ("", "DATABASE_URL=postgres://svc_api:Vx7!pQ2#rT9z@10.0.3.14/db", (210, 210, 220)),
        ("", "STRIPE_SECRET=sk_" "live_51HxQ2bLkq8ZpR3tYv0aW9cE", (210, 210, 220)),
        ("$ ", "git log -1 --format='%an <%ae>'", (130, 200, 255)),
        ("", "Priya Raman <priya.raman@fintrack.io>", (210, 210, 220)),
        ("$ ", "npm test", (130, 200, 255)),
        ("", "  48 passing (1s)", (120, 220, 140)),
    ]
    for i, (prompt, text, color) in enumerate(lines):
        ly = y + TITLE + 30 + i * 46
        px = x + 32
        if prompt:
            d.text((px, ly), prompt, font=fm, fill=(120, 220, 140))
            px += d.textlength(prompt, font=fm)
        d.text((px, ly), text, font=fm, fill=color)
        layout.mark(f"term:{i}", (px, ly, d.textlength(text, font=fm), 32))
    layout.mark("terminal:text", (x + 20, y + TITLE + 20, w - 40, len(lines) * 46 + 20))


def docs_page(width, height):
    """The docs window's long page: its top shows on the desktop."""
    page = Image.new("RGB", (width, height), (255, 255, 255))
    d = ImageDraw.Draw(page)
    spots = {}
    fh, fb, fs = font(BOLD, 38), font(BOLD, 28), font(SANS, 25)
    y = 36
    d.text((56, y), "Release notes 2.4", font=fh, fill=(25, 25, 35))
    y += 70
    d.text((56, y), "Faster sync, a new chart view, and fewer surprises.", font=fs, fill=(110, 110, 120))
    y += 64
    # A bar chart: bright colors for the color picker.
    d.text((56, y), "Weekly active teams", font=fb, fill=(40, 40, 50))
    y += 52
    colors = [(22, 119, 255), (82, 196, 26), (250, 173, 20), (245, 34, 45), (114, 46, 209), (19, 194, 194)]
    base = y + 200
    for i, (c, v) in enumerate(zip(colors, [120, 170, 140, 190, 110, 160])):
        bx = 70 + i * 120
        d.rounded_rectangle((bx, base - v, bx + 80, base), 10, fill=c)
        spots[f"bar{i}"] = (bx, base - v, 80, v)
    d.line((56, base, 56 + 6 * 120 + 20, base), fill=(220, 220, 228), width=2)
    y = base + 50
    sections = [
        ("What's new", [
            "Sync is up to 3x faster on large workspaces.",
            "Charts: switch any table to a bar or line chart.",
            "Mentions now work in thread replies.",
            "Dark mode follows the system setting.",
        ]),
        ("Fixes", [
            "Exports no longer drop the last row of a table.",
            "Search finds words with accents again.",
            "Pasting images into comments keeps their size.",
            "Notifications respect quiet hours on mobile.",
        ]),
        ("Upgrading", [
            "Workspace admins can roll out 2.4 per team.",
            "The API is unchanged; webhooks gain a retry header.",
            "Self-hosted installs need PostgreSQL 14 or later.",
            "Read the migration guide before upgrading.",
        ]),
        ("Known issues", [
            "Very large CSV imports can time out after 5 minutes.",
            "Chart colors reset when a column is renamed.",
            "Offline edits to the same cell keep the last change.",
            "Some screen readers announce charts twice.",
        ]),
        ("Thanks", [
            "To everyone who filed issues during the beta.",
            "To the 140 teams who tried the new sync early.",
            "To the docs team for the new tutorials.",
            "And to you, for reading the release notes.",
        ]),
    ]
    for title, items in sections:
        d.text((56, y), title, font=fb, fill=(40, 40, 50))
        spots[f"h:{title}"] = (56, y, d.textlength(title, font=fb), 34)
        y += 54
        for item in items:
            d.ellipse((66, y + 12, 76, y + 22), fill=(150, 150, 160))
            d.text((96, y), item, font=fs, fill=(60, 60, 70))
            spots[f"li:{item}"] = (96, y, d.textlength(item, font=fs), 32)
            y += 44
        y += 30
    return page.crop((0, 0, width, min(height, y + 40))), spots


def build(out: Path) -> Layout:
    out.mkdir(parents=True, exist_ok=True)
    layout = Layout()
    img = wallpaper()
    chat(img, layout)
    terminal(img, layout)
    window(img, DOCS, "Docs: Release notes")
    cx, cy, cw, ch = DOCS_CONTENT
    page, spots = docs_page(cw, 4000)
    page.save(out / "page.png")
    img.paste(page.crop((0, 0, cw, ch - 22)), (cx, cy))
    for name, (sx, sy, sw, sh) in spots.items():
        layout.mark(f"docs:{name}", (cx + sx, cy + sy, sw, sh))
    layout.page_height = page.height
    img.save(out / "desktop.png")
    return layout


if __name__ == "__main__":
    lay = build(Path(__file__).parent / "out")
    print(len(lay.spots), "spots")
