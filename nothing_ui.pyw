"""Nothing Windows UI — system tray app for Nothing Ear (a).

Double-click to run (pythonw, no console). Lives in the tray; left-click the
icon to open the panel. Started with --tray (as Windows autostart does) it
stays hidden until the icon is clicked.
"""
import ctypes
import ctypes.wintypes
import json
import locale
import math
import os
import socket
import sys
import threading
import time
import winreg

import pystray
import webview
from PIL import Image, ImageDraw

from ear import (ST_BT_OFF, ST_CONNECTED, ST_CONNECTING, ST_DISCONNECTED, ST_UNPAIRED, Ear,
                 save_config)

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "i18n.json"), encoding="utf-8") as _f:
    I18N = json.load(_f)
STATUS_KEYS = {ST_BT_OFF: "st_bt_off", ST_CONNECTING: "st_connecting", ST_UNPAIRED: "st_unpaired",
               ST_DISCONNECTED: "tray_earbuds_off"}
APP_NAME = "Nothing Windows UI"
IPC_PORT = 47811
WIN_W, WIN_H = 360, 600
THEME_BG = {"light": "#F4F4F2", "oled": "#000000"}
RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"

user32 = ctypes.windll.user32
user32.GetWindowLongW.restype = ctypes.c_long
GWL_EXSTYLE, WS_EX_TOOLWINDOW, WS_EX_APPWINDOW = -20, 0x80, 0x40000
SWP_NOSIZE, SWP_NOZORDER, SWP_NOACTIVATE = 0x1, 0x4, 0x10
SW_HIDE, SW_SHOW = 0, 5
GA_ROOTOWNER = 3
SLIDE_IN_SECONDS, SLIDE_OUT_SECONDS = 0.22, 0.16
SM_CYSCREEN = 1
FLYOUT_MARGIN = 12  # gap to the screen edge / taskbar, like the Windows 11 quick settings panel

ANC_LABELS = {"off": "anc_label_off", "transparency": "anc_label_tr", "nc_low": "anc_label_low",
              "nc_mid": "anc_label_mid", "nc_high": "anc_label_high", "nc_adaptive": "anc_label_adaptive"}


def windows_language():
    """The Windows display language as a short code, e.g. "pt"."""
    name = locale.windows_locale.get(ctypes.windll.kernel32.GetUserDefaultUILanguage(), "en")
    return name.split("_")[0].lower()


# --- autostart --------------------------------------------------------------
def autostart_command():
    pyw = os.path.join(os.path.dirname(sys.executable), "pythonw.exe")
    if not os.path.exists(pyw):
        pyw = sys.executable
    return f'"{pyw}" "{os.path.abspath(__file__)}" --tray'


def get_autostart():
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY) as k:
            return winreg.QueryValueEx(k, APP_NAME)[0] == autostart_command()
    except OSError:
        return False


def set_autostart(enabled):
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY, 0, winreg.KEY_SET_VALUE) as k:
        if enabled:
            winreg.SetValueEx(k, APP_NAME, 0, winreg.REG_SZ, autostart_command())
        else:
            try:
                winreg.DeleteValue(k, APP_NAME)
            except OSError:
                pass


# --- tray icon drawing (dot-matrix) -----------------------------------------
DIGITS = {  # 3x5 pixel font
    "0": ["111", "101", "101", "101", "111"], "1": ["010", "110", "010", "010", "111"],
    "2": ["111", "001", "111", "100", "111"], "3": ["111", "001", "111", "001", "111"],
    "4": ["101", "101", "111", "001", "001"], "5": ["111", "100", "111", "001", "111"],
    "6": ["111", "100", "111", "101", "111"], "7": ["111", "001", "010", "010", "010"],
    "8": ["111", "101", "111", "101", "111"], "9": ["111", "101", "111", "001", "111"],
}
RED = (215, 25, 33, 255)


def taskbar_is_light():
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                            r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize") as k:
            return winreg.QueryValueEx(k, "SystemUsesLightTheme")[0] == 1
    except OSError:
        return False


