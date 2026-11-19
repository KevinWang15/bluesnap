#!/usr/bin/env python3
"""Records the README's demo GIFs by driving the real app.

Run `demo/record.sh` (or `python3 demo/record.py [scene ...]`) on Linux with
Xvfb, xdotool, ImageMagick and ffmpeg. It builds nothing itself; record.sh
builds the app first. Everything runs on a private virtual X display, so the
recording neither sees nor is disturbed by the mouse and keyboard of whoever
is using the computer; the app renders with software Vulkan (Mesa lavapipe)
when no GPU is available there. The app runs in demo mode (see
crates/platform/src/linux.rs): the screen it "captures" is demo/desktop.py's
fake desktop, and it reports where its controls are (crates/app/src/demo.rs),
so scenes click controls by name and keep working when the UI changes.

Each step of a scene captures one frame of the app's window with how long it
should show; afterwards frames are cropped to where the action was, a cursor
and a caption are drawn on them, and ffmpeg encodes the GIF to docs/demo/.

The AI scene needs a model: BLUESNAP_DEMO_AI_URL, BLUESNAP_DEMO_AI_TOKEN and
BLUESNAP_DEMO_AI_LLM and/or BLUESNAP_DEMO_AI_DECISIONS (model names). Without
them it's skipped.
"""

import hashlib
import json
import math
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

sys.path.insert(0, str(Path(__file__).parent))
import desktop  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
WORK = ROOT / "demo" / "out"
OUT = ROOT / "docs" / "demo"
SCROLL_FILE = WORK / "scroll-offset"
BIN = Path(os.environ.get("BLUESNAP_BIN", ROOT / "target" / "debug" / "bluesnap"))
SCALE = desktop.SCALE
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"


