//! Jailed CLEO script runtime.
//!
//! This module deliberately does not patch executable memory. It drives CLEO scripts
//! from an NSTimer installed on the main thread and calls the game's existing SCM
//! opcode handlers directly. The addresses match the CLEO 2.6.0 GTA:SA target.

use once_cell::sync::Lazy;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

const GAME_STATE_ADDR: usize = 0x1006806d0;
const GAME_TIME_ADDR: usize = 0x1007d3af8;
const COMMAND_TABLE_ADDR: usize = 0x1005c11d8;
const EXTENDED_HANDLER_ADDR: usize = 0x10020980c;

const MAX_INSTRUCTIONS_PER_TICK: usize = 512;

extern "C" {
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
}

fn game_slide() -> usize {
    static SLIDE: Lazy<usize> = Lazy::new(|| unsafe {
        // CLEO 2.6.0 used the smaller slide of image 0 / image 1 because the
        // game image moved between those positions on newer iOS releases.
        let a = _dyld_get_image_vmaddr_slide(0).max(0) as usize;
        let b = _dyld_get_image_vmaddr_slide(1).max(0) as usize;
        a.min(b)
    });

    *SLIDE
}

fn absolute(address: usize) -> usize {
    address + game_slide()
}

unsafe fn read_global<T: Copy>(address: usize) -> T {
    (absolute(address) as *const T).read()
}

#[repr(C, align(8))]
#[derive(Debug)]
struct GameScript {
    next: usize,
    previous: usize,
    name: [u8; 8],
    base_ip: *const u16,
    ip: *const u16,
    call_stack: [usize; 8],
    stack_pos: u16,
    locals: [u32; 40],
    timers: [i32; 2],
    active: bool,
    bool_flag: bool,
    use_mission_cleanup: bool,
    is_external: bool,
    ovr_textbox: bool,
    attach_type: u8,
    wakeup_time: u32,
    condition_count: u16,
    not_flag: bool,
    checking_game_over: bool,
    game_over: bool,
    skip_scene_pos: i32,
    is_mission: bool,
}

impl GameScript {
    fn new(ip: *const u16, active: bool) -> Self {
        Self {
            next: 0,
            previous: 0,
            name: *b"unnamed!",
            base_ip: ip,
            ip,
            call_stack: [0; 8],
            stack_pos: 0,
            locals: [0; 40],
            timers: [0; 2],
            active,
            bool_flag: false,
            use_mission_cleanup: false,
            is_external: false,
            ovr_textbox: false,
            attach_type: 0,
            wakeup_time: 0,
            condition_count: 0,
            not_flag: false,
            checking_game_over: false,
            game_over: false,
            skip_scene_pos: 0,
            is_mission: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Csi,
    Csa,
}

#[derive(Debug)]
struct Script {
    kind: Kind,
    bytes: Vec<u8>,
    game: GameScript,
    name: String,
    enabled: bool,
    error: Option<String>,
}

unsafe impl Send for Script {}

impl Script {
    fn new(kind: Kind, bytes: Vec<u8>, name: String) -> Self {
        let ip = bytes.as_ptr().cast::<u16>();

        Self {
            kind,
            bytes,
            game: GameScript::new(ip, false),
            name,
            enabled: kind == Kind::Csa,
            error: None,
        }
    }

    fn reset(&mut self, active: bool) {
        let base = self.bytes.as_ptr().cast::<u16>();
        self.game = GameScript::new(base, active);
        self.error = None;
    }

    fn stop_with_error(&mut self, message: String) {
        self.game.active = false;
        self.error = Some(message);
    }

    fn update_one(&mut self) -> bool {
        let op_as_written = unsafe {
            let op = self.game.ip.read();
            self.game.ip = self.game.ip.add(1);
            op
        };

        self.game.not_flag = op_as_written & 0x8000 != 0;
        let opcode = op_as_written & 0x7fff;

        // CLEO intercepts terminate so the game does not free memory owned by Rust.
        if opcode == 0x004e {
            self.reset(false);
            return true;
        }

        // These are Android-specific / unimplemented CLEO opcodes in the original
        // iOS checker. Stop cleanly instead of letting the game execute them.
        if matches!(opcode, 0x0dd0..=0x0ddb | 0x0dde | 0x0de1..=0x0df6) {
            self.stop_with_error(format!("Unsupported iOS opcode {opcode:#06x}"));
            return true;
        }

        type Handler = fn(*mut GameScript, u16) -> u8;

        let handler_addr = if opcode >= 0x0a8c {
            absolute(EXTENDED_HANDLER_ADDR)
        } else {
            let handler_index = (opcode / 100) as usize;
            let handler_offset = handler_index * 2;
            let table = absolute(COMMAND_TABLE_ADDR) as *const usize;
            unsafe { table.add(handler_offset).read() }
        };

        if handler_addr == 0 {
            self.stop_with_error(format!("No handler for opcode {opcode:#06x}"));
            return true;
        }

        let handler: Handler = unsafe { std::mem::transmute(handler_addr) };
        handler(&mut self.game, opcode) != 0
    }

    fn update(&mut self) {
        if !self.game.active {
            return;
        }

        let game_time = unsafe { read_global::<u32>(GAME_TIME_ADDR) };
        if self.game.wakeup_time > game_time {
            return;
        }

        for _ in 0..MAX_INSTRUCTIONS_PER_TICK {
            if self.update_one() {
                return;
            }

            if !self.game.active {
                return;
            }
        }

        // Yield after a bounded number of opcodes. This replaces the original
        // anti-lag loop breaker and prevents one bad script from freezing the app.
    }
}

static SCRIPTS: Lazy<Mutex<Vec<Script>>> = Lazy::new(|| Mutex::new(Vec::new()));
static IN_GAME: AtomicBool = AtomicBool::new(false);
static INITIALISED: AtomicBool = AtomicBool::new(false);

fn cleo_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.set_file_name("Documents");
    path.push("CLEO");
    path
}

fn collect_scripts(dir: &Path, out: &mut Vec<(Kind, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            collect_scripts(&path, out);
            continue;
        }

        let Some(ext) = path.extension().and_then(|x| x.to_str()) else {
            continue;
        };

        if ext.eq_ignore_ascii_case("csi") {
            out.push((Kind::Csi, path));
        } else if ext.eq_ignore_ascii_case("csa") {
            out.push((Kind::Csa, path));
        }
    }
}

