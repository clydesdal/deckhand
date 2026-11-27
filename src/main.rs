//! DeckHand – drives an Elgato Stream Deck without the Elgato software.
//!
//! Lives in the system tray. Reads `config.toml`, draws an icon on each key,
//! and fires keyboard shortcuts / commands when keys are pressed. Edit
//! config.toml while it is running and the deck reloads automatically.

// Release builds are a GUI app (no console window). Debug builds keep the console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::HashMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
};

use ab_glyph::{FontVec, PxScale};
use anyhow::{Context, Result, anyhow, bail};
use elgato_streamdeck::{DeviceStateUpdate, StreamDeck, list_devices, new_hidapi};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use image::{DynamicImage, Rgba, RgbaImage, imageops::FilterType};
use imageproc::drawing::{draw_text_mut, text_size};
use serde::Deserialize;
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

const APP_NAME: &str = "DeckHand";
const KEY_PX: u32 = 72;
const DEFAULT_CONFIG: &str = include_str!("../config.toml");
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

static QUIT: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);
/// true = release the deck so another program can use it
static PAUSED: AtomicBool = AtomicBool::new(false);
/// manual-reset event, signalled while the deck is released (handle as usize)
static PAUSED_ACK: OnceLock<usize> = OnceLock::new();

// Names other programs use to control a running instance (see CONTROL.md).
const MUTEX_NAME: &str = r"Local\DeckHand.SingleInstance";
const EV_QUIT: &str = r"Local\DeckHand.Quit";
const EV_PAUSE: &str = r"Local\DeckHand.Pause";
const EV_RESUME: &str = r"Local\DeckHand.Resume";
const EV_PAUSED: &str = r"Local\DeckHand.Paused";
static BRIGHTNESS: AtomicU8 = AtomicU8::new(60);
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn log(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    eprintln!("{msg}");
    if let Some(p) = LOG_PATH.get() {
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(p) {
            let t = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(f, "[{t}] {msg}");
        }
    }
}

// ---------------------------------------------------------------- config ----

#[derive(Deserialize)]
struct Config {
    #[serde(default = "default_brightness")]
    brightness: u8,
    #[serde(default, rename = "button")]
    buttons: Vec<Button>,
}

fn default_brightness() -> u8 {
    60
}

#[derive(Deserialize, Clone)]
struct Button {
    /// 0..=14, left-to-right, top-to-bottom
    key: u8,
    #[serde(default)]
    label: String,
    /// background colour, "#rrggbb"
    #[serde(default = "default_color")]
    color: String,
    /// path to a PNG/JPG icon (relative to config.toml)
    icon: Option<String>,
    /// hex codepoint from the "Segoe Fluent Icons" font, e.g. "E8C6"
    glyph: Option<String>,
    action: Action,
}

fn default_color() -> String {
    "#2c3e50".into()
}

#[derive(Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Action {
    /// e.g. keys = "ctrl+shift+v" or "win+shift+s"
    Hotkey { keys: String },
    /// types out text
    Text { text: String },
    /// launches a program / file / URL via `start`
    Run { command: String },
}

fn load_config(path: &Path) -> Result<HashMap<u8, Button>> {
    let txt = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&txt).context("parsing config.toml")?;
    BRIGHTNESS.store(cfg.brightness, Ordering::Relaxed);
    Ok(cfg.buttons.into_iter().map(|b| (b.key, b)).collect())
}

/// Finds config.toml: command-line arg, then next to the exe (or a parent of
/// it, so `target\release` finds the project's config), then the current dir.
/// If none exists, writes the default next to the exe.
fn find_config() -> Result<PathBuf> {
    if let Some(arg) = std::env::args().skip(1).find(|a| !a.starts_with("--")) {
        return Ok(PathBuf::from(arg));
    }
    let exe_dir = std::env::current_exe()?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    for dir in exe_dir.ancestors().take(3) {
        let p = dir.join("config.toml");
        if p.exists() {
            return Ok(p);
        }
    }
    let cwd = std::env::current_dir()?.join("config.toml");
    if cwd.exists() {
        return Ok(cwd);
    }
    let p = exe_dir.join("config.toml");
    fs::write(&p, DEFAULT_CONFIG)?;
    let _ = fs::create_dir_all(exe_dir.join("icons"));
    Ok(p)
}

