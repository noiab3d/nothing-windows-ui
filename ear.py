"""Nothing headset link: Bluetooth presence detection + the Nothing RFCOMM protocol.

Protocol (same one the Nothing X app uses, documented by the open-source
ear-web project): frames are
    55 60 01 | cmd (u16 LE) | payload len | 00 | op id | payload | CRC16/MODBUS (LE)
"""
import asyncio
import ctypes
import json
import math
import os
import socket
import struct
import threading
import time
import wave
import winsound
from ctypes import wintypes

HERE = os.path.dirname(os.path.abspath(__file__))
CONFIG_PATH = os.path.join(HERE, "config.json")

CMD_GET_BATTERY = 0xC007
CMD_GET_ANC = 0xC01E
CMD_GET_EQ = 0xC01F
CMD_GET_BASS = 0xC04E
CMD_GET_INEAR = 0xC00E
CMD_GET_LATENCY = 0xC041
CMD_GET_FIRMWARE = 0xC042
CMD_GET_GESTURES = 0xC018

CMD_SET_ANC = 0xF00F
CMD_SET_EQ = 0xF010
CMD_SET_BASS = 0xF051
CMD_SET_INEAR = 0xF004
CMD_SET_LATENCY = 0xF040
CMD_SET_GESTURE = 0xF003
CMD_RING = 0xF002
CMD_FIT_TEST = 0xF014
CMD_SET_TIME = 0xF00A

CMD_GET_DUAL = 0xC027
CMD_GET_DUAL_LIST = 0xC028
CMD_GET_CODEC = 0xC029
CMD_GET_CUSTOM_EQ = 0xC044
CMD_SET_DUAL = 0xF01A
CMD_SET_DUAL_DEVICE = 0xF01B
CMD_SET_CODEC = 0xF01C
CMD_SET_CUSTOM_EQ = 0xF041

CODEC_NAMES = ["AAC", "LHDC", "LDAC"]
BASS_WIRE_3_STAGE = [0, 5, 10]
TESTED_MODEL = "B162"  # the only model this app has actually been run against
IOCTL_BTH_DISCONNECT_DEVICE = 0x41000C  # bthioctl.h

# ANC wire value <-> name
ANC_MODES = {0x05: "off", 0x07: "transparency", 0x03: "nc_low", 0x02: "nc_mid",
             0x01: "nc_high", 0x04: "nc_adaptive"}
ANC_WIRE = {v: k for k, v in ANC_MODES.items()}

# Status values shown in the UI
ST_BT_OFF = "bluetooth desligado"
ST_UNPAIRED = "sem fones"
ST_DISCONNECTED = "desligado"
ST_CONNECTING = "a ligar"
ST_CONNECTED = "ligado"


def crc16(data):
    crc = 0xFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ 0xA001 if crc & 1 else crc >> 1
    return crc


def load_config():
    try:
        with open(CONFIG_PATH, encoding="utf-8-sig") as f:  # tolerate a BOM from hand edits
            return json.load(f)
    except (OSError, ValueError):
        return {}


def save_config(cfg):
    try:
        with open(CONFIG_PATH, "w", encoding="utf-8") as f:
            json.dump(cfg, f, indent=2)
    except OSError:
        pass


# --- Bluetooth presence (is the headset connected to this PC?) ---------------
class _SYSTEMTIME(ctypes.Structure):
    _fields_ = [(n, wintypes.WORD) for n in ("y", "m", "dow", "d", "h", "mi", "s", "ms")]


class _BT_DEVICE_INFO(ctypes.Structure):
    _fields_ = [("dwSize", wintypes.DWORD), ("Address", ctypes.c_ulonglong),
                ("ulClassofDevice", wintypes.ULONG), ("fConnected", wintypes.BOOL),
                ("fRemembered", wintypes.BOOL), ("fAuthenticated", wintypes.BOOL),
                ("stLastSeen", _SYSTEMTIME), ("stLastUsed", _SYSTEMTIME),
                ("szName", wintypes.WCHAR * 248)]


class _BT_SEARCH_PARAMS(ctypes.Structure):
    _fields_ = [("dwSize", wintypes.DWORD), ("fReturnAuthenticated", wintypes.BOOL),
                ("fReturnRemembered", wintypes.BOOL), ("fReturnUnknown", wintypes.BOOL),
                ("fReturnConnected", wintypes.BOOL), ("fIssueInquiry", wintypes.BOOL),
                ("cTimeoutMultiplier", ctypes.c_ubyte), ("hRadio", wintypes.HANDLE)]