pub fn reload_scripts() {
    let root = cleo_dir();
    let _ = fs::create_dir_all(&root);

    let mut paths = Vec::new();
    collect_scripts(&root, &mut paths);
    paths.sort_by_key(|(_, p)| p.display().to_string().to_lowercase());

    let mut scripts = Vec::new();

    for (kind, path) in paths {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };

        if bytes.len() < 2 {
            continue;
        }

        let name = path
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("Unnamed")
            .to_string();

        scripts.push(Script::new(kind, bytes, name));
    }

    *SCRIPTS.lock().unwrap() = scripts;
}

fn begin_game_session() {
    let mut scripts = SCRIPTS.lock().unwrap();

    for script in scripts.iter_mut() {
        match script.kind {
            Kind::Csi => script.reset(false),
            Kind::Csa => script.reset(script.enabled),
        }
    }
}

fn end_game_session() {
    let mut scripts = SCRIPTS.lock().unwrap();
    for script in scripts.iter_mut() {
        script.game.active = false;
    }
}

pub fn tick() {
    if !INITIALISED.load(Ordering::SeqCst) {
        return;
    }

    let game_state = unsafe { read_global::<u32>(GAME_STATE_ADDR) };
    let now_in_game = game_state == 9;
    let was_in_game = IN_GAME.swap(now_in_game, Ordering::SeqCst);

    if now_in_game && !was_in_game {
        begin_game_session();
    } else if !now_in_game && was_in_game {
        end_game_session();
    }

    if !now_in_game {
        return;
    }

    let mut scripts = SCRIPTS.lock().unwrap();

    for script in scripts.iter_mut() {
        if script.kind == Kind::Csa && !script.enabled {
            continue;
        }

        script.update();
    }
}

#[derive(Debug, Clone)]
pub struct ScriptStatus {
    pub name: String,
    pub active: bool,
    pub enabled: bool,
    pub error: Option<String>,
}

fn statuses(kind: Kind) -> Vec<ScriptStatus> {
    SCRIPTS
        .lock()
        .unwrap()
        .iter()
        .filter(|script| script.kind == kind)
        .map(|script| ScriptStatus {
            name: script.name.clone(),
            active: script.game.active,
            enabled: script.enabled,
            error: script.error.clone(),
        })
        .collect()
}

pub fn csi_statuses() -> Vec<ScriptStatus> {
    statuses(Kind::Csi)
}

pub fn csa_statuses() -> Vec<ScriptStatus> {
    statuses(Kind::Csa)
}

pub fn activate_csi(index: usize) -> bool {
    if !IN_GAME.load(Ordering::SeqCst) {
        return false;
    }

    let mut scripts = SCRIPTS.lock().unwrap();
    let Some(script) = scripts.iter_mut().filter(|s| s.kind == Kind::Csi).nth(index) else {
        return false;
    };

    script.reset(true);
    true
}

pub fn toggle_csa(index: usize) -> bool {
    let in_game = IN_GAME.load(Ordering::SeqCst);
    let mut scripts = SCRIPTS.lock().unwrap();
    let Some(script) = scripts.iter_mut().filter(|s| s.kind == Kind::Csa).nth(index) else {
        return false;
    };

    script.enabled = !script.enabled;

    if script.enabled && in_game {
        script.reset(true);
    } else if !script.enabled {
        script.game.active = false;
    }

    true
}

pub fn is_in_game() -> bool {
    IN_GAME.load(Ordering::SeqCst)
}

pub fn init() {
    reload_scripts();
    INITIALISED.store(true, Ordering::SeqCst);
}