// ------------------------------------------------------------- rendering ----

struct Fonts {
    text: FontVec,
    icons: Option<FontVec>,
}

impl Fonts {
    fn load() -> Result<Self> {
        let win = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let fonts = Path::new(&win).join("Fonts");
        let text = load_font(&[fonts.join("segoeuib.ttf"), fonts.join("arialbd.ttf")])
            .context("no text font found")?;
        let icons = load_font(&[fonts.join("SegoeIcons.ttf"), fonts.join("segmdl2.ttf")]);
        if icons.is_none() {
            log("warning: Segoe Fluent Icons font not found – glyph icons disabled");
        }
        Ok(Self { text, icons })
    }
}

fn load_font(candidates: &[PathBuf]) -> Option<FontVec> {
    candidates
        .iter()
        .find_map(|p| fs::read(p).ok().and_then(|b| FontVec::try_from_vec(b).ok()))
}

fn parse_color(s: &str) -> Rgba<u8> {
    let h = s.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0x2c3e50);
    Rgba([(v >> 16) as u8, (v >> 8) as u8, v as u8, 255])
}

fn render_button(b: &Button, base_dir: &Path, fonts: &Fonts, pressed: bool) -> DynamicImage {
    let mut bg = parse_color(&b.color);
    if pressed {
        for c in &mut bg.0[..3] {
            *c = c.saturating_add(70);
        }
    }
    let mut img = RgbaImage::from_pixel(KEY_PX, KEY_PX, bg);
    let has_label = !b.label.is_empty();
    let icon_area = if has_label { 48 } else { 64 };
    let white = Rgba([255, 255, 255, 255]);

    let mut drew_icon = false;

    // 1. user-supplied image icon
    if let Some(icon) = &b.icon {
        match image::open(base_dir.join(icon)) {
            Ok(i) => {
                let i = i.resize(icon_area, icon_area, FilterType::Lanczos3).to_rgba8();
                let x = (KEY_PX - i.width()) / 2;
                let y = if has_label { 4 } else { (KEY_PX - i.height()) / 2 };
                image::imageops::overlay(&mut img, &i, x as i64, y as i64);
                drew_icon = true;
            }
            Err(e) => log(format!("key {}: can't load icon {icon}: {e}", b.key)),
        }
    }

    // 2. glyph from the Windows icon font
    if !drew_icon {
        if let (Some(g), Some(font)) = (&b.glyph, &fonts.icons) {
            if let Some(ch) = u32::from_str_radix(g, 16).ok().and_then(char::from_u32) {
                let s = ch.to_string();
                let scale = PxScale::from(if has_label { 36.0 } else { 46.0 });
                let (w, h) = text_size(scale, font, &s);
                let x = (KEY_PX as i32 - w as i32) / 2;
                let y = if has_label { 8 } else { (KEY_PX as i32 - h as i32) / 2 };
                draw_text_mut(&mut img, white, x, y, scale, font, &s);
                drew_icon = true;
            }
        }
    }

    // 3. label (bottom if there is an icon, centred otherwise)
    if has_label {
        let mut size = if drew_icon { 15.0 } else { 20.0 };
        let mut scale = PxScale::from(size);
        while text_size(scale, &fonts.text, &b.label).0 > KEY_PX - 4 && size > 8.0 {
            size -= 1.0;
            scale = PxScale::from(size);
        }
        let (w, h) = text_size(scale, &fonts.text, &b.label);
        let x = (KEY_PX as i32 - w as i32) / 2;
        let y = if drew_icon {
            KEY_PX as i32 - h as i32 - 6
        } else {
            (KEY_PX as i32 - h as i32) / 2
        };
        draw_text_mut(&mut img, white, x, y, scale, &fonts.text, &b.label);
    }

    DynamicImage::ImageRgba8(img)
}