try:
    _bthprops = ctypes.WinDLL("bthprops.cpl")
    _bthprops.BluetoothFindFirstDevice.restype = wintypes.HANDLE
    _bthprops.BluetoothFindNextDevice.argtypes = [wintypes.HANDLE, ctypes.c_void_p]
    _bthprops.BluetoothFindDeviceClose.argtypes = [wintypes.HANDLE]
    _bthprops.BluetoothFindFirstRadio.restype = wintypes.HANDLE
    _bthprops.BluetoothFindRadioClose.argtypes = [wintypes.HANDLE]
except OSError:
    _bthprops = None

_kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
_kernel32.DeviceIoControl.argtypes = [wintypes.HANDLE, wintypes.DWORD, ctypes.c_void_p, wintypes.DWORD,
                                      ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD),
                                      ctypes.c_void_p]
_kernel32.CloseHandle.argtypes = [wintypes.HANDLE]

# What each model supports, derived from the open-source ear-web project (tools/make_models.py).
try:
    with open(os.path.join(HERE, "models.json"), encoding="utf-8") as _f:
        _DATA = json.load(_f)
except (OSError, ValueError):
    _DATA = {"ids": {}, "models": {}}
MODEL_IDS, MODELS = _DATA["ids"], _DATA["models"]
_NAME_TO_BASE = {m["name"].lower(): base for base, m in MODELS.items()}
_MODEL_NAMES = set(_NAME_TO_BASE)


class _BT_RADIO_PARAMS(ctypes.Structure):
    _fields_ = [("dwSize", wintypes.DWORD)]


def bt_disconnect(mac):
    """Ask Windows to drop the Bluetooth link to a device. True if the request was accepted."""
    if not _bthprops:
        return False
    params = _BT_RADIO_PARAMS(ctypes.sizeof(_BT_RADIO_PARAMS))
    radio = wintypes.HANDLE()
    find = _bthprops.BluetoothFindFirstRadio(ctypes.byref(params), ctypes.byref(radio))
    if not find:
        return False
    addr = ctypes.c_ulonglong(int(mac.replace(":", ""), 16))
    returned = wintypes.DWORD()
    ok = _kernel32.DeviceIoControl(radio, IOCTL_BTH_DISCONNECT_DEVICE, ctypes.byref(addr), 8,
                                   None, 0, ctypes.byref(returned), None)
    _kernel32.CloseHandle(radio)
    _bthprops.BluetoothFindRadioClose(find)
    return bool(ok)

try:
    from winrt.windows.devices.radios import Radio, RadioKind, RadioState
except ImportError:
    Radio = None


def bt_radio_on():
    """True/False if the PC's Bluetooth is on/off (False also when there is no adapter); None if unknown."""
    if Radio is None:
        return None

    async def states():
        return [r.state for r in await Radio.get_radios_async() if r.kind == RadioKind.BLUETOOTH]

    try:
        found = asyncio.run(states())
    except Exception:
        return None
    return any(s == RadioState.ON for s in found)


def bt_connected(mac):
    """True/False if the device's Bluetooth link to this PC is up; None if unknown."""
    if not _bthprops:
        return None
    info = _BT_DEVICE_INFO()
    info.dwSize = ctypes.sizeof(info)
    info.Address = int(mac.replace(":", ""), 16)
    if _bthprops.BluetoothGetDeviceInfo(None, ctypes.byref(info)) != 0:
        return None
    return bool(info.fConnected)


def _chime_path(rising):
    """Short two-note blip (rising = on, falling = off), generated once as a WAV next to the app."""
    path = os.path.join(HERE, "sounds", f"chime_{'on' if rising else 'off'}.wav")
    if os.path.exists(path):
        return path
    rate, notes = 44100, [(880, 0.075), (1318.5, 0.11)]
    if not rising:
        notes = [(1318.5, 0.075), (880, 0.11)]
    frames = bytearray()
    for freq, dur in notes:
        n = int(rate * dur)
        for i in range(n):
            env = min(1.0, i / (rate * 0.004)) * (1 - i / n) ** 1.5  # quick attack, soft decay
            v = 0.22 * env * math.sin(2 * math.pi * freq * i / rate)
            frames += struct.pack("<h", int(v * 32767))
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(bytes(frames))
    return path