def make_icon(level=None, charging=False):
    """64px icon: battery % as dot-matrix digits, or a dotted ring when disconnected."""
    fg = (20, 20, 20, 255) if taskbar_is_light() else (245, 245, 245, 255)
    dim = fg[:3] + (90,)
    img = Image.new("RGBA", (64, 64), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    if level is None:
        for i in range(12):
            a = i * math.pi / 6
            x, y = 32 + 22 * math.cos(a), 32 + 22 * math.sin(a)
            d.ellipse([x - 4, y - 4, x + 4, y + 4], fill=dim)
        d.ellipse([25, 25, 39, 39], fill=RED)
        return img
    text = str(min(level, 99)).rjust(2)
    cell, dot = 9, 8  # 7 cols x 5 rows of dots
    ox, oy = (64 - 7 * cell) // 2 + 1, (64 - 5 * cell) // 2
    for ci, ch in enumerate(text):
        if ch == " ":
            continue
        for r, row in enumerate(DIGITS[ch]):
            for c, bit in enumerate(row):
                if bit == "1":
                    x, y = ox + (ci * 4 + c) * cell, oy + r * cell
                    d.ellipse([x, y, x + dot, y + dot], fill=fg)
    color = RED if (charging or level <= 15) else None
    if color:
        d.ellipse([52, 0, 63, 11], fill=color)
    return img


# --- app ----------------------------------------------------------------------
class App:
    def __init__(self, start_hidden):
        self.ear = Ear(on_change=self._on_change)
        self.window = None
        self._hwnd_cache = None
        self._ready = False
        self._auto_hidden_at = 0.0
        self._slide_id = 0
        self.icon = None
        self.visible = not start_hidden
        self._last_status = None
        self._notified_low = set()
        self._welcome_pending = False

    # Called from Ear threads
    def _on_change(self, what):
        st = self.ear.status
        if what == "status" and st != self._last_status:
            if st == ST_CONNECTED:
                self._welcome_pending = True
            elif st in (ST_DISCONNECTED, ST_BT_OFF) and self._last_status == ST_CONNECTED:
                self.notify(self.t("n_bt_off" if st == ST_BT_OFF else "n_disconnected"))
                self._notified_low.clear()
            self._last_status = st
        if what == "battery":
            bat = self.ear.state.get("battery", {})
            if self._welcome_pending and bat:
                self._welcome_pending = False
                self.notify(f"{self.t('n_connected')} · {self.battery_text()}")
            for side, key in (("left", "n_low_l"), ("right", "n_low_r")):
                b = bat.get(side)
                if b and b["level"] <= 15 and not b["charging"] and side not in self._notified_low:
                    self._notified_low.add(side)
                    self.notify(f"{self.t(key)}: {b['level']}%")
        self.refresh_tray()

    # --- language -------------------------------------------------------
    def lang(self):
        """The chosen language, or the Windows one; English if we have no translation for it."""
        choice = self.ear.cfg.get("language", "auto")
        code = windows_language() if choice == "auto" else choice
        return code if code in I18N else "en"

    def t(self, key):
        return I18N[self.lang()].get(key) or I18N["en"].get(key, key)

    def notify(self, msg):
        if self.icon and self.ear.cfg.get("notifications", True):
            try:
                self.icon.notify(msg, self.ear.display_name())
            except Exception:
                pass

    def battery_text(self):
        bat = self.ear.state.get("battery", {})
        parts = [f"{self.t(key)} {bat[k]['level']}%{'⚡' if bat[k]['charging'] else ''}"
                 for k, key in (("left", "left_s"), ("right", "right_s"), ("case", "case")) if k in bat]
        return " · ".join(parts) or self.t("tray_no_data")

    def status_text(self):
        if self.ear.status == ST_CONNECTED:
            return self.battery_text()
        return self.t(STATUS_KEYS.get(self.ear.status, "tray_earbuds_off"))

    def open_bluetooth_settings(self, *_):
        os.startfile("ms-settings:bluetooth")

    def refresh_tray(self):
        if not self.icon:
            return
        bat = self.ear.state.get("battery", {}) if self.ear.status == ST_CONNECTED else {}
        buds = [bat[k] for k in ("left", "right") if k in bat]
        if buds:
            low = min(buds, key=lambda b: b["level"])
            self.icon.icon = make_icon(low["level"], any(b["charging"] for b in buds))
        else:
            self.icon.icon = make_icon(None)
        self.icon.title = f"{self.ear.display_name()}\n{self.status_text()}"[:127]
        self.icon.update_menu()

    # --- window ---------------------------------------------------------
    def show(self, *_):
        self.visible = True
        hwnd = self._hwnd() if self._ready else None
        if not hwnd:
            return
        self._style_window()
        x, y = self._corner(hwnd)
        off_screen = user32.GetSystemMetrics(SM_CYSCREEN)
        self._move(hwnd, x, off_screen)
        user32.ShowWindow(hwnd, SW_SHOW)
        user32.SetForegroundWindow(hwnd)
        self._slide(hwnd, x, off_screen, y, SLIDE_IN_SECONDS, ease_out=True)

    def hide(self):
        was_visible, self.visible = self.visible, False
        hwnd = self._hwnd() if self._ready else None
        if not hwnd:
            return
        finished = True
        if was_visible:
            rect = ctypes.wintypes.RECT()
            user32.GetWindowRect(hwnd, ctypes.byref(rect))
            finished = self._slide(hwnd, rect.left, rect.top, user32.GetSystemMetrics(SM_CYSCREEN),
                                   SLIDE_OUT_SECONDS, ease_out=False)
        if finished:
            user32.ShowWindow(hwnd, SW_HIDE)

    def _move(self, hwnd, x, y):
        user32.SetWindowPos(hwnd, 0, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)

    def _slide(self, hwnd, x, y_from, y_to, seconds, ease_out):
        """Move the window vertically, like a Windows flyout (it passes behind the taskbar).

        Returns False if a newer show/hide took over before it finished.
        """
        self._slide_id += 1
        me = self._slide_id
        start = time.perf_counter()
        while True:
            p = min(1.0, (time.perf_counter() - start) / seconds)
            eased = 1 - (1 - p) ** 3 if ease_out else p ** 3
            if me != self._slide_id:
                return False
            self._move(hwnd, x, round(y_from + (y_to - y_from) * eased))
            if p >= 1.0:
                return True
            time.sleep(0.006)

    def toggle(self, *_):
        # Clicking the tray icon first steals focus (which auto-hides the panel);
        # don't reopen it straight away in that case.
        if not self.visible and time.time() - self._auto_hidden_at < 0.5:
            return
        self.hide() if self.visible else self.show()

    def _is_ours(self, fg, hwnd):
        if not fg:
            return False
        if fg == hwnd or user32.GetAncestor(fg, GA_ROOTOWNER) == hwnd:
            return True  # our window or one of its popups (e.g. an open dropdown)
        pid = ctypes.wintypes.DWORD()
        user32.GetWindowThreadProcessId(fg, ctypes.byref(pid))
        return pid.value == os.getpid()

    def _watch_focus(self):
        """Hide the panel when another window takes focus (click outside), like Windows flyouts."""
        had_focus = False
        while True:
            time.sleep(0.1)
            hwnd = self._hwnd() if self._ready else None
            if not (self.visible and hwnd):
                had_focus = False
                continue
            if self._is_ours(user32.GetForegroundWindow(), hwnd):
                had_focus = True
            elif had_focus:
                had_focus = False
                self._auto_hidden_at = time.time()
                self.hide()

    def _hwnd(self):
        """Our top-level window handle (found by title + our process id)."""
        if self._hwnd_cache and user32.IsWindow(self._hwnd_cache):
            return self._hwnd_cache
        found = []
        pid = os.getpid()

        @ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.wintypes.HWND, ctypes.wintypes.LPARAM)
        def cb(hwnd, _):
            owner = ctypes.wintypes.DWORD()
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
            if owner.value == pid:
                buf = ctypes.create_unicode_buffer(64)
                user32.GetWindowTextW(hwnd, buf, 64)
                if buf.value == APP_NAME:
                    found.append(hwnd)
                    return False
            return True

        user32.EnumWindows(cb, 0)
        self._hwnd_cache = found[0] if found else None
        return self._hwnd_cache

    def _style_window(self):
        """Tool window (no taskbar button / Alt+Tab entry) with native rounded corners."""
        hwnd = self._hwnd()
        if not hwnd:
            return False
        ex = user32.GetWindowLongW(hwnd, GWL_EXSTYLE)
        user32.SetWindowLongW(hwnd, GWL_EXSTYLE, (ex | WS_EX_TOOLWINDOW) & ~WS_EX_APPWINDOW)
        pref = ctypes.c_int(2)  # DWMWCP_ROUND
        try:
            ctypes.windll.dwmapi.DwmSetWindowAttribute(hwnd, 33, ctypes.byref(pref), 4)
        except OSError:
            pass
        return True

    def _corner(self, hwnd):
        """Where the window rests: the bottom-right corner of the work area (above the taskbar)."""
        work, win = ctypes.wintypes.RECT(), ctypes.wintypes.RECT()
        user32.SystemParametersInfoW(0x30, 0, ctypes.byref(work), 0)  # SPI_GETWORKAREA
        user32.GetWindowRect(hwnd, ctypes.byref(win))
        w, h = win.right - win.left, win.bottom - win.top
        margin = round(FLYOUT_MARGIN * user32.GetDpiForWindow(hwnd) / 96)
        return work.right - w - margin, work.bottom - h - margin

    def _on_started(self):
        # WinForms re-adds the taskbar button every time *it* shows the form, so show it once
        # off-screen to finish initialising, then do all show/hide through Win32 directly.
        self.window.show()
        for _ in range(100):
            if self._hwnd():
                break
            time.sleep(0.05)
        hwnd = self._hwnd()
        if hwnd:
            user32.ShowWindow(hwnd, SW_HIDE)
            self._style_window()
        self._ready = True
        threading.Thread(target=self._watch_focus, daemon=True).start()
        if self.visible:
            self.show()

    def quit(self, *_):
        self.ear.stop = True
        if self.icon:
            self.icon.stop()
        if self.window:
            self.window.destroy()
        os._exit(0)

    # --- tray menu ------------------------------------------------------
    def _anc_available(self, mode):
        caps = self.ear.capabilities()
        return (mode == "off" or mode in caps["levels"] or (mode == "transparency" and caps["transparency"])
                or (mode == "nc_high" and caps["anc"] and not caps["levels"]))

    def _anc_item(self, mode):
        return pystray.MenuItem(
            lambda _: self.t(ANC_LABELS[mode]),
            lambda *_: threading.Thread(target=self.ear.action, args=({"action": "anc", "mode": mode},),
                                        daemon=True).start(),
            checked=lambda _: self.ear.state.get("anc") == mode, radio=True,
            enabled=lambda _: self.ear.status == ST_CONNECTED, visible=lambda _: self._anc_available(mode))

    def _ear_action(self, name):
        threading.Thread(target=self.ear.action, args=({"action": name},), daemon=True).start()

    def _toggle_autostart(self, *_):
        on = not get_autostart()
        set_autostart(on)
        self.ear.cfg["autostart"] = on
        save_config(self.ear.cfg)

    def build_tray(self):
        menu = pystray.Menu(
            pystray.MenuItem(lambda _: self.t("tray_open"), self.toggle, default=True),
            pystray.MenuItem(lambda _: self.status_text(), None, enabled=False),
            pystray.MenuItem(lambda _: self.t("tray_bt"), self.open_bluetooth_settings,
                             visible=lambda _: self.ear.status == ST_BT_OFF),
            pystray.MenuItem(lambda _: self.t("disconnect"), lambda *_: self._ear_action("disconnect"),
                             visible=lambda _: self.ear.status == ST_CONNECTED),
            pystray.MenuItem(lambda _: self.t("connect"), lambda *_: self._ear_action("connect"),
                             visible=lambda _: self.ear.status == ST_DISCONNECTED),
            pystray.Menu.SEPARATOR,
            pystray.MenuItem(lambda _: self.t("noise"), pystray.Menu(*[self._anc_item(m) for m in
                             ("off", "transparency", "nc_adaptive", "nc_low", "nc_mid", "nc_high")])),
            pystray.Menu.SEPARATOR,
            pystray.MenuItem(lambda _: self.t("autostart"), self._toggle_autostart,
                             checked=lambda _: get_autostart()),
            pystray.MenuItem(lambda _: self.t("quit"), self.quit),
        )
        self.icon = pystray.Icon(APP_NAME, make_icon(None), APP_NAME, menu)

    # --- single instance ------------------------------------------------
    def _ipc_server(self, srv):
        while True:
            conn, _ = srv.accept()
            with conn:
                if conn.recv(16) == b"show":
                    self.show()

    def run(self):
        # Keep the autostart entry pointing at this copy of the app.
        if self.ear.cfg.get("autostart", True):
            try:
                set_autostart(True)
            except OSError:
                pass
        self.build_tray()
        self.icon.run_detached()
        self.refresh_tray()
        threading.Thread(target=self.ear.run, daemon=True).start()

        # Created hidden; _on_started turns it into a tool window, then shows it in the corner.
        theme = self.ear.cfg.get("theme", "light")
        self.window = webview.create_window(
            APP_NAME, os.path.join(HERE, "ui.html"), js_api=Api(self), width=WIN_W, height=WIN_H,
            x=-32000, y=-32000, frameless=True, easy_drag=False, resizable=False, hidden=True,
            background_color=THEME_BG.get(theme, THEME_BG["light"]), on_top=False)
        self.window.events.closing += self._on_closing
        # private_mode: no browser cache, so an updated ui.html is never hidden by a stale copy.
        webview.start(self._on_started, http_server=True, private_mode=True)

    def _on_closing(self):
        # Closing the window only hides it; the app keeps running in the tray.
        threading.Thread(target=self.hide, daemon=True).start()
        return False


