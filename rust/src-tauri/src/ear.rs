//! Nothing headset link: Bluetooth presence detection + the Nothing RFCOMM protocol.
//!
//! Frames (same protocol as the Nothing X app, documented by the open-source ear-web project):
//!     55 60 01 | cmd (u16 LE) | payload len (u16 LE) | op id | payload | CRC16/MODBUS (LE)
//! Unsolicited pushes start 55 00 01 and carry no CRC.
use std::collections::BTreeMap;
use std::fs;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::{sleep, spawn};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use windows::core::{GUID, PCWSTR};
use windows::Devices::Radios::{Radio, RadioKind, RadioState};
use windows::Win32::Devices::Bluetooth::{
    BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindFirstRadio, BluetoothFindNextDevice,
    BluetoothFindRadioClose, BluetoothGetDeviceInfo, AF_BTH, BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS,
    BLUETOOTH_FIND_RADIO_PARAMS, BTHPROTO_RFCOMM, SOCKADDR_BTH,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::Networking::WinSock::{
    closesocket, connect, recv, send, setsockopt, socket, WSAStartup, SEND_RECV_FLAGS, SOCKADDR, SOCKET, SOCK_STREAM,
    SOL_SOCKET, SO_RCVTIMEO, WSADATA,
};
use windows::Win32::System::IO::DeviceIoControl;

const CMD_GET_BATTERY: u16 = 0xC007;
const CMD_GET_ANC: u16 = 0xC01E;
const CMD_GET_EQ: u16 = 0xC01F;
const CMD_GET_BASS: u16 = 0xC04E;
const CMD_GET_INEAR: u16 = 0xC00E;
const CMD_GET_LATENCY: u16 = 0xC041;
const CMD_GET_FIRMWARE: u16 = 0xC042;
const CMD_GET_GESTURES: u16 = 0xC018;
const CMD_GET_DUAL: u16 = 0xC027;
const CMD_GET_DUAL_LIST: u16 = 0xC028;
const CMD_GET_CODEC: u16 = 0xC029;
const CMD_GET_CUSTOM_EQ: u16 = 0xC044;

const CMD_SET_ANC: u16 = 0xF00F;
const CMD_SET_EQ: u16 = 0xF010;
const CMD_SET_BASS: u16 = 0xF051;
const CMD_SET_INEAR: u16 = 0xF004;
const CMD_SET_LATENCY: u16 = 0xF040;
const CMD_SET_GESTURE: u16 = 0xF003;
const CMD_RING: u16 = 0xF002;
const CMD_FIT_TEST: u16 = 0xF014;
const CMD_SET_TIME: u16 = 0xF00A;
const CMD_SET_DUAL: u16 = 0xF01A;
const CMD_SET_DUAL_DEVICE: u16 = 0xF01B;
const CMD_SET_CODEC: u16 = 0xF01C;
const CMD_SET_CUSTOM_EQ: u16 = 0xF041;

/// The Nothing control service; Windows resolves it to the right RFCOMM channel via SDP.
const NOTHING_SERVICE: GUID = GUID::from_u128(0xaeac4a03_dff5_498f_843a_34487cf133eb);
/// Announces the headset's model id as soon as it is opened.
const FAST_PAIR_SERVICE: GUID = GUID::from_u128(0xdf21fe2c_2515_4fdb_8886_f12c4d67927c);
const IOCTL_BTH_DISCONNECT_DEVICE: u32 = 0x41000C; // bthioctl.h

const ANC_MODES: [(u8, &str); 6] = [
    (0x05, "off"),
    (0x07, "transparency"),
    (0x03, "nc_low"),
    (0x02, "nc_mid"),
    (0x01, "nc_high"),
    (0x04, "nc_adaptive"),
];
const CODEC_NAMES: [&str; 3] = ["AAC", "LHDC", "LDAC"];
const BASS_WIRE_3_STAGE: [u8; 3] = [0, 5, 10];
/// The only model this app has actually been run against.
const TESTED_MODEL: &str = "B162";

pub const ST_BT_OFF: &str = "bluetooth desligado";
pub const ST_UNPAIRED: &str = "sem fones";
pub const ST_DISCONNECTED: &str = "desligado";
pub const ST_CONNECTING: &str = "a ligar";
pub const ST_CONNECTED: &str = "ligado";

static MODEL_DATA: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../../../models.json")).unwrap_or(Value::Null));
static I18N: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../../../i18n.json")).unwrap_or(Value::Null));

/// What changed, reported to the tray.
#[derive(Clone, Copy, PartialEq)]
pub enum Change {
    Status,
    Battery,
    Anc,
    Name,
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for b in data {
        crc ^= *b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}

/// The Windows display language as a short code, e.g. "pt".
fn windows_language() -> &'static str {
    match unsafe { GetUserDefaultUILanguage() } & 0x3FF {
        0x16 => "pt",
        0x0A => "es",
        0x0C => "fr",
        0x07 => "de",
        0x10 => "it",
        _ => "en",
    }
}

// --- saved settings -----------------------------------------------------------
#[derive(Clone, Serialize, Deserialize)]
pub struct CaseLast {
    pub level: u8,
    pub time: f64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub name: Option<String>,
    pub mac: Option<String>,
    pub model_id: Option<String>,
    pub autostart: Option<bool>,
    pub notifications: Option<bool>,
    pub theme: Option<String>,
    pub language: Option<String>,
    pub low_latency: bool,
    pub case_last: Option<CaseLast>,
}

// --- Bluetooth presence --------------------------------------------------------
fn mac_to_u64(mac: &str) -> u64 {
    u64::from_str_radix(&mac.replace(':', ""), 16).unwrap_or(0)
}

fn device_info(addr: u64) -> BLUETOOTH_DEVICE_INFO {
    let mut info: BLUETOOTH_DEVICE_INFO = unsafe { zeroed() };
    info.dwSize = size_of::<BLUETOOTH_DEVICE_INFO>() as u32;
    info.Address.Anonymous.ullLong = addr;
    info
}

/// Is the device's Bluetooth link to this PC up? None if unknown.
fn bt_connected(mac: &str) -> Option<bool> {
    let mut info = device_info(mac_to_u64(mac));
    (unsafe { BluetoothGetDeviceInfo(None, &mut info) } == 0).then(|| info.fConnected.as_bool())
}

/// Is the PC's Bluetooth on? False also when there is no adapter; None if unknown.
fn bt_radio_on() -> Option<bool> {
    let radios = Radio::GetRadiosAsync().ok()?.get().ok()?;
    let mut any_on = false;
    for r in radios {
        if r.Kind().ok()? == RadioKind::Bluetooth && r.State().ok()? == RadioState::On {
            any_on = true;
        }
    }
    Some(any_on)
}

/// Ask Windows to drop the Bluetooth link to a device. True if the request was accepted.
fn bt_disconnect(mac: &str) -> bool {
    let params = BLUETOOTH_FIND_RADIO_PARAMS { dwSize: size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32 };
    let mut radio = HANDLE::default();
    let addr = mac_to_u64(mac);
    let mut returned = 0u32;
    unsafe {
        let Ok(find) = BluetoothFindFirstRadio(&params, &mut radio) else { return false };
        let ok = DeviceIoControl(
            radio,
            IOCTL_BTH_DISCONNECT_DEVICE,
            Some(&addr as *const u64 as *const _),
            8,
            None,
            0,
            Some(&mut returned),
            None,
        )
        .is_ok();
        let _ = CloseHandle(radio);
        let _ = BluetoothFindRadioClose(find);
        ok
    }
}

fn is_nothing_headset(name: &str) -> bool {
    let low = name.to_lowercase();
    if base_for_name(&low).is_some() {
        return true;
    }
    (low.contains("nothing") || low.contains("cmf"))
        && ["ear", "buds", "headphone", "neckband", "clip"].iter().any(|w| low.contains(w))
}

fn base_for_name(low_name: &str) -> Option<String> {
    MODEL_DATA["models"].as_object()?.iter().find_map(|(base, m)| {
        (m["name"].as_str()?.to_lowercase() == low_name).then(|| base.clone())
    })
}

/// Paired Nothing / CMF headsets, preferring one that is connected now.
fn find_paired_ear() -> Option<(String, String)> {
    let mut params: BLUETOOTH_DEVICE_SEARCH_PARAMS = unsafe { zeroed() };
    params.dwSize = size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32;
    params.fReturnRemembered = true.into();
    params.fReturnConnected = true.into();
    params.fReturnAuthenticated = true.into();
    let mut info = device_info(0);
    let mut found: Vec<(String, String, bool)> = vec![];
    unsafe {
        let Ok(h) = BluetoothFindFirstDevice(&params, &mut info) else { return None };
        loop {
            let name = String::from_utf16_lossy(&info.szName);
            let name = name.trim_end_matches('\0').to_string();
            if is_nothing_headset(&name) {
                let a = info.Address.Anonymous.rgBytes;
                let mac = format!(
                    "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                    a[5], a[4], a[3], a[2], a[1], a[0]
                );
                found.push((name, mac, info.fConnected.as_bool()));
            }
            if BluetoothFindNextDevice(h, &mut info).is_err() {
                break;
            }
        }
        let _ = BluetoothFindDeviceClose(h);
    }
    found.sort_by_key(|d| !d.2);
    found.into_iter().next().map(|d| (d.0, d.1))
}

/// Model id from a Fast Pair message stream: group 3, code 1, 3-byte value.
fn parse_model_id(data: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = (data[i + 2] as usize) << 8 | data[i + 3] as usize;
        if data[i] == 3 && data[i + 1] == 1 && len == 3 && i + 7 <= data.len() {
            return Some(data[i + 4..i + 7].iter().map(|b| format!("{b:02X}")).collect());
        }
        i += 4 + len;
    }
    None
}