// --------------------------------------------------------------- actions ----

fn parse_key(name: &str) -> Result<Key> {
    let n = name.trim().to_ascii_lowercase();
    Ok(match n.as_str() {
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" => Key::Alt,
        "win" | "meta" | "super" | "cmd" => Key::Meta,
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "esc" | "escape" => Key::Escape,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "printscreen" | "prtsc" | "print" => Key::Other(0x2C), // VK_SNAPSHOT
        f if f.starts_with('f') && f.len() > 1 && f[1..].parse::<u32>().is_ok() => {
            let num: u32 = f[1..].parse()?;
            if !(1..=24).contains(&num) {
                bail!("bad function key {name}");
            }
            Key::Other(0x70 + num - 1) // VK_F1..
        }
        c if c.len() == 1 => {
            let ch = c.chars().next().unwrap().to_ascii_uppercase();
            if ch.is_ascii_alphanumeric() {
                // Use the virtual-key code so apps see a real shortcut (Ctrl+C etc.)
                Key::Other(ch as u32)
            } else {
                Key::Unicode(ch)
            }
        }
        _ => bail!("unknown key '{name}'"),
    })
}

fn run_action(enigo: &mut Enigo, action: &Action) -> Result<()> {
    match action {
        Action::Hotkey { keys } => {
            let parsed: Vec<Key> = keys.split('+').map(parse_key).collect::<Result<_>>()?;
            for k in &parsed {
                enigo.key(*k, Direction::Press).map_err(|e| anyhow!("{e:?}"))?;
            }
            for k in parsed.iter().rev() {
                enigo.key(*k, Direction::Release).map_err(|e| anyhow!("{e:?}"))?;
            }
        }
        Action::Text { text } => enigo.text(text).map_err(|e| anyhow!("{e:?}"))?,
        Action::Run { command } => spawn_start(command)?,
    }
    Ok(())
}

