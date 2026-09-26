//! Jailed CLEO script runtime.
//!
//! This module deliberately does not patch executable memory. It drives CLEO scripts
//! from an NSTimer installed on the main thread and calls the game's existing SCM
//! opcode handlers directly. The addresses match the CLEO 2.6.0 GTA:SA target.

use once_cell::sync::Lazy;
use std::{
    collections::HashMap,
    ffi::{c_char, c_void, CStr, CString},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Mutex,
    },
};

const GAME_STATE_ADDR: usize = 0x1006806d0;
const GAME_TIME_ADDR: usize = 0x1007d3af8;
const COMMAND_TABLE_ADDR: usize = 0x1005c11d8;
const EXTENDED_HANDLER_ADDR: usize = 0x10020980c;
const COLLECT_PARAMETERS_ADDR: usize = 0x1001cf474;
const GET_POINTER_TO_VARIABLE_ADDR: usize = 0x1001cfb04;
const SCRIPT_PARAMS_ADDR: usize = 0x1007ad690;

const MAX_INSTRUCTIONS_PER_TICK: usize = 512;
const SCRIPT_VIRTUAL_STRIDE: u32 = 0x0010_0000;
const SYMBOL_VIRTUAL_STRIDE: u32 = 0x0010_0000;
const SYMBOL_VIRTUAL_SPAN: u32 = 0x0010_0000;