// --- chimes ----------------------------------------------------------------------
static CHIME_ON: &[u8] = include_bytes!("../sounds/chime_on.wav");
static CHIME_OFF: &[u8] = include_bytes!("../sounds/chime_off.wav");

fn play_chime(on: bool) {
    spawn(move || {
        sleep(Duration::from_millis(400)); // let the buds switch audio mode first
        let wav = if on { CHIME_ON } else { CHIME_OFF };
        unsafe {
            let _ = PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_NODEFAULT);
        }
    });
}

// --- RFCOMM socket -----------------------------------------------------------------
struct Link(SOCKET);

impl Link {
    fn open(mac: &str, service: GUID) -> Option<Link> {
        unsafe {
            let s = socket(AF_BTH as i32, SOCK_STREAM, BTHPROTO_RFCOMM as i32).ok()?;
            let addr = SOCKADDR_BTH {
                addressFamily: AF_BTH,
                btAddr: mac_to_u64(mac),
                serviceClassId: service,
                port: 0,
            };
            let rc = connect(s, &addr as *const _ as *const SOCKADDR, size_of::<SOCKADDR_BTH>() as i32);
            if rc != 0 {
                let _ = closesocket(s);
                return None;
            }
            Some(Link(s))
        }
    }

    fn set_timeout(&self, ms: u32) {
        let bytes = ms.to_ne_bytes();
        unsafe {
            setsockopt(self.0, SOL_SOCKET, SO_RCVTIMEO, Some(&bytes));
        }
    }