def play_chime(rising):
    time.sleep(0.4)  # let the buds switch audio mode first, or the start of the sound gets cut
    try:
        winsound.PlaySound(_chime_path(rising), winsound.SND_FILENAME | winsound.SND_NODEFAULT)
    except (OSError, RuntimeError):
        pass


def is_nothing_headset(name):
    low = name.lower()
    if low in _MODEL_NAMES:
        return True
    return ("nothing" in low or "cmf" in low) and any(
        w in low for w in ("ear", "buds", "headphone", "neckband", "clip"))


def find_paired_ear():
    """(name, MAC) of a paired Nothing / CMF headset, preferring one connected right now."""
    if not _bthprops:
        return None
    params = _BT_SEARCH_PARAMS(dwSize=ctypes.sizeof(_BT_SEARCH_PARAMS), fReturnAuthenticated=True,
                               fReturnRemembered=True, fReturnConnected=True)
    info = _BT_DEVICE_INFO()
    info.dwSize = ctypes.sizeof(info)
    found = []
    handle = _bthprops.BluetoothFindFirstDevice(ctypes.byref(params), ctypes.byref(info))
    if not handle:
        return None
    while True:
        if is_nothing_headset(info.szName):
            raw = f"{info.Address:012X}"
            found.append((not info.fConnected, info.szName, ":".join(raw[i:i + 2] for i in range(0, 12, 2))))
        if not _bthprops.BluetoothFindNextDevice(handle, ctypes.byref(info)):
            break
    _bthprops.BluetoothFindDeviceClose(handle)
    found.sort(key=lambda d: d[0])
    return (found[0][1], found[0][2]) if found else None


def parse_model_id(data):
    """Model id from a Fast Pair message stream: group 3, code 1, 3-byte value."""
    i = 0
    while i + 4 <= len(data):
        length = data[i + 2] << 8 | data[i + 3]
        if data[i] == 3 and data[i + 1] == 1 and length == 3 and i + 7 <= len(data):
            return data[i + 4:i + 7].hex().upper()
        i += 4 + length
    return None