/// `cmd /C start "" <target>` – opens programs, folders, files and URLs.
fn spawn_start(target: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("cmd")
        .args(["/C", "start", "", target])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

// ----------------------------------------------------------- deck worker ----

fn draw_all(deck: &StreamDeck, buttons: &HashMap<u8, Button>, base: &Path, fonts: &Fonts) -> Result<()> {
    deck.set_brightness(BRIGHTNESS.load(Ordering::Relaxed))?;
    deck.clear_all_button_images()?;
    for b in buttons.values() {
        if b.key < deck.kind().key_count() {
            deck.set_button_image(b.key, render_button(b, base, fonts, false))?;
        }
    }
    deck.flush()?;
    Ok(())
}

fn mtime(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Runs forever (until QUIT): connects, serves key presses, reconnects if the
/// deck is unplugged.
fn deck_worker(config_path: PathBuf, status: Arc<Mutex<String>>) {
    let set_status = |s: String| {
        log(&s);
        *status.lock().unwrap() = s;
    };
    let base_dir = config_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));

    let fonts = match Fonts::load() {
        Ok(f) => f,
        Err(e) => {
            set_status(format!("Error: {e:#}"));
            return;
        }
    };
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => {
            set_status(format!("Error: keyboard init failed: {e:?}"));
            return;
        }
    };

    let mut buttons = match load_config(&config_path) {
        Ok(b) => b,
        Err(e) => {
            set_status(format!("Config error: {e:#}"));
            HashMap::new()
        }
    };
    let mut last_mtime = mtime(&config_path);
    let mut waiting_logged = false;

    while !QUIT.load(Ordering::Relaxed) {
        // ---- paused: deck is released for another program ----
        if PAUSED.load(Ordering::Relaxed) {
            set_status("Paused – deck released for other programs".into());
            set_paused_ack(true);
            while PAUSED.load(Ordering::Relaxed) && !QUIT.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(100));
            }
            set_paused_ack(false);
            waiting_logged = false;
            continue;
        }

        // ---- connect ----
        let deck = match (|| -> Result<Arc<StreamDeck>> {
            let hid = new_hidapi()?;
            let (kind, serial) = list_devices(&hid)
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no Stream Deck found"))?;
            let deck = StreamDeck::connect(&hid, kind, &serial)?;
            Ok(Arc::new(deck))
        })() {
            Ok(d) => d,
            Err(_) => {
                if !waiting_logged {
                    set_status("Waiting for Stream Deck… (is the Elgato app running?)".into());
                    waiting_logged = true;
                }
                for _ in 0..20 {
                    if QUIT.load(Ordering::Relaxed) {
                        return;
                    }
                    if PAUSED.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                continue;
            }
        };
        waiting_logged = false;
        set_status(format!("Connected: {:?} ({} keys)", deck.kind(), deck.kind().key_count()));

        // ---- serve ----
        let result = (|| -> Result<()> {
            draw_all(&deck, &buttons, &base_dir, &fonts)?;
            let reader = deck.get_reader();
            while !QUIT.load(Ordering::Relaxed) && !PAUSED.load(Ordering::Relaxed) {
                for update in reader.read(Some(Duration::from_millis(200)))? {
                    match update {
                        DeviceStateUpdate::ButtonDown(k) => {
                            if let Some(b) = buttons.get(&k) {
                                deck.set_button_image(k, render_button(b, &base_dir, &fonts, true))?;
                                deck.flush()?;
                                log(format!("key {k}: {}", b.label));
                                if let Err(e) = run_action(&mut enigo, &b.action) {
                                    log(format!("key {k}: action failed: {e}"));
                                }
                            }
                        }
                        DeviceStateUpdate::ButtonUp(k) => {
                            if let Some(b) = buttons.get(&k) {
                                deck.set_button_image(k, render_button(b, &base_dir, &fonts, false))?;
                                deck.flush()?;
                            }
                        }
                        _ => {}
                    }
                }

                // hot-reload config (file changed or "Reload" clicked in tray)
                let m = mtime(&config_path);
                if m != last_mtime || RELOAD.swap(false, Ordering::Relaxed) {
                    last_mtime = m;
                    match load_config(&config_path) {
                        Ok(b) => {
                            buttons = b;
                            draw_all(&deck, &buttons, &base_dir, &fonts)?;
                            log("config reloaded");
                        }
                        Err(e) => log(format!("config error (keeping old layout): {e:#}")),
                    }
                }
            }
            // quitting / pausing: blank the keys so the deck doesn't show stale
            // buttons. The deck handle is closed when `deck` drops at the end
            // of this loop iteration, before the pause is acknowledged.
            deck.clear_all_button_images()?;
            deck.flush()?;
            Ok(())
        })();

        if let Err(e) = result {
            set_status(format!("Disconnected ({e}) – retrying…"));
            thread::sleep(Duration::from_secs(1));
        }
    }
}

// ------------------------------------------------------------------ tray ----

fn error_box(msg: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, title) = (w(msg), w(APP_NAME));
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR) };
}

/// Returns false if another copy is already running (it would fight over the deck).
fn single_instance() -> bool {
    use windows_sys::Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
        System::Threading::CreateMutexW,
    };
    let name = wide(MUTEX_NAME);
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        // handle intentionally leaked: held for the lifetime of the process
        !h.is_null() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

// ------------------------------------------------ external control -------
// A running instance owns these named events. Another program (or
// `DeckHand.exe --quit` etc.) opens one and signals it.

const SYNCHRONIZE: u32 = 0x0010_0000;
const EVENT_MODIFY_STATE: u32 = 0x0002;

/// Creates a named event owned by this (primary) instance. Handle leaked on purpose.
fn create_event(name: &str, manual_reset: bool) -> usize {
    use windows_sys::Win32::System::Threading::CreateEventW;
    let n = wide(name);
    unsafe { CreateEventW(std::ptr::null(), manual_reset as i32, 0, n.as_ptr()) as usize }
}