extern "C" {
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
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

#[derive(Debug, Clone)]
struct AddressRegion {
    virtual_base: u32,
    real_base: usize,
    span: u32,
    writable: bool,
    name: String,
}

static NEXT_SCRIPT_VBASE: AtomicU32 = AtomicU32::new(0xE000_0000);
static NEXT_SYMBOL_VBASE: AtomicU32 = AtomicU32::new(0xD000_0000);
static SYMBOL_REGIONS: Lazy<Mutex<Vec<AddressRegion>>> = Lazy::new(|| Mutex::new(Vec::new()));
static MUTEX_VARS: Lazy<Mutex<HashMap<u32, u32>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn register_symbol_region(name: &str, real_base: usize, writable: bool) -> u32 {
    let mut regions = SYMBOL_REGIONS.lock().unwrap();

    if let Some(existing) = regions.iter().find(|region| region.name == name) {
        return existing.virtual_base;
    }

    let virtual_base = NEXT_SYMBOL_VBASE.fetch_add(SYMBOL_VIRTUAL_STRIDE, Ordering::SeqCst);
    regions.push(AddressRegion {
        virtual_base,
        real_base,
        span: SYMBOL_VIRTUAL_SPAN,
        writable,
        name: name.to_string(),
    });
    virtual_base
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
    virtual_base: u32,
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
            virtual_base: NEXT_SCRIPT_VBASE.fetch_add(SCRIPT_VIRTUAL_STRIDE, Ordering::SeqCst),
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

    fn collect_value_args(&mut self, count: u32) {
        type Collect = fn(*mut GameScript, u32);
        let collect: Collect = unsafe { std::mem::transmute(absolute(COLLECT_PARAMETERS_ADDR)) };
        collect(&mut self.game, count);
    }

    fn read_variable_arg<T: Copy>(&mut self) -> T {
        let get_ptr: fn(*mut GameScript) -> T =
            unsafe { std::mem::transmute(absolute(GET_POINTER_TO_VARIABLE_ADDR)) };
        get_ptr(&mut self.game)
    }

    fn script_params() -> *const u32 {
        absolute(SCRIPT_PARAMS_ADDR) as *const u32
    }

    fn resolve_virtual_address(&self, address: u32, size: usize) -> Option<(*mut u8, bool)> {
        let script_len = self.bytes.len().min(SCRIPT_VIRTUAL_STRIDE as usize) as u32;
        let script_end = self.virtual_base.saturating_add(script_len);

        if address >= self.virtual_base
            && address.saturating_add(size as u32) <= script_end
        {
            let offset = (address - self.virtual_base) as usize;
            let real = unsafe { self.bytes.as_ptr().add(offset) as *mut u8 };
            return Some((real, true));
        }

        let regions = SYMBOL_REGIONS.lock().unwrap();
        for region in regions.iter() {
            let end = region.virtual_base.saturating_add(region.span);
            if address >= region.virtual_base
                && address.saturating_add(size as u32) <= end
            {
                let offset = (address - region.virtual_base) as usize;
                let real = (region.real_base + offset) as *mut u8;
                return Some((real, region.writable));
            }
        }

        None
    }

    fn read_virtual_c_string(&self, address: u32) -> Option<String> {
        // Symbol names used by CLEO Android are short. Cap the scan so a corrupt
        // script can never walk arbitrary memory indefinitely.
        let mut out = Vec::new();

        for offset in 0..256u32 {
            let (ptr, _) = self.resolve_virtual_address(address.wrapping_add(offset), 1)?;
            let byte = unsafe { ptr.read() };
            if byte == 0 {
                return String::from_utf8(out).ok();
            }
            out.push(byte);
        }

        None
    }

    fn android_symbol(&self, name: &str) -> Option<(usize, bool)> {
        // First try normal dynamic lookup. A few C/runtime symbols are exported on
        // iOS even though most GTA C++ symbols are stripped.
        let c_name = CString::new(name).ok()?;
        let ptr = unsafe {
            // Darwin RTLD_DEFAULT is ((void *)-2).
            dlsym((-2isize) as *mut c_void, c_name.as_ptr())
        };

        if !ptr.is_null() {
            // Treat arbitrary dynamically found symbols as read-only until we have
            // verified that a script is addressing writable game data.
            return Some((ptr as usize, false));
        }

        // GTA-specific stripped-symbol translations are added here as we verify
        // their iOS 2.02.11 addresses/patterns.
        None
    }

    fn update_android_opcode(&mut self, opcode: u16) -> Option<bool> {
        match opcode {
            // get_label_addr
            0x0dd0 => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(1);

                let raw = unsafe { Self::script_params().read() };
                let signed = raw as i32;
                let offset = if signed >= 0 {
                    signed as u32
                } else {
                    signed.unsigned_abs()
                };

                if offset as usize >= self.bytes.len() {
                    self.stop_with_error(format!(
                        "Android label offset {offset:#x} is outside script"
                    ));
                    return Some(true);
                }

                unsafe {
                    destination.write(self.virtual_base.wrapping_add(offset));
                }
                Some(false)
            }

            // get_func_addr_by_cstr_name
            0x0dd1 => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(1);
                let string_address = unsafe { Self::script_params().read() };

                let Some(name) = self.read_virtual_c_string(string_address) else {
                    self.stop_with_error("Android 0DD1 received an invalid symbol string".to_string());
                    return Some(true);
                };

                let Some((real, writable)) = self.android_symbol(&name) else {
                    self.stop_with_error(format!("Android symbol not found on iOS: {name}"));
                    return Some(true);
                };

                let token = register_symbol_region(&name, real, writable);
                unsafe {
                    destination.write(token);
                }
                Some(false)
            }

            // get_platform. Return Android intentionally: compatibility scripts
            // should follow their Android branch rather than an unknown-platform path.
            0x0dd5 => {
                let destination = self.read_variable_arg::<*mut u32>();
                unsafe {
                    destination.write(1);
                }
                Some(false)
            }

            // get_game_version. CLEO Android uses 17 for GTASA 2.00-or-higher.
            0x0dd6 => {
                let destination = self.read_variable_arg::<*mut u32>();
                unsafe {
                    destination.write(17);
                }
                Some(false)
            }

            // read_mem_addr
            0x0dd8 => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(3);
                let params = Self::script_params();
                let address = unsafe { params.read() };
                let size = unsafe { params.add(1).read() } as usize;
                let add_image_base = unsafe { params.add(2).read() } != 0;

                if !(1..=4).contains(&size) {
                    self.stop_with_error(format!("Android 0DD8 invalid read size {size}"));
                    return Some(true);
                }

                if add_image_base {
                    self.stop_with_error(
                        "Android 0DD8 add_ib=1 needs an iOS address translation".to_string(),
                    );
                    return Some(true);
                }

                let Some((source, _)) = self.resolve_virtual_address(address, size) else {
                    self.stop_with_error(format!(
                        "Android 0DD8 address {address:#010x} is not translated"
                    ));
                    return Some(true);
                };

                let mut value = 0u32;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        source as *const u8,
                        (&mut value as *mut u32).cast::<u8>(),
                        size,
                    );
                    destination.write(value);
                }
                Some(false)
            }

            // write_mem_addr. Script-local data is writable now; writes into GTA
            // symbols stay blocked until their segment protection is verified.
            0x0dd9 => {
                self.collect_value_args(5);
                let params = Self::script_params();
                let address = unsafe { params.read() };
                let value = unsafe { params.add(1).read() };
                let size = unsafe { params.add(2).read() } as usize;
                let add_image_base = unsafe { params.add(3).read() } != 0;

                if !(1..=4).contains(&size) {
                    self.stop_with_error(format!("Android 0DD9 invalid write size {size}"));
                    return Some(true);
                }

                if add_image_base {
                    self.stop_with_error(
                        "Android 0DD9 add_ib=1 needs an iOS address translation".to_string(),
                    );
                    return Some(true);
                }

                let Some((destination, writable)) = self.resolve_virtual_address(address, size) else {
                    self.stop_with_error(format!(
                        "Android 0DD9 address {address:#010x} is not translated"
                    ));
                    return Some(true);
                };

                if !writable {
                    self.stop_with_error(
                        "Android 0DD9 tried to write an unverified iOS symbol".to_string(),
                    );
                    return Some(true);
                }

                unsafe {
                    std::ptr::copy_nonoverlapping(
                        (&value as *const u32).cast::<u8>(),
                        destination,
                        size,
                    );
                }
                Some(false)
            }

            // set_mutex_var
            0x0ddc => {
                self.collect_value_args(2);
                let params = Self::script_params();
                let id = unsafe { params.read() };
                let value = unsafe { params.add(1).read() };
                MUTEX_VARS.lock().unwrap().insert(id, value);
                Some(false)
            }

            // get_mutex_var
            0x0ddd => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(1);
                let id = unsafe { Self::script_params().read() };
                let value = *MUTEX_VARS.lock().unwrap().get(&id).unwrap_or(&0);
                unsafe {
                    destination.write(value);
                }
                Some(false)
            }

            // Known Android CLEO opcodes not ported yet. Keep the error explicit so
            // each script tells us exactly what compatibility work remains.
            0x0dd2..=0x0dd4
            | 0x0dd7
            | 0x0dda..=0x0ddb
            | 0x0dde
            | 0x0de0..=0x0df6 => {
                self.stop_with_error(format!("Android CLEO opcode {opcode:#06x} not ported yet"));
                Some(true)
            }

            _ => None,
        }
    }

    fn update_one(&mut self) -> bool {
        let offset = self.game.ip as usize - self.game.base_ip as usize;
        if offset + 2 > self.bytes.len() {
            self.stop_with_error("Script instruction pointer moved past end of file".to_string());
            return true;
        }

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

        if let Some(can_interrupt) = self.update_android_opcode(opcode) {
            return can_interrupt;
        }

        // The game's touch-zone opcode needs its own jailed UIKit translation.
        if opcode == 0x00e1 {
            self.stop_with_error("iOS touch-zone opcode 0x00e1 not ported yet".to_string());
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