class Ear:
    def __init__(self, on_change=None):
        self.cfg = load_config()
        self.on_change = on_change or (lambda ev: None)
        self.sock = None
        self.lock = threading.Lock()
        self.op = 0
        self.status = ST_CONNECTING
        self.state = {}
        self.fit = None
        self.stop = False
        self._buf = b""
        self._dual = []
        self._no_connect_until = 0.0
        self._connect_requested = False

    # --- connection -----------------------------------------------------
    def _packet(self, cmd, payload=b""):
        self.op = self.op % 250 + 1
        h = bytes([0x55, 0x60, 0x01, cmd & 0xFF, cmd >> 8, len(payload), 0, self.op]) + bytes(payload)
        c = crc16(h)
        return h + bytes([c & 0xFF, c >> 8])

    def _try_channel(self, mac, ch, timeout=4):
        s = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM, socket.BTPROTO_RFCOMM)
        s.settimeout(timeout)
        try:
            s.connect((mac, ch))
            s.send(self._packet(CMD_GET_BATTERY))
            data = s.recv(1024)
            if data[:3] == b"\x55\x60\x01":
                return s, data
        except OSError:
            pass
        s.close()
        return None, None

    def _set_status(self, st):
        if st != self.status:
            self.status = st
            self.on_change("status")

    def _connect(self):
        mac = self.cfg["mac"]
        self._set_status(ST_CONNECTING)
        channels = list(range(1, 31))
        if self.cfg.get("channel") in channels:
            channels.remove(self.cfg["channel"])
            channels.insert(0, self.cfg["channel"])
        for ch in channels:
            # Opening the channel also brings a dropped link back, but only do that when asked.
            if bt_connected(mac) is False and not self._connect_requested:
                break
            s, first = self._try_channel(mac, ch)
            if s:
                if self.cfg.get("channel") != ch:
                    self.cfg["channel"] = ch
                    save_config(self.cfg)
                s.settimeout(None)
                self.sock = s
                self._buf = b""
                self._handle_stream(first)
                self._set_status(ST_CONNECTED)
                return True
        return False

    def send(self, cmd, payload=b""):
        with self.lock:
            if not self.sock:
                return False
            try:
                self.sock.sendall(self._packet(cmd, payload))
                return True
            except OSError:
                self._drop()
                return False

    def _drop(self):
        try:
            if self.sock:
                self.sock.close()
        except OSError:
            pass
        self.sock = None
        self.state.pop("battery", None)

    def refresh_all(self):
        self.send(CMD_SET_TIME, int(time.time()).to_bytes(4, "big"))
        # The buds reset low-latency mode whenever they disconnect, so re-apply the user's choice.
        if self.cfg.get("low_latency"):
            self.send(CMD_SET_LATENCY, bytes([0x01, 0x00]))
            time.sleep(0.08)
        spec = self.spec()
        cmds = [CMD_GET_BATTERY, CMD_GET_LATENCY, CMD_GET_FIRMWARE, CMD_GET_GESTURES]
        cmds += [CMD_GET_ANC] if spec["anc"] else []
        cmds += [CMD_GET_EQ] if spec["eq_presets"] else []
        cmds += [CMD_GET_BASS] if spec["bass_levels"] else []
        cmds += [CMD_GET_INEAR] if spec["in_ear"] else []
        cmds += [CMD_GET_DUAL] if spec["dual"] else []
        cmds += [CMD_GET_CODEC] if spec["hq"] else []
        cmds += [CMD_GET_CUSTOM_EQ] if spec["custom_eq"] else []
        for cmd in cmds:
            self.send(cmd)
            time.sleep(0.08)
        if spec["dual"]:
            self._request_dual_list(restart=True)

    # --- receiving ------------------------------------------------------
    def _handle_stream(self, data):
        buf = self._buf + data
        while True:
            i = buf.find(b"\x55")
            if i < 0:
                buf = b""
                break
            buf = buf[i:]
            if len(buf) < 8:
                break
            # 55 60 01 = reply with a CRC16 trailer; 55 00 01 = unsolicited push (e.g. the
            # fit-test result) with no CRC at all.
            with_crc = buf[1] == 0x60
            if buf[1] not in (0x60, 0x00) or buf[2] != 0x01:
                buf = buf[1:]
                continue
            plen = buf[5] | (buf[6] << 8)
            n = 8 + plen + (2 if with_crc else 0)
            if len(buf) < n:
                break
            frame, buf = buf[:n], buf[n:]
            if with_crc and crc16(frame[:-2]) != frame[-2] | (frame[-1] << 8):
                buf = frame[1:] + buf
                continue
            try:
                self._parse(frame[3] | (frame[4] << 8), frame[8:8 + plen])
            except (IndexError, ValueError):
                pass
        self._buf = buf

    def _parse(self, cmd, p):
        st = self.state
        if cmd in (0x4007, 0xE001):
            names = {2: "left", 3: "right", 4: "case", 6: "single"}
            bat = {}
            for k in range(p[0]):
                dev, val = p[1 + 2 * k], p[2 + 2 * k]
                if dev in names:
                    bat[names[dev]] = {"level": val & 0x7F, "charging": bool(val & 0x80)}
            st["battery"] = bat
            # The case only reports through a docked bud, so remember its last reading.
            case = bat.get("case")
            last = self.cfg.get("case_last") or {}
            if case and (case["level"] != last.get("level") or time.time() - last.get("time", 0) > 300):
                self.cfg["case_last"] = {"level": case["level"], "time": time.time()}
                save_config(self.cfg)
            self.on_change("battery")
        elif cmd in (0x401E, 0xE003):
            st["anc"] = ANC_MODES.get(p[1], "unknown")
            self.on_change("anc")
        elif cmd in (0x401F, 0x4050):
            st["eq"] = p[0]
        elif cmd == 0x404E:
            wire = p[1]
            level = (BASS_WIRE_3_STAGE.index(wire) + 1 if wire in BASS_WIRE_3_STAGE else 1) \
                if self.spec()["bass_levels"] == 3 else wire // 2
            st["bass"] = {"enabled": p[0] == 1, "level": level}
        elif cmd == 0x400E:
            st["inear"] = p[2] == 1
        elif cmd == 0x4041:
            st["latency"] = p[0] == 1
        elif cmd == 0x4042:
            st["firmware"] = p.decode("ascii", "replace")
        elif cmd == 0x4018:
            g = {}
            for k in range(p[0]):
                dev, button, gtype, action = p[1 + 4 * k:5 + 4 * k]
                side = {2: "left", 3: "right", 6: "single"}.get(dev)
                if side:
                    g.setdefault(side, {})[f"{button}_{gtype}"] = action
            st["gestures"] = g
        elif cmd == 0xE00D:
            self.fit = {"left": p[0], "right": p[1], "time": time.time()}
        elif cmd == 0x4027:
            st["dual"] = p[0] == 1
        elif cmd == 0x4028:
            self._read_dual_list(p)
        elif cmd == 0xE00E:
            self._request_dual_list(restart=True)
        elif cmd == 0x4029:
            st["codec"] = p[0] if p[0] < len(CODEC_NAMES) else 0
        elif cmd == 0x4044:
            # [count][total gain f32][per band: type u8, gain f32, freq f32, q f32]; peak, high, low
            gains = [round(struct.unpack_from("<f", p, 6 + 13 * i)[0]) for i in range(3)]
            st["custom_eq"] = {"bass": gains[2], "mid": gains[0], "treble": gains[1]}

    def _request_dual_list(self, restart=False):
        if restart:
            self._dual = []
        self.send(CMD_GET_DUAL_LIST, bytes([len(self._dual) & 0xFF]))

    def _read_dual_list(self, p):
        """Paged list of devices the buds know: flags (self / connected), MAC, name."""
        if len(p) < 3:
            return
        offset, added = 3, False
        for _ in range(p[2]):
            if offset + 8 > len(p):
                break
            flags, mac = p[offset], p[offset + 1:offset + 7]
            name_len = p[offset + 7] & 0x7F
            device = {"mac": mac.hex("-").upper(), "self": bool(flags & 0xF0), "connected": bool(flags & 0x0F),
                      "name": p[offset + 8:offset + 8 + name_len].decode("utf-8", "replace")}
            known = next((i for i, d in enumerate(self._dual) if d["mac"] == device["mac"]), None)
            if known is None:
                self._dual.append(device)
                added = True
            else:
                self._dual[known] = device
            offset += 8 + name_len
        self.state["dual_list"] = list(self._dual)
        if added and len(self._dual) < 20:
            self._request_dual_list()

    # --- which headset is this? ------------------------------------------
    def _pick_headset(self):
        """New PC or new headset: follow whichever Nothing device is paired / connected."""
        found = find_paired_ear()
        cached = self.cfg.get("mac")
        if not found or found[1] == cached or (cached and bt_connected(cached)):
            return
        for key in ("channel", "fast_pair_channel", "model_id", "case_last"):
            self.cfg.pop(key, None)
        self.cfg["name"], self.cfg["mac"] = found
        self.state.clear()
        save_config(self.cfg)
        self.on_change("name")

    def _detect_model(self):
        """Ask the headset itself: on connect its Fast Pair channel announces a model id.

        Windows only knows the Bluetooth name, which can be renamed. Python can't resolve a
        service UUID to a channel, so look for the one that announces it; remembered afterwards.
        """
        mac = self.cfg["mac"]
        channels = [c for c in range(1, 31) if c != self.cfg.get("channel")]
        for ch in channels:
            if not self.sock or self.cfg.get("mac") != mac:
                return
            s = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM, socket.BTPROTO_RFCOMM)
            s.settimeout(3)
            try:
                s.connect((mac, ch))
                s.settimeout(1.5)
                model = parse_model_id(s.recv(1024))
            except OSError:
                model = None
            finally:
                s.close()
            if model:
                self.cfg["model_id"] = model
                self.cfg["fast_pair_channel"] = ch
                save_config(self.cfg)
                self.on_change("name")
                self.refresh_all()  # now we know what this model supports
                return

    def _base(self):
        return MODEL_IDS.get(self.cfg.get("model_id")) or _NAME_TO_BASE.get((self.cfg.get("name") or "").lower())

    def spec(self):
        """This model's capabilities; an unknown model is treated like the one we've tested."""
        return MODELS.get(self._base()) or MODELS.get(TESTED_MODEL) or {
            "anc": 0, "fit": False, "bass_levels": 0, "dual": False, "dual_reboot": False, "hq": 0, "custom_eq": None,
            "eq_presets": False, "in_ear": False, "ring": "sides", "single": False, "gestures": []}

    def display_name(self):
        model = MODELS.get(self._base())
        return (model and model["name"]) or self.cfg.get("name") or "Nothing"

    def capabilities(self):
        spec, anc = self.spec(), self.spec()["anc"]
        levels = [m for bit, m in ((0x08, "nc_adaptive"), (0x04, "nc_low"), (0x02, "nc_mid"), (0x04, "nc_high"))
                  if anc & bit]
        return {
            "anc": bool(anc), "transparency": bool(anc & 0x10), "levels": levels,
            "fit": spec["fit"], "bass_levels": spec["bass_levels"], "in_ear": spec["in_ear"],
            "eq_presets": spec["eq_presets"], "custom_eq": bool(spec["custom_eq"]),
            "dual": spec["dual"], "dual_reboot": spec["dual_reboot"], "single": spec["single"],
            "codecs": [i for i, bit in enumerate((0, 2, 4)) if i == 0 or spec["hq"] & bit] if spec["hq"] else [],
            "codec_names": CODEC_NAMES, "gestures": spec["gestures"], "ring": spec["ring"],
            "tested": self._base() in (TESTED_MODEL, None),
        }

    # --- worker threads -------------------------------------------------
    def run(self):
        """Main loop: watch the Bluetooth link and keep the RFCOMM session up."""
        threading.Thread(target=self._reader, daemon=True).start()
        threading.Thread(target=self._poller, daemon=True).start()
        while not self.stop:
            if bt_radio_on() is False:
                if self.sock:
                    with self.lock:
                        self._drop()
                self._set_status(ST_BT_OFF)
                time.sleep(2)
                continue
            if not self.sock:
                self._pick_headset()
            if not self.cfg.get("mac"):
                self._set_status(ST_UNPAIRED)
                time.sleep(5)
                continue
            linked = bt_connected(self.cfg["mac"])
            if self.sock:
                if linked is False:
                    with self.lock:
                        self._drop()
                    self._set_status(ST_DISCONNECTED)
            elif (linked is False and not self._connect_requested) or time.time() < self._no_connect_until:
                self._set_status(ST_DISCONNECTED)
            elif self._connect():
                self._connect_requested = False
                threading.Thread(target=self.refresh_all, daemon=True).start()
                if not self.cfg.get("model_id"):
                    threading.Thread(target=self._detect_model, daemon=True).start()
            else:
                self._connect_requested = False
                self._set_status(ST_DISCONNECTED)
                time.sleep(3)
            time.sleep(1.5)

    def _reader(self):
        while not self.stop:
            sock = self.sock
            if not sock:
                time.sleep(0.3)
                continue
            try:
                data = sock.recv(4096)
                if not data:
                    raise OSError("closed")
                self._handle_stream(data)
            except OSError:
                if sock is self.sock:
                    with self.lock:
                        self._drop()
                    self._set_status(ST_DISCONNECTED)

    def _poller(self):
        while not self.stop:
            time.sleep(30)
            self.send(CMD_GET_BATTERY)

    # --- actions from the UI / tray -------------------------------------
    def action(self, a):
        kind = a.get("action")
        if kind == "anc" and a.get("mode") in ANC_WIRE:
            self.send(CMD_SET_ANC, bytes([0x01, ANC_WIRE[a["mode"]], 0x00]))
            self.state["anc"] = a["mode"]
            self.on_change("anc")
            time.sleep(0.3)
            self.send(CMD_GET_ANC)
        elif kind == "eq":
            self.send(CMD_SET_EQ, bytes([int(a["preset"]), 0x00]))
            self.state["eq"] = int(a["preset"])
            time.sleep(0.2)
            self.send(CMD_GET_EQ)
        elif kind == "bass":
            three_stage = self.spec()["bass_levels"] == 3
            lvl = max(1, min(3 if three_stage else 5, int(a["level"])))
            wire = BASS_WIRE_3_STAGE[lvl - 1] if three_stage else lvl * 2
            self.send(CMD_SET_BASS, bytes([1 if a["enabled"] else 0, wire]))
            self.state["bass"] = {"enabled": bool(a["enabled"]), "level": lvl}
            time.sleep(0.2)
            self.send(CMD_GET_BASS)
        elif kind == "inear":
            self.send(CMD_SET_INEAR, bytes([0x01, 0x01, 1 if a["enabled"] else 0]))
            self.state["inear"] = bool(a["enabled"])
            time.sleep(0.2)
            self.send(CMD_GET_INEAR)
        elif kind == "latency":
            self.send(CMD_SET_LATENCY, bytes([1 if a["enabled"] else 2, 0x00]))
            self.state["latency"] = bool(a["enabled"])
            self.cfg["low_latency"] = bool(a["enabled"])
            save_config(self.cfg)
            # The buds chime for noise-control changes but not for this, so chime from the PC.
            threading.Thread(target=play_chime, args=(bool(a["enabled"]),), daemon=True).start()
            time.sleep(0.2)
            self.send(CMD_GET_LATENCY)
        elif kind == "gesture":
            dev = {"left": 0x02, "right": 0x03, "single": 0x06}[a["side"]]
            self.send(CMD_SET_GESTURE, bytes([0x01, dev, int(a.get("button", 1)), int(a["type"]), int(a["value"])]))
            time.sleep(0.3)
            self.send(CMD_GET_GESTURES)
        elif kind == "ring":
            on = 1 if a["on"] else 0
            ring = self.spec()["ring"]
            if ring == "all":
                self.send(CMD_RING, bytes([on]))
            elif ring == "single":
                self.send(CMD_RING, bytes([0x06, on]))
            else:
                self.send(CMD_RING, bytes([0x02 if a["side"] == "left" else 0x03, on]))
        elif kind == "fit_test":
            self.fit = None
            self.send(CMD_FIT_TEST, bytes([0x01]))
        elif kind == "dual":
            self.send(CMD_SET_DUAL, bytes([1 if a["enabled"] else 0]))
            self.state["dual"] = bool(a["enabled"])
            time.sleep(0.3)
            self.send(CMD_GET_DUAL)
            if a["enabled"]:
                self._request_dual_list(restart=True)
        elif kind == "dual_device":
            mac = bytes(int(b, 16) for b in a["mac"].split("-"))
            self.send(CMD_SET_DUAL_DEVICE, bytes([1 if a["connect"] else 0]) + mac)
            time.sleep(1.0)
            self._request_dual_list(restart=True)
        elif kind == "codec" and int(a["index"]) in self.capabilities()["codecs"]:
            # The buds restart to apply this; the normal reconnect picks them up again.
            self.send(CMD_SET_CODEC, bytes([int(a["index"])]))
            self.state["codec"] = int(a["index"])
        elif kind == "custom_eq" and self.spec()["custom_eq"]:
            self._send_custom_eq(int(a["bass"]), int(a["mid"]), int(a["treble"]))
        elif kind == "disconnect":
            return {"ok": self.disconnect()}
        elif kind == "connect":
            self._no_connect_until = 0.0
            self._connect_requested = True
        return {"ok": True}

    def _send_custom_eq(self, bass, mid, treble):
        c = self.spec()["custom_eq"]
        clamp = lambda v: max(-6, min(6, v))
        bass, mid, treble = clamp(bass), clamp(mid), clamp(treble)
        bands = [(1, mid, c["freqPeak"], c["qPeak"]), (2, treble, c["freqHigh"], c["qHigh"]),
                 (0, bass, c["freqLow"], c["qLow"])]
        payload = bytes([len(bands)]) + struct.pack("<f", -max(0.0, bass, mid, treble))
        for kind, gain, freq, q in bands:
            payload += bytes([kind]) + struct.pack("<fff", gain, freq, q)
        self.send(CMD_SET_CUSTOM_EQ, payload + bytes(3 * len(bands)))
        self.state["custom_eq"] = {"bass": bass, "mid": mid, "treble": treble}

    def disconnect(self):
        """Drop the PC's Bluetooth link, e.g. so the buds are free to connect to a phone."""
        mac = self.cfg.get("mac")
        if not mac:
            return False
        self._no_connect_until = time.time() + 8  # our own reconnect would bring the link right back
        with self.lock:
            self._drop()
        self._set_status(ST_DISCONNECTED)
        return bt_disconnect(mac)

    def snapshot(self):
        return {"status": self.status, "name": self.display_name(),
                "state": self.state, "fit": self.fit, "case_last": self.cfg.get("case_last"),
                "caps": self.capabilities()}