/// Non-blocking check; consumes the signal for auto-reset events.
fn event_fired(h: usize) -> bool {
    use windows_sys::Win32::{Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject};
    h != 0 && unsafe { WaitForSingleObject(h as _, 0) } == WAIT_OBJECT_0
}

fn set_paused_ack(on: bool) {
    use windows_sys::Win32::System::Threading::{ResetEvent, SetEvent};
    if let Some(&h) = PAUSED_ACK.get() {
        unsafe {
            if on {
                SetEvent(h as _);
            } else {
                ResetEvent(h as _);
            }
        }
    }
}

/// Client side: `--quit`, `--pause`, `--resume`, `--status`.
/// Exit codes: 0 = done / running, 1 = not running, 2 = timed out, 3 = paused (status), 64 = bad arg.
fn control_command(cmd: &str) -> i32 {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenEventW, OpenMutexW, SetEvent, WaitForSingleObject},
    };
    let open_event = |name: &str, access: u32| unsafe { OpenEventW(access, 0, wide(name).as_ptr()) };
    let running = || unsafe {
        let m = OpenMutexW(SYNCHRONIZE, 0, wide(MUTEX_NAME).as_ptr());
        if m.is_null() {
            false
        } else {
            CloseHandle(m);
            true
        }
    };
    let signal = |name: &str| -> bool {
        let h = open_event(name, EVENT_MODIFY_STATE);
        if h.is_null() {
            return false;
        }
        unsafe {
            SetEvent(h);
            CloseHandle(h);
        }
        true
    };
    // waits up to 5 s for the instance to confirm the deck is released
    let wait_paused = |ms: u32| -> bool {
        let h = open_event(EV_PAUSED, SYNCHRONIZE);
        if h.is_null() {
            return false;
        }
        let r = unsafe { WaitForSingleObject(h, ms) };
        unsafe { CloseHandle(h) };
        r == WAIT_OBJECT_0
    };

    match cmd {
        "--status" => match (running(), wait_paused(0)) {
            (false, _) => 1,
            (true, true) => 3,
            (true, false) => 0,
        },
        "--quit" => {
            if !signal(EV_QUIT) {
                return 1;
            }
            // wait for the process to exit (its mutex disappears with it)
            for _ in 0..100 {
                if !running() {
                    return 0;
                }
                thread::sleep(Duration::from_millis(50));
            }
            2
        }
        "--pause" => {
            if !signal(EV_PAUSE) {
                return 1;
            }
            if wait_paused(5000) { 0 } else { 2 }
        }
        "--resume" => {
            if signal(EV_RESUME) { 0 } else { 1 }
        }
        _ => 64,
    }
}

// ------------------------------------------------- start with Windows ----
// Uses HKCU\Software\Microsoft\Windows\CurrentVersion\Run (per-user, no admin).

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "DeckHand";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn autostart_enabled() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    let (key, val) = (wide(RUN_KEY), wide(RUN_VALUE));
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) == 0
    }
}

fn set_autostart(enable: bool) -> Result<()> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    let (key, val) = (wide(RUN_KEY), wide(RUN_VALUE));
    let rc = unsafe {
        if enable {
            let exe = std::env::current_exe()?;
            let data = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                val.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr())
        }
    };
    if rc != 0 && !(rc == 2 && !enable) {
        // 2 = ERROR_FILE_NOT_FOUND, fine when removing something already gone
        bail!("registry error {rc}");
    }
    log(format!("start with Windows: {}", if enable { "on" } else { "off" }));
    Ok(())
}

fn tray_icon_image() -> Result<Icon> {
    let img = image::load_from_memory(ICON_PNG)?
        .resize_exact(32, 32, FilterType::Lanczos3)
        .to_rgba8();
    let (w, h) = img.dimensions();
    Ok(Icon::from_rgba(img.into_raw(), w, h)?)
}