    fn write(&self, data: &[u8]) -> bool {
        (unsafe { send(self.0, data, SEND_RECV_FLAGS(0)) }) == data.len() as i32
    }

    fn read(&self, buf: &mut [u8]) -> Option<usize> {
        let n = unsafe { recv(self.0, buf, SEND_RECV_FLAGS(0)) };
        (n > 0).then_some(n as usize)
    }

    fn close(&self) {
        unsafe {
            let _ = closesocket(self.0);
        }
    }
}

// --- the headset ---------------------------------------------------------------------
pub struct Ear {
    config_path: PathBuf,
    pub cfg: Mutex<Config>,
    pub status: Mutex<&'static str>,
    state: Mutex<BTreeMap<String, Value>>,
    fit: Mutex<Option<Value>>,
    dual: Mutex<Vec<Value>>,
    link: Mutex<Option<Arc<Link>>>,
    op: AtomicU8,
    stop: AtomicBool,
    connect_requested: AtomicBool,
    no_connect_until: Mutex<Option<Instant>>,
    on_change: Box<dyn Fn(Change) + Send + Sync>,
}

impl Ear {
    pub fn new(config_path: PathBuf, on_change: Box<dyn Fn(Change) + Send + Sync>) -> Arc<Ear> {
        let cfg = fs::read(&config_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Config>(&b).ok())
            .unwrap_or_default();
        Arc::new(Ear {
            config_path,
            cfg: Mutex::new(cfg),
            status: Mutex::new(ST_CONNECTING),
            state: Mutex::default(),
            fit: Mutex::new(None),
            dual: Mutex::default(),
            link: Mutex::new(None),
            op: AtomicU8::new(0),
            stop: AtomicBool::new(false),
            connect_requested: AtomicBool::new(false),
            no_connect_until: Mutex::new(None),
            on_change,
        })
    }

    pub fn save_config(&self) {
        if let Some(dir) = self.config_path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_vec_pretty(&*self.cfg.lock().unwrap()) {
            let _ = fs::write(&self.config_path, text);
        }
    }

