"""The "real screen" behind the app on the private display: the fake desktop,
with the docs window's page scrolled by the offset in the scroll file. It's
exactly what demo mode captures, so the scrolling capture's frame and bar sit
over the content being captured, as on a real desktop.

Usage: backdrop.py <desktop.png> <page.png> <scroll-offset-file>
"""

import sys
import tkinter as tk
from pathlib import Path

from PIL import Image, ImageTk

import desktop


def main():
    screen, page, offset_file = Image.open(sys.argv[1]).convert("RGB"), Image.open(sys.argv[2]).convert("RGB"), Path(sys.argv[3])
    x, y, w, h = desktop.DOCS_CONTENT
    h -= 22  # above the window's rounded bottom corners, as in demo mode
    root = tk.Tk()
    root.overrideredirect(True)
    root.geometry(f"{screen.width}x{screen.height}+0+0")
    label = tk.Label(root, borderwidth=0, highlightthickness=0)
    label.pack()
    state = {"offset": None}

    def refresh():
        try:
            offset = int(offset_file.read_text().strip() or 0)
        except (OSError, ValueError):
            offset = 0
        offset = max(0, min(offset, page.height - h))
        if offset != state["offset"]:
            state["offset"] = offset
            frame = screen.copy()
            frame.paste(page.crop((0, offset, w, offset + h)), (x, y))
            state["photo"] = ImageTk.PhotoImage(frame)
            label.configure(image=state["photo"])
        root.after(30, refresh)

    refresh()
    root.mainloop()


if __name__ == "__main__":
    main()