def private_display():
    """Starts Xvfb on a free display, for this process and everything it runs."""
    n = next(n for n in range(90, 200) if not Path(f"/tmp/.X11-unix/X{n}").exists() and not Path(f"/tmp/.X{n}-lock").exists())
    proc = subprocess.Popen(["Xvfb", f":{n}", "-screen", "0", f"{desktop.W}x{desktop.H}x24", "-nolisten", "tcp"],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(50):
        if Path(f"/tmp/.X11-unix/X{n}").exists():
            break
        time.sleep(0.1)
    os.environ["DISPLAY"] = f":{n}"
    lavapipe = Path("/usr/share/vulkan/icd.d/lvp_icd.json")
    if lavapipe.exists():
        os.environ.setdefault("VK_ICD_FILENAMES", str(lavapipe))
    procs = [proc]
    # A compositor, for translucent windows (the scrolling capture's dim layer).
    if shutil.which("xcompmgr"):
        procs.append(subprocess.Popen(["xcompmgr"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    else:
        print("  note: install xcompmgr for translucent windows (apt install xcompmgr)")
    # The screen behind the app: the fake desktop, scrolled like demo mode's.
    SCROLL_FILE.write_text("0")
    here = Path(__file__).parent
    procs.append(subprocess.Popen([sys.executable, here / "backdrop.py", WORK / "desktop.png", WORK / "page.png", SCROLL_FILE], cwd=here))
    time.sleep(1.5)
    return procs


def run(*args, **kw):
    return subprocess.run([str(a) for a in args], check=True, capture_output=True, text=True, **kw).stdout


# ----- windows -----------------------------------------------------------------


class Win:
    """An X window: id, absolute position and size in device pixels."""

    def __init__(self, wid, name, x, y, w, h):
        self.id, self.name, self.x, self.y, self.w, self.h = wid, name, x, y, w, h


def windows():
    out = []
    for line in run("xwininfo", "-root", "-tree").splitlines():
        parts = line.split()
        if not parts or not parts[0].startswith("0x"):
            continue
        geo = [p for p in parts if "x" in p and "+" in p and p[0].isdigit()]
        absolute = [p for p in parts if p.startswith("+") and p.count("+") == 2]
        if not geo or not absolute:
            continue
        size = geo[-1].split("+")[0]
        w, h = (int(v) for v in size.split("x"))
        ax, ay = (int(v) for v in absolute[-1].strip("+").replace("+-", "+-").split("+")[:2])
        name = line.split('"')[1] if '"' in line else ""
        out.append(Win(parts[0], name, ax, ay, w, h))
    return out


def window_info(wid):
    """A window's name, absolute position and size."""
    info = run("xwininfo", "-id", wid)
    field = lambda label: int(info.split(label)[1].split()[0])  # noqa: E731
    name = info.split('"')[1] if info.count('"') >= 2 else ""
    return Win(wid, name, field("Absolute upper-left X:"), field("Absolute upper-left Y:"), field("Width:"), field("Height:"))


def top_level():
    """Ids of the root window's children (the top-level windows)."""
    tree = run("xwininfo", "-root", "-children")
    return [line.split()[0] for line in tree.splitlines() if line.strip().startswith("0x")]


def wait_window(pred, known, timeout=20.0):
    """A top-level window not in `known` (one the app opened) matching `pred`."""
    end = time.time() + timeout
    while time.time() < end:
        for wid in top_level():
            if wid not in known:
                try:
                    w = window_info(wid)
                except subprocess.CalledProcessError:
                    continue  # closed meanwhile
                if pred(w):
                    return w
        time.sleep(0.2)
    raise RuntimeError("window did not appear")


# ----- the app -----------------------------------------------------------------


class App:
    """bluesnap in demo mode, with a private config directory."""

    def __init__(self, layout, ai=None, capture_after_ms=900):
        self.layout_dir = WORK / "layout"
        shutil.rmtree(self.layout_dir, ignore_errors=True)
        self.layout_dir.mkdir(parents=True)
        config = WORK / "config"
        shutil.rmtree(config, ignore_errors=True)
        (config / "bluesnap").mkdir(parents=True)
        if ai:
            path = config / "bluesnap" / "ai.json"
            path.write_text(json.dumps(ai))
            path.chmod(0o600)
        self.scroll_file = SCROLL_FILE
        self.scroll_file.write_text("0")
        d = desktop.DOCS_CONTENT
        env = dict(
            os.environ,
            GPUI_X11_SCALE_FACTOR=str(SCALE),
            XDG_CONFIG_HOME=str(config),
            BLUESNAP_DEMO_SCREEN=str(WORK / "desktop.png"),
            BLUESNAP_DEMO_SCALE=str(SCALE),
            BLUESNAP_DEMO_WINDOWS=";".join(",".join(map(str, w)) for w in layout.windows),
            BLUESNAP_DEMO_SCROLL=f"{WORK / 'page.png'}@{d[0]},{d[1]},{d[2]},{d[3] - 22}",
            BLUESNAP_DEMO_SCROLL_OFFSET=str(self.scroll_file),
            BLUESNAP_DEMO_CAPTURE_AFTER_MS=str(capture_after_ms),
            BLUESNAP_DEMO_LAYOUT=str(self.layout_dir),
            RUST_LOG="bluesnap=info,bluesnap_ai=info",
        )
        self.log = open(WORK / "app.log", "w")
        # Windows that aren't the app's (the backdrop, the compositor's).
        self.known = set(top_level())
        self.proc = subprocess.Popen([str(BIN)], env=env, stdout=self.log, stderr=subprocess.STDOUT)
        App.running.append(self)
        self.overlay = self.wait_window(lambda w: (w.w, w.h) == (desktop.W, desktop.H))
        time.sleep(0.6)

    def wait_window(self, pred, timeout=20.0):
        """One of the app's windows (not the backdrop's, or anyone else's)."""
        return wait_window(pred, self.known, timeout)

    def controls(self, window="capture"):
        path = self.layout_dir / f"{window}.json"
        for _ in range(50):
            try:
                data = json.loads(path.read_text())
                return data["scale"], data["controls"]
            except (OSError, ValueError, KeyError):
                time.sleep(0.1)
        raise RuntimeError(f"no layout for {window}")

    def logged(self, text):
        return text in (WORK / "app.log").read_text()

    running = []

    @classmethod
    def close_all(cls):
        for app in list(cls.running):
            app.close()

    def close(self):
        if self not in App.running:
            return
        App.running.remove(self)
        self.proc.terminate()
        try:
            self.proc.wait(5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.log.close()


# ----- recording ---------------------------------------------------------------


def ease(t):
    return 0.5 - math.cos(math.pi * t) / 2


class Rec:
    """Drives the app and records frames of `win`. Points are in device pixels
    relative to that window (for the overlay: screenshot pixels)."""

    def __init__(self, app, name):
        self.app, self.name = app, name
        self.win = app.overlay
        self.dir = WORK / "frames" / name
        shutil.rmtree(self.dir, ignore_errors=True)
        self.dir.mkdir(parents=True)
        self.frames = []
        self.pos = (desktop.W // 2, desktop.H // 2)
        self.kind = "cross"
        self.text = None
        self.focus = []  # rectangles the crop must include
        self.settle = 0.06
        self.mouse(self.pos)

    # input
    def mouse(self, p):
        run("xdotool", "mousemove", self.win.x + int(p[0]), self.win.y + int(p[1]))
        self.pos = p

    def frame(self, ms):
        """Captures the whole (private) screen: every window, as composed."""
        time.sleep(self.settle)
        path = self.dir / f"{len(self.frames):04d}.bmp"
        run("magick", "import", "-window", "root", f"bmp:{path}")
        absolute = (self.pos[0] + self.win.x, self.pos[1] + self.win.y)
        self.frames.append(dict(path=path, ms=ms, at=absolute, kind=self.kind, caption=self.text))

    def pause(self, ms):
        self.frame(ms)

    def caption(self, text):
        self.text = text

    def cursor(self, kind):
        self.kind = kind

    def goto(self, p, ms=600):
        start = self.pos
        steps = max(1, ms // 40)
        for i in range(1, steps + 1):
            t = ease(i / steps)
            self.mouse((start[0] + (p[0] - start[0]) * t, start[1] + (p[1] - start[1]) * t))
            self.frame(40)

    def switch_window(self, win):
        """Switches target window while keeping the cursor at the same screen pixel."""
        abs_x = self.pos[0] + self.win.x
        abs_y = self.pos[1] + self.win.y
        self.win = win
        self.pos = (abs_x - win.x, abs_y - win.y)

    def click(self, p=None, ms=500):
        if p:
            self.goto(p, ms)
        run("xdotool", "mousedown", "1")
        time.sleep(0.05)
        run("xdotool", "mouseup", "1")
        self.frame(160)

    def right_click(self, p=None, ms=500):
        if p:
            self.goto(p, ms)
        run("xdotool", "mousedown", "3")
        time.sleep(0.05)
        run("xdotool", "mouseup", "3")
        self.frame(180)

    def drag(self, a, b, ms=700, path=None):
        """Press at `a`, move to `b` (through `path` points if given), release."""
        self.goto(a)
        run("xdotool", "mousedown", "1")
        self.frame(80)
        for p in (path or []) + [b]:
            self.goto(p, ms // (len(path or []) + 1))
        run("xdotool", "mouseup", "1")
        self.frame(160)

    def focus_window(self):
        """Keyboard focus to `win` (there's no window manager to activate it)."""
        subprocess.run(["xdotool", "windowfocus", "--sync", self.win.id], capture_output=True)

    def key(self, name, ms=300):
        self.focus_window()
        run("xdotool", "key", name)
        self.frame(ms)

    def type(self, text, per_char=70):
        self.focus_window()
        for ch in text:
            run("xdotool", "type", "--delay", "0", ch)
            self.frame(per_char)

    def rect(self, name, window="capture"):
        """Bounds of a named control (see crates/app/src/demo.rs), relative to `win`."""
        controls = {}
        for _ in range(30):
            scale, controls = self.app.controls(window)
            if name in controls:
                r = tuple(v * scale for v in controls[name])
                self.include(r)
                return r
            time.sleep(0.08)
        raise KeyError(f"{name} not in {sorted(controls)}")

    def control(self, name, window="capture"):
        """Center of a named control."""
        return center(self.rect(name, window))

    def press(self, name, window="capture", ms=450):
        kind = self.kind
        self.cursor("arrow")
        self.click(self.control(name, window), ms)
        self.cursor(kind)

    def wait_until(self, done, timeout=60.0, every=250):
        end = time.time() + timeout
        while not done() and time.time() < end:
            self.frame(every)
        self.frame(every)

    def include(self, rect, margin=0):
        """Makes the GIF's crop cover `rect` (relative to `win`)."""
        x, y, w, h = rect
        self.focus.append((x + self.win.x - margin, y + self.win.y - margin, w + 2 * margin, h + 2 * margin))

    # output
    def save(self, crop=None, width=960):
        """Encodes the GIF. `crop` (absolute) defaults to everything in `focus`, padded."""
        if crop is None:
            xs = [r[0] for r in self.focus] + [r[0] + r[2] for r in self.focus]
            ys = [r[1] for r in self.focus] + [r[1] + r[3] for r in self.focus]
            x0, y0, x1, y1 = min(xs) - 60, min(ys) - 60, max(xs) + 60, max(ys) + 60
            crop = (x0, y0, x1 - x0, y1 - y0)
        x, y, w, h = (int(v) for v in crop)
        x, y = max(0, x), max(0, y)
        w, h = min(w, desktop.W - x), min(h, desktop.H - y)
        # GIF frames at `width`, even sizes for the encoder.
        out_w = min(width, w)
        out_h = round(h * out_w / w) // 2 * 2
        cursors = {k: cursor_sprite(k) for k in ("arrow", "cross")}
        frames_dir = self.dir / "png"
        frames_dir.mkdir()
        entries, last_hash = [], None
        for f in self.frames:
            img = Image.open(f["path"]).convert("RGB")
            # The cursor, where it was (relative to the window recorded then).
            sprite, (hx, hy) = cursors[f["kind"]]
            img.paste(sprite, (int(f["at"][0] - hx), int(f["at"][1] - hy)), sprite)
            img = img.crop((x, y, x + w, y + h)).resize((out_w, out_h), Image.LANCZOS)
            if f["caption"]:
                draw_caption(img, f["caption"])
            digest = hashlib.md5(img.tobytes()).hexdigest()
            if digest == last_hash:
                entries[-1][1] += f["ms"]
                continue
            last_hash = digest
            path = frames_dir / f"{len(entries):04d}.png"
            img.save(path)
            entries.append([path, f["ms"]])
        listing = self.dir / "frames.txt"
        with open(listing, "w") as fh:
            fh.write("ffconcat version 1.0\n")
            for path, ms in entries:
                fh.write(f"file '{path}'\nduration {ms / 1000:.3f}\n")
            fh.write(f"file '{entries[-1][0]}'\n")
        OUT.mkdir(parents=True, exist_ok=True)
        gif = OUT / f"{self.name}.gif"
        palette = "split[a][b];[a]palettegen=stats_mode=diff:max_colors=255[p];[b][p]paletteuse=dither=sierra2_4a:diff_mode=rectangle"
        run("ffmpeg", "-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", listing, "-vf", palette, "-loop", "0", gif)
        print(f"{gif.relative_to(ROOT)}: {out_w}x{out_h}, {len(entries)} frames, {gif.stat().st_size / 1e6:.1f} MB")


def cursor_sprite(kind):
    """A cursor drawn at device scale, and its hotspot."""
    s = SCALE
    if kind == "arrow":
        pts = [(0, 0), (0, 17), (4, 13), (7, 20), (9.5, 19), (6.5, 12), (12, 12)]
        img = Image.new("RGBA", (int(16 * s), int(24 * s)), (0, 0, 0, 0))
        d = ImageDraw.Draw(img)
        poly = [(1.5 * s + x * s, 1.5 * s + y * s) for x, y in pts]
        d.polygon(poly, fill=(20, 20, 20, 255), outline=(255, 255, 255, 255), width=int(1.4 * s))
        return img, (int(1.5 * s), int(1.5 * s))
    size = int(24 * s)
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    c = size // 2
    for width, color in ((int(3.2 * s), (255, 255, 255, 255)), (int(1.2 * s), (20, 20, 20, 255))):
        d.line((c, 2 * s, c, c - 3 * s), fill=color, width=width)
        d.line((c, c + 3 * s, c, size - 2 * s), fill=color, width=width)
        d.line((2 * s, c, c - 3 * s, c), fill=color, width=width)
        d.line((c + 3 * s, c, size - 2 * s, c), fill=color, width=width)
    return img, (c, c)


def draw_caption(img, text):
    d = ImageDraw.Draw(img)
    f = ImageFont.truetype(FONT, max(15, img.width // 52))
    tw = d.textlength(text, font=f)
    pad_x, pad_y = f.size * 0.9, f.size * 0.55
    w, h = tw + 2 * pad_x, f.size + 2 * pad_y
    x, y = (img.width - w) / 2, img.height - h - f.size * 0.8
    overlay = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(overlay).rounded_rectangle((x, y, x + w, y + h), h / 2, fill=(15, 15, 20, 215))
    img.paste(overlay, (0, 0), overlay)
    d.text((x + pad_x, y + pad_y - f.size * 0.08), text, font=f, fill=(255, 255, 255))


# ----- helpers for scenes ---------------------------------------------------------


def center(rect):
    x, y, w, h = rect
    return (x + w / 2, y + h / 2)


def ai_settings():
    url, token = os.environ.get("BLUESNAP_DEMO_AI_URL"), os.environ.get("BLUESNAP_DEMO_AI_TOKEN", "")
    llm, decisions = os.environ.get("BLUESNAP_DEMO_AI_LLM"), os.environ.get("BLUESNAP_DEMO_AI_DECISIONS")
    if not url or not (llm or decisions):
        return None
    endpoint = lambda model: model and {"url": url, "token": token, "model": model}  # noqa: E731
    return {"llm": endpoint(llm), "decisions": endpoint(decisions)}


if __name__ == "__main__":
    import scenes

    layout = desktop.build(WORK)
    wanted = sys.argv[1:] or list(scenes.ALL)
    display = private_display()
    try:
        for name in wanted:
            print(f"recording {name}…", flush=True)
            scenes.ALL[name](layout)
    finally:
        App.close_all()
        for proc in reversed(display):
            proc.terminate()