    pub fn status(&self) -> &'static str {
        *self.status.lock().unwrap()
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.state.lock().unwrap().get(key).cloned()
    }

    fn set(&self, key: &str, value: Value) {
        self.state.lock().unwrap().insert(key.into(), value);
    }

    fn set_status(&self, st: &'static str) {
        let mut cur = self.status.lock().unwrap();
        if *cur != st {
            *cur = st;
            drop(cur);
            (self.on_change)(Change::Status);
        }
    }

    // --- language --------------------------------------------------------------
    /// The chosen language, or the Windows one; English if we have no translation for it.
    pub fn lang(&self) -> String {
        let choice = self.cfg.lock().unwrap().language.clone().unwrap_or_else(|| "auto".into());
        let code = if choice == "auto" { windows_language().to_string() } else { choice };
        if I18N.get(&code).is_some() { code } else { "en".into() }
    }

    pub fn t(&self, key: &str) -> String {
        I18N[self.lang().as_str()][key]
            .as_str()
            .or_else(|| I18N["en"][key].as_str())
            .unwrap_or(key)
            .to_string()
    }

    // --- which headset is this? -------------------------------------------------
    fn base(&self) -> Option<String> {
        let cfg = self.cfg.lock().unwrap();
        cfg.model_id
            .as_deref()
            .and_then(|id| MODEL_DATA["ids"][id].as_str().map(String::from))
            .or_else(|| base_for_name(&cfg.name.clone().unwrap_or_default().to_lowercase()))
    }

    /// This model's capabilities; an unknown model is treated like the one we've tested.
    fn spec(&self) -> Value {
        self.base()
            .and_then(|b| MODEL_DATA["models"].get(&b).cloned())
            .or_else(|| MODEL_DATA["models"].get(TESTED_MODEL).cloned())
            .unwrap_or(Value::Null)
    }

    pub fn display_name(&self) -> String {
        self.base()
            .and_then(|b| MODEL_DATA["models"][&b]["name"].as_str().map(String::from))
            .or_else(|| self.cfg.lock().unwrap().name.clone())
            .unwrap_or_else(|| "Nothing".into())
    }

    pub fn capabilities(&self) -> Value {
        let spec = self.spec();
        let anc = spec["anc"].as_u64().unwrap_or(0);
        let hq = spec["hq"].as_u64().unwrap_or(0);
        let levels: Vec<&str> = [(0x08, "nc_adaptive"), (0x04, "nc_low"), (0x02, "nc_mid"), (0x04, "nc_high")]
            .iter()
            .filter(|(bit, _)| anc & bit != 0)
            .map(|(_, m)| *m)
            .collect();
        let codecs: Vec<usize> = if hq == 0 {
            vec![]
        } else {
            (0..3).filter(|i| *i == 0 || hq & (1 << i) != 0).collect()
        };
        let base = self.base();
        json!({
            "anc": anc != 0, "transparency": anc & 0x10 != 0, "levels": levels,
            "fit": spec["fit"], "bass_levels": spec["bass_levels"], "in_ear": spec["in_ear"],
            "eq_presets": spec["eq_presets"], "custom_eq": !spec["custom_eq"].is_null(),
            "dual": spec["dual"], "dual_reboot": spec["dual_reboot"], "single": spec["single"],
            "codecs": codecs, "codec_names": CODEC_NAMES, "gestures": spec["gestures"], "ring": spec["ring"],
            "tested": base.is_none() || base.as_deref() == Some(TESTED_MODEL),
        })
    }

    /// New PC or new headset: follow whichever Nothing device is paired / connected.
    fn pick_headset(&self) {
        let Some((name, mac)) = find_paired_ear() else { return };
        let mut cfg = self.cfg.lock().unwrap();
        let cached_up = cfg.mac.as_deref().and_then(bt_connected) == Some(true);
        if cfg.mac.as_deref() == Some(&mac) || cached_up {
            return;
        }
        cfg.name = Some(name);
        cfg.mac = Some(mac);
        cfg.model_id = None;
        cfg.case_last = None;
        drop(cfg);
        self.state.lock().unwrap().clear();
        self.save_config();
        (self.on_change)(Change::Name);
    }

    /// Ask the headset itself: Windows only knows the Bluetooth name, which can be renamed.
    fn detect_model(&self, mac: &str) {
        let Some(link) = Link::open(mac, FAST_PAIR_SERVICE) else { return };
        link.set_timeout(2500);
        let mut buf = [0u8; 1024];
        let model = link.read(&mut buf).and_then(|n| parse_model_id(&buf[..n]));
        link.close();
        if let Some(model) = model {
            self.cfg.lock().unwrap().model_id = Some(model);
            self.save_config();
            (self.on_change)(Change::Name);
            self.refresh_all(); // now we know what this model supports
        }
    }

    // --- sending -----------------------------------------------------------------
    fn packet(&self, cmd: u16, payload: &[u8]) -> Vec<u8> {
        let op = self.op.load(Ordering::Relaxed) % 250 + 1;
        self.op.store(op, Ordering::Relaxed);
        let mut p = vec![0x55, 0x60, 0x01, cmd as u8, (cmd >> 8) as u8, payload.len() as u8, 0, op];
        p.extend_from_slice(payload);
        let c = crc16(&p);
        p.extend_from_slice(&c.to_le_bytes());
        p
    }

    fn send(&self, cmd: u16, payload: &[u8]) -> bool {
        let packet = self.packet(cmd, payload);
        let link = self.link.lock().unwrap().clone();
        match link {
            Some(l) if l.write(&packet) => true,
            Some(_) => {
                self.drop_link();
                false
            }
            None => false,
        }
    }

    fn drop_link(&self) {
        if let Some(l) = self.link.lock().unwrap().take() {
            l.close();
        }
        self.state.lock().unwrap().remove("battery");
    }

    fn try_connect(&self, mac: &str) -> Option<Arc<Link>> {
        self.set_status(ST_CONNECTING);
        let link = Link::open(mac, NOTHING_SERVICE)?;
        link.set_timeout(4000);
        let mut buf = [0u8; 1024];
        let first = if link.write(&self.packet(CMD_GET_BATTERY, &[])) { link.read(&mut buf) } else { None };
        match first {
            Some(n) if buf[..n].starts_with(&[0x55, 0x60, 0x01]) => {
                link.set_timeout(0);
                let link = Arc::new(link);
                *self.link.lock().unwrap() = Some(link.clone());
                let mut pending = vec![];
                self.handle_stream(&mut pending, &buf[..n]);
                self.set_status(ST_CONNECTED);
                Some(link)
            }
            _ => {
                link.close();
                None
            }
        }
    }

    fn refresh_all(&self) {
        let secs = (now() as u32).to_be_bytes();
        self.send(CMD_SET_TIME, &secs);
        // The buds reset low-latency mode whenever they disconnect, so re-apply the user's choice.
        if self.cfg.lock().unwrap().low_latency {
            self.send(CMD_SET_LATENCY, &[0x01, 0x00]);
            sleep(Duration::from_millis(80));
        }
        let spec = self.spec();
        let truthy = |k: &str| match &spec[k] {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_u64().unwrap_or(0) != 0,
            Value::Null => false,
            _ => true,
        };
        let mut cmds = vec![CMD_GET_BATTERY, CMD_GET_LATENCY, CMD_GET_FIRMWARE, CMD_GET_GESTURES];
        for (key, cmd) in [
            ("anc", CMD_GET_ANC),
            ("eq_presets", CMD_GET_EQ),
            ("bass_levels", CMD_GET_BASS),
            ("in_ear", CMD_GET_INEAR),
            ("dual", CMD_GET_DUAL),
            ("hq", CMD_GET_CODEC),
            ("custom_eq", CMD_GET_CUSTOM_EQ),
        ] {
            if truthy(key) {
                cmds.push(cmd);
            }
        }
        for cmd in cmds {
            self.send(cmd, &[]);
            sleep(Duration::from_millis(80));
        }
        if truthy("dual") {
            self.request_dual_list(true);
        }
    }

    fn request_dual_list(&self, restart: bool) {
        let mut dual = self.dual.lock().unwrap();
        if restart {
            dual.clear();
        }
        let offset = dual.len() as u8;
        drop(dual);
        self.send(CMD_GET_DUAL_LIST, &[offset]);
    }

    /// Paged list of devices the buds know: flags (self / connected), MAC, name.
    fn read_dual_list(&self, p: &[u8]) {
        if p.len() < 3 {
            return;
        }
        let (mut offset, mut added) = (3, false);
        let mut dual = self.dual.lock().unwrap();
        for _ in 0..p[2] {
            if offset + 8 > p.len() {
                break;
            }
            let flags = p[offset];
            let mac: Vec<String> = p[offset + 1..offset + 7].iter().map(|b| format!("{b:02X}")).collect();
            let mac = mac.join("-");
            let name_len = (p[offset + 7] & 0x7F) as usize;
            let end = (offset + 8 + name_len).min(p.len());
            let device = json!({
                "mac": mac, "self": flags & 0xF0 != 0, "connected": flags & 0x0F != 0,
                "name": String::from_utf8_lossy(&p[offset + 8..end]),
            });
            match dual.iter_mut().find(|d| d["mac"] == mac) {
                Some(known) => *known = device,
                None => {
                    dual.push(device);
                    added = true;
                }
            }
            offset += 8 + name_len;
        }
        let list = Value::Array(dual.clone());
        let more = added && dual.len() < 20;
        drop(dual);
        self.set("dual_list", list);
        if more {
            self.request_dual_list(false);
        }
    }

    // --- receiving -----------------------------------------------------------------
    fn handle_stream(&self, buf: &mut Vec<u8>, data: &[u8]) {
        buf.extend_from_slice(data);
        loop {
            let Some(i) = buf.iter().position(|b| *b == 0x55) else {
                buf.clear();
                return;
            };
            buf.drain(..i);
            if buf.len() < 8 {
                return;
            }
            // 55 60 01 = reply with a CRC16 trailer; 55 00 01 = unsolicited push, no CRC.
            let with_crc = buf[1] == 0x60;
            if !(with_crc || buf[1] == 0x00) || buf[2] != 0x01 {
                buf.remove(0);
                continue;
            }
            let plen = buf[5] as usize | (buf[6] as usize) << 8;
            let n = 8 + plen + if with_crc { 2 } else { 0 };
            if buf.len() < n {
                return;
            }
            if with_crc && crc16(&buf[..n - 2]) != u16::from_le_bytes([buf[n - 2], buf[n - 1]]) {
                buf.remove(0);
                continue;
            }
            let frame: Vec<u8> = buf.drain(..n).collect();
            self.parse(frame[3] as u16 | (frame[4] as u16) << 8, &frame[8..8 + plen]);
        }
    }

    fn parse(&self, cmd: u16, p: &[u8]) {
        match cmd {
            0x4007 | 0xE001 if !p.is_empty() => {
                let mut bat = serde_json::Map::new();
                for k in 0..p[0] as usize {
                    let (Some(dev), Some(val)) = (p.get(1 + 2 * k), p.get(2 + 2 * k)) else { break };
                    let name = match dev {
                        2 => "left",
                        3 => "right",
                        4 => "case",
                        6 => "single",
                        _ => continue,
                    };
                    bat.insert(name.into(), json!({"level": val & 0x7F, "charging": val & 0x80 != 0}));
                }
                // The case only reports through a docked bud, so remember its last reading.
                if let Some(level) = bat.get("case").and_then(|c| c["level"].as_u64()) {
                    let level = level as u8;
                    let mut cfg = self.cfg.lock().unwrap();
                    let stale = cfg.case_last.as_ref().is_none_or(|l| l.level != level || now() - l.time > 300.0);
                    if stale {
                        cfg.case_last = Some(CaseLast { level, time: now() });
                        drop(cfg);
                        self.save_config();
                    }
                }
                self.set("battery", Value::Object(bat));
                (self.on_change)(Change::Battery);
            }
            0x401E | 0xE003 if p.len() > 1 => {
                let mode = ANC_MODES.iter().find(|m| m.0 == p[1]).map_or("unknown", |m| m.1);
                self.set("anc", json!(mode));
                (self.on_change)(Change::Anc);
            }
            0x401F | 0x4050 if !p.is_empty() => self.set("eq", json!(p[0])),
            0x404E if p.len() > 1 => {
                let level = if self.spec()["bass_levels"] == 3 {
                    BASS_WIRE_3_STAGE.iter().position(|w| *w == p[1]).map_or(1, |i| i + 1) as u8
                } else {
                    p[1] / 2
                };
                self.set("bass", json!({"enabled": p[0] == 1, "level": level}));
            }
            0x400E if p.len() > 2 => self.set("inear", json!(p[2] == 1)),
            0x4041 if !p.is_empty() => self.set("latency", json!(p[0] == 1)),
            0x4042 => self.set("firmware", json!(String::from_utf8_lossy(p))),
            0x4018 if !p.is_empty() => {
                let mut g: BTreeMap<&str, BTreeMap<String, u8>> = BTreeMap::new();
                for rec in p[1..].chunks_exact(4).take(p[0] as usize) {
                    let side = match rec[0] {
                        2 => "left",
                        3 => "right",
                        6 => "single",
                        _ => continue,
                    };
                    g.entry(side).or_default().insert(format!("{}_{}", rec[1], rec[2]), rec[3]);
                }
                self.set("gestures", json!(g));
            }
            0xE00D if p.len() > 1 => {
                *self.fit.lock().unwrap() = Some(json!({"left": p[0], "right": p[1], "time": now()}));
            }
            0x4027 if !p.is_empty() => self.set("dual", json!(p[0] == 1)),
            0x4028 => self.read_dual_list(p),
            0xE00E => self.request_dual_list(true),
            0x4029 if !p.is_empty() => {
                self.set("codec", json!(if (p[0] as usize) < CODEC_NAMES.len() { p[0] } else { 0 }));
            }
            0x4044 if p.len() >= 6 + 13 * 3 => {
                // [count][total gain f32][per band: type u8, gain f32, freq f32, q f32]; peak, high, low
                let gain = |i: usize| {
                    let at = 6 + 13 * i;
                    f32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]]).round() as i32
                };
                self.set("custom_eq", json!({"bass": gain(2), "mid": gain(0), "treble": gain(1)}));
            }
            _ => {}
        }
    }

    // --- worker threads ------------------------------------------------------------
    /// Watch the Bluetooth link and keep the RFCOMM session up. Runs until the app quits.
    pub fn run(self: Arc<Self>) {
        {
            let ear = self.clone();
            spawn(move || loop {
                sleep(Duration::from_secs(30));
                ear.send(CMD_GET_BATTERY, &[]);
            });
        }
        while !self.stop.load(Ordering::Relaxed) {
            if bt_radio_on() == Some(false) {
                self.drop_link();
                self.set_status(ST_BT_OFF);
                sleep(Duration::from_secs(2));
                continue;
            }
            let has_link = self.link.lock().unwrap().is_some();
            if !has_link {
                self.pick_headset();
            }
            let Some(mac) = self.cfg.lock().unwrap().mac.clone() else {
                self.set_status(ST_UNPAIRED);
                sleep(Duration::from_secs(5));
                continue;
            };
            let linked = bt_connected(&mac);
            let requested = self.connect_requested.load(Ordering::Relaxed);
            let holding = self.no_connect_until.lock().unwrap().is_some_and(|t| Instant::now() < t);
            if has_link {
                if linked == Some(false) {
                    self.drop_link();
                    self.set_status(ST_DISCONNECTED);
                }
            } else if (linked == Some(false) && !requested) || holding {
                self.set_status(ST_DISCONNECTED);
            } else if let Some(link) = self.try_connect(&mac) {
                self.connect_requested.store(false, Ordering::Relaxed);
                let ear = self.clone();
                spawn(move || ear.reader(link));
                let ear = self.clone();
                spawn(move || ear.refresh_all());
                if self.cfg.lock().unwrap().model_id.is_none() {
                    let ear = self.clone();
                    let mac = mac.clone();
                    spawn(move || ear.detect_model(&mac));
                }
            } else {
                self.connect_requested.store(false, Ordering::Relaxed);
                self.set_status(ST_DISCONNECTED);
                sleep(Duration::from_secs(3));
            }
            sleep(Duration::from_millis(1500));
        }
    }

    fn reader(&self, link: Arc<Link>) {
        let mut pending = vec![];
        let mut buf = [0u8; 4096];
        while let Some(n) = link.read(&mut buf) {
            self.handle_stream(&mut pending, &buf[..n]);
        }
        // Only report it if this is still the live link (not one we closed on purpose).
        let is_current = self.link.lock().unwrap().as_ref().is_some_and(|l| Arc::ptr_eq(l, &link));
        if is_current {
            self.drop_link();
            self.set_status(ST_DISCONNECTED);
        }
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.drop_link();
    }

    /// Drop the PC's Bluetooth link, e.g. so the buds are free to connect to a phone.
    fn disconnect(&self) -> bool {
        let Some(mac) = self.cfg.lock().unwrap().mac.clone() else { return false };
        // Our own reconnect would bring the link right back.
        *self.no_connect_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(8));
        self.drop_link();
        self.set_status(ST_DISCONNECTED);
        bt_disconnect(&mac)
    }

    fn send_custom_eq(&self, bass: i32, mid: i32, treble: i32) {
        let c = self.spec()["custom_eq"].clone();
        if c.is_null() {
            return;
        }
        let (bass, mid, treble) = (bass.clamp(-6, 6), mid.clamp(-6, 6), treble.clamp(-6, 6));
        let f = |k: &str| c[k].as_f64().unwrap_or(0.0) as f32;
        let bands = [
            (1u8, mid, f("freqPeak"), f("qPeak")),
            (2, treble, f("freqHigh"), f("qHigh")),
            (0, bass, f("freqLow"), f("qLow")),
        ];
        let mut payload = vec![bands.len() as u8];
        payload.extend_from_slice(&(-(bass.max(mid).max(treble).max(0) as f32)).to_le_bytes());
        for (kind, gain, freq, q) in bands {
            payload.push(kind);
            payload.extend_from_slice(&(gain as f32).to_le_bytes());
            payload.extend_from_slice(&freq.to_le_bytes());
            payload.extend_from_slice(&q.to_le_bytes());
        }
        payload.extend_from_slice(&[0u8; 9]);
        self.send(CMD_SET_CUSTOM_EQ, &payload);
        self.set("custom_eq", json!({"bass": bass, "mid": mid, "treble": treble}));
    }

    // --- actions from the UI / tray --------------------------------------------------
    pub fn action(&self, a: &Value) -> Value {
        let flag = |k: &str| a[k].as_bool().unwrap_or(false);
        let num = |k: &str| a[k].as_u64().unwrap_or(0) as u8;
        let int = |k: &str| a[k].as_i64().unwrap_or(0) as i32;
        let pause = |ms| sleep(Duration::from_millis(ms));
        match a["action"].as_str().unwrap_or("") {
            "anc" => {
                let mode = a["mode"].as_str().unwrap_or("");
                if let Some(m) = ANC_MODES.iter().find(|m| m.1 == mode) {
                    self.send(CMD_SET_ANC, &[0x01, m.0, 0x00]);
                    self.set("anc", json!(mode));
                    (self.on_change)(Change::Anc);
                    pause(300);
                    self.send(CMD_GET_ANC, &[]);
                }
            }
            "eq" => {
                self.send(CMD_SET_EQ, &[num("preset"), 0x00]);
                self.set("eq", json!(num("preset")));
                pause(200);
                self.send(CMD_GET_EQ, &[]);
            }
            "bass" => {
                let three_stage = self.spec()["bass_levels"] == 3;
                let level = num("level").clamp(1, if three_stage { 3 } else { 5 });
                let wire = if three_stage { BASS_WIRE_3_STAGE[level as usize - 1] } else { level * 2 };
                self.send(CMD_SET_BASS, &[flag("enabled") as u8, wire]);
                self.set("bass", json!({"enabled": flag("enabled"), "level": level}));
                pause(200);
                self.send(CMD_GET_BASS, &[]);
            }
            "inear" => {
                self.send(CMD_SET_INEAR, &[0x01, 0x01, flag("enabled") as u8]);
                self.set("inear", json!(flag("enabled")));
                pause(200);
                self.send(CMD_GET_INEAR, &[]);
            }
            "latency" => {
                let on = flag("enabled");
                self.send(CMD_SET_LATENCY, &[if on { 1 } else { 2 }, 0x00]);
                self.set("latency", json!(on));
                self.cfg.lock().unwrap().low_latency = on;
                self.save_config();
                // The buds chime for noise-control changes but not for this, so chime from the PC.
                play_chime(on);
                pause(200);
                self.send(CMD_GET_LATENCY, &[]);
            }
            "gesture" => {
                let dev = match a["side"].as_str() {
                    Some("left") => 0x02,
                    Some("right") => 0x03,
                    _ => 0x06,
                };
                let button = a["button"].as_u64().unwrap_or(1) as u8;
                self.send(CMD_SET_GESTURE, &[0x01, dev, button, num("type"), num("value")]);
                pause(300);
                self.send(CMD_GET_GESTURES, &[]);
            }
            "ring" => {
                let on = flag("on") as u8;
                match self.spec()["ring"].as_str() {
                    Some("all") => self.send(CMD_RING, &[on]),
                    Some("single") => self.send(CMD_RING, &[0x06, on]),
                    _ => self.send(CMD_RING, &[if a["side"] == "left" { 0x02 } else { 0x03 }, on]),
                };
            }
            "fit_test" => {
                *self.fit.lock().unwrap() = None;
                self.send(CMD_FIT_TEST, &[0x01]);
            }
            "dual" => {
                let on = flag("enabled");
                self.send(CMD_SET_DUAL, &[on as u8]);
                self.set("dual", json!(on));
                pause(300);
                self.send(CMD_GET_DUAL, &[]);
                if on {
                    self.request_dual_list(true);
                }
            }
            "dual_device" => {
                let mut payload = vec![flag("connect") as u8];
                payload.extend(a["mac"].as_str().unwrap_or("").split('-').filter_map(|b| u8::from_str_radix(b, 16).ok()));
                self.send(CMD_SET_DUAL_DEVICE, &payload);
                pause(1000);
                self.request_dual_list(true);
            }
            "codec" => {
                let index = num("index");
                let allowed = self.capabilities()["codecs"].as_array().is_some_and(|c| c.iter().any(|v| *v == json!(index)));
                if allowed {
                    // The buds restart to apply this; the normal reconnect picks them up again.
                    self.send(CMD_SET_CODEC, &[index]);
                    self.set("codec", json!(index));
                }
            }
            "custom_eq" => self.send_custom_eq(int("bass"), int("mid"), int("treble")),
            "disconnect" => return json!({"ok": self.disconnect()}),
            "connect" => {
                *self.no_connect_until.lock().unwrap() = None;
                self.connect_requested.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
        json!({"ok": true})
    }

    pub fn snapshot(&self) -> Value {
        let name = self.display_name();
        let caps = self.capabilities();
        let lang = self.lang();
        let cfg = self.cfg.lock().unwrap();
        json!({
            "status": self.status(),
            "name": name,
            "state": *self.state.lock().unwrap(),
            "fit": *self.fit.lock().unwrap(),
            "case_last": cfg.case_last,
            "caps": caps,
            "lang": lang,
            "language": cfg.language.clone().unwrap_or_else(|| "auto".into()),
            "notifications": cfg.notifications.unwrap_or(true),
            "theme": cfg.theme.clone().unwrap_or_else(|| "light".into()),
        })
    }
}

/// Winsock must be initialised once before any socket call.
pub fn init_winsock() {
    unsafe {
        let mut data: WSADATA = zeroed();
        WSAStartup(0x0202, &mut data);
    }
}