fn run() -> Result<()> {
    if !single_instance() {
        error_box("DeckHand is already running – look for it in the system tray.");
        return Ok(());
    }

    let config_path = find_config()?;
    let base_dir = config_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let _ = LOG_PATH.set(base_dir.join("deckhand.log"));
    log(format!("starting, config: {}", config_path.display()));

    // external control events (see CONTROL.md)
    let ev_quit = create_event(EV_QUIT, false);
    let ev_pause = create_event(EV_PAUSE, false);
    let ev_resume = create_event(EV_RESUME, false);
    let _ = PAUSED_ACK.set(create_event(EV_PAUSED, true));

    // ---- deck thread ----
    let status = Arc::new(Mutex::new(String::from("Starting…")));
    let worker = {
        let (p, s) = (config_path.clone(), status.clone());
        thread::spawn(move || deck_worker(p, s))
    };

    // ---- tray menu ----
    let title = MenuItem::new(APP_NAME, false, None);
    let status_item = MenuItem::new("Starting…", false, None);
    let edit = MenuItem::new("Edit config", true, None);
    let icons = MenuItem::new("Open icons folder", true, None);
    let reload = MenuItem::new("Reload config", true, None);
    let open_log = MenuItem::new("View log", true, None);
    let pause = CheckMenuItem::new("Pause (release deck)", true, false, None);
    let autostart = CheckMenuItem::new("Start with Windows", true, autostart_enabled(), None);
    let quit = MenuItem::new("Quit", true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &title,
        &status_item,
        &PredefinedMenuItem::separator(),
        &edit,
        &icons,
        &reload,
        &open_log,
        &PredefinedMenuItem::separator(),
        &pause,
        &autostart,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(APP_NAME)
        .with_icon(tray_icon_image()?)
        .build()?;

    // ---- Win32 message loop (the tray needs it on this thread) ----
    use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};
    let mut last_status = String::new();
    loop {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            let id = ev.id();
            if id == quit.id() {
                QUIT.store(true, Ordering::Relaxed);
            } else if id == edit.id() {
                let _ = Command::new("notepad").arg(&config_path).spawn();
            } else if id == icons.id() {
                let dir = base_dir.join("icons");
                let _ = fs::create_dir_all(&dir);
                let _ = Command::new("explorer").arg(&dir).spawn();
            } else if id == pause.id() {
                PAUSED.store(pause.is_checked(), Ordering::Relaxed);
            } else if id == autostart.id() {
                // the menu has already toggled the tick; make the registry match it
                let want = autostart.is_checked();
                if let Err(e) = set_autostart(want) {
                    log(format!("start with Windows failed: {e:#}"));
                    error_box(&format!("Couldn't change start-up setting: {e:#}"));
                    autostart.set_checked(!want);
                }
            } else if id == reload.id() {
                RELOAD.store(true, Ordering::Relaxed);
            } else if id == open_log.id() {
                if let Some(p) = LOG_PATH.get() {
                    let _ = Command::new("notepad").arg(p).spawn();
                }
            }
        }

        // signals from other programs
        if event_fired(ev_quit) {
            log("quit requested by another program");
            QUIT.store(true, Ordering::Relaxed);
        }
        if event_fired(ev_pause) {
            log("pause requested by another program");
            PAUSED.store(true, Ordering::Relaxed);
        }
        if event_fired(ev_resume) {
            log("resume requested by another program");
            PAUSED.store(false, Ordering::Relaxed);
        }
        let paused = PAUSED.load(Ordering::Relaxed);
        if pause.is_checked() != paused {
            pause.set_checked(paused);
        }

        let s = status.lock().unwrap().clone();
        if s != last_status {
            status_item.set_text(&s);
            let _ = tray.set_tooltip(Some(format!("{APP_NAME}\n{s}")));
            last_status = s;
        }

        if QUIT.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(30));
    }

    let _ = worker.join();
    drop(tray);
    log("quit");
    Ok(())
}

fn main() {
    if let Some(cmd) = std::env::args().skip(1).find(|a| a.starts_with("--")) {
        std::process::exit(control_command(&cmd));
    }
    if let Err(e) = run() {
        log(format!("fatal: {e:#}"));
        error_box(&format!("{e:#}"));
    }
}