class Api:
    """Methods callable from ui.html as window.pywebview.api.*"""

    def __init__(self, app):
        self._app = app

    def get_state(self):
        snap = self._app.ear.snapshot()
        snap["autostart"] = get_autostart()
        snap["notifications"] = self._app.ear.cfg.get("notifications", True)
        snap["theme"] = self._app.ear.cfg.get("theme", "light")
        snap["language"] = self._app.ear.cfg.get("language", "auto")
        snap["lang"] = self._app.lang()
        return snap

    def open_bluetooth_settings(self):
        self._app.open_bluetooth_settings()

    def action(self, body):
        return self._app.ear.action(body)

    def set_option(self, key, value):
        app = self._app
        if key == "autostart":
            set_autostart(bool(value))
            app.ear.cfg["autostart"] = bool(value)
        elif key == "notifications":
            app.ear.cfg["notifications"] = bool(value)
        elif key == "theme" and value in THEME_BG:
            app.ear.cfg["theme"] = value
        elif key == "language" and (value == "auto" or value in I18N):
            app.ear.cfg["language"] = value
        save_config(app.ear.cfg)
        app.refresh_tray()
        return True

    def hide(self):
        self._app.hide()

    def minimize(self):
        self._app.window.minimize()

    def quit(self):
        threading.Thread(target=self._app.quit, daemon=True).start()


def main():
    start_hidden = "--tray" in sys.argv
    srv = socket.socket()
    try:
        srv.bind(("127.0.0.1", IPC_PORT))
    except OSError:
        # Already running: ask that instance to show its window.
        try:
            with socket.create_connection(("127.0.0.1", IPC_PORT), timeout=2) as c:
                c.sendall(b"show")
        except OSError:
            pass
        return
    srv.listen(2)
    app = App(start_hidden)
    threading.Thread(target=app._ipc_server, args=(srv,), daemon=True).start()
    app.run()


if __name__ == "__main__":
    try:
        ctypes.windll.shcore.SetProcessDpiAwareness(2)
    except (AttributeError, OSError):
        pass
    main()
