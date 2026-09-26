//! Jailed CLEO script runtime.
//!
//! This module deliberately does not patch executable memory. It drives CLEO scripts
//! from a CADisplayLink installed on the main thread and calls the game's existing SCM
//! opcode handlers directly. The addresses match the CLEO 2.6.0 GTA:SA target.

use once_cell::sync::Lazy;
use std::{
    collections::HashMap,
    ffi::{c_char, c_void, CStr, CString},
    fs,
    io::Write,
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
const UPDATE_COMPARE_FLAG_ADDR: usize = 0x1001df890;
// Original CLEO iOS 2.6 writes this game variable from CTimer::GetCyclesPerMillisecond
// to enforce the selected frame cap. The jailed build cannot install that hook, so
// compatible 60 FPS scripts re-apply the same value from our main-thread runtime tick.
const FPS_CAP_ADDR: usize = 0x1008f07b8;

const ANDROID_IMAGE_VBASE: u32 = 0xB000_0000;

const MAX_INSTRUCTIONS_PER_TICK: usize = 512;
const SCRIPT_VIRTUAL_STRIDE: u32 = 0x0010_0000;
const SYMBOL_VIRTUAL_STRIDE: u32 = 0x0010_0000;
const SYMBOL_VIRTUAL_SPAN: u32 = 0x0000_1000;

const LC_SEGMENT_64: u32 = 0x19;
const VM_PROT_WRITE: i32 = 0x2;
const VM_PROT_EXECUTE: i32 = 0x4;

#[repr(C)]
struct MachHeader64 {
    magic: u32,
    cpu_type: i32,
    cpu_subtype: i32,
    file_type: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
struct LoadCommand {
    cmd: u32,
    cmdsize: u32,
}

#[repr(C)]
struct SegmentCommand64 {
    cmd: u32,
    cmdsize: u32,
    segname: [u8; 16],
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    maxprot: i32,
    initprot: i32,
    nsects: u32,
    flags: u32,
}

const LC_SYMTAB: u32 = 0x2;
const N_STAB: u8 = 0xe0;

#[repr(C)]
struct SymtabCommand {
    cmd: u32,
    cmdsize: u32,
    symoff: u32,
    nsyms: u32,
    stroff: u32,
    strsize: u32,
}

#[repr(C)]
struct Nlist64 {
    n_strx: u32,
    n_type: u8,
    n_sect: u8,
    n_desc: u16,
    n_value: u64,
}

extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
    fn _dyld_get_image_header(image_index: u32) -> *const MachHeader64;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(r#"
    .text
    .p2align 2
    .globl _cleo_android_call_arm64
_cleo_android_call_arm64:
    stp x19, x20, [sp, #-16]!
    mov x9, x0
    mov x10, x1
    mov x11, x2
    mov x19, x3
    mov x20, x4

    ldp x0, x1, [x10, #0]
    ldp x2, x3, [x10, #16]
    ldp x4, x5, [x10, #32]
    ldp x6, x7, [x10, #48]

    ldp d0, d1, [x11, #0]
    ldp d2, d3, [x11, #16]
    ldp d4, d5, [x11, #32]
    ldp d6, d7, [x11, #48]

    blr x9

    str x0, [x19]
    str d0, [x20]
    ldp x19, x20, [sp], #16
    ret
"#);

#[cfg(target_arch = "aarch64")]
extern "C" {
    fn cleo_android_call_arm64(
        target: usize,
        integer_regs: *const u64,
        float_regs: *const u64,
        result_x0: *mut u64,
        result_d0: *mut u64,
    );
}

fn image_contains_preferred_address(
    image_index: u32,
    preferred_address: usize,
    require_write: bool,
) -> bool {
    unsafe {
        let header = _dyld_get_image_header(image_index);
        if header.is_null() {
            return false;
        }

        let mut command =
            (header as *const u8).add(std::mem::size_of::<MachHeader64>());

        for _ in 0..(*header).ncmds {
            let load = &*(command as *const LoadCommand);
            if load.cmdsize < std::mem::size_of::<LoadCommand>() as u32 {
                return false;
            }

            if load.cmd == LC_SEGMENT_64
                && load.cmdsize >= std::mem::size_of::<SegmentCommand64>() as u32
            {
                let segment = &*(command as *const SegmentCommand64);
                let start = segment.vmaddr as usize;
                let end = start.saturating_add(segment.vmsize as usize);

                if preferred_address >= start
                    && preferred_address < end
                    && (!require_write || (segment.initprot & VM_PROT_WRITE) != 0)
                {
                    return true;
                }
            }

            command = command.add(load.cmdsize as usize);
        }
    }

    false
}

fn game_image_index() -> u32 {
    static INDEX: Lazy<u32> = Lazy::new(|| unsafe {
        let count = _dyld_image_count();

        // GAME_STATE_ADDR is a known writable variable in GTA:SA 2.02.11.
        // Identify the actual game Mach-O by preferred VM address instead of
        // assuming the game is always dyld image 0 or taking the smaller slide.
        for index in 0..count {
            if image_contains_preferred_address(index, GAME_STATE_ADDR, true) {
                return index;
            }
        }

        // Main executables are normally image 0. Keep a deterministic fallback
        // rather than guessing between unrelated dylibs.
        0
    });

    *INDEX
}

pub(crate) fn game_slide() -> usize {
    unsafe { _dyld_get_image_vmaddr_slide(game_image_index()).max(0) as usize }
}

fn canonical_runtime_address(address: usize) -> usize {
    // arm64e function pointers may contain pointer-authentication bits in the
    // upper part of the value. Strip only for range validation; calls still use
    // the original authenticated function pointer.
    #[cfg(target_arch = "aarch64")]
    {
        address & 0x0000_FFFF_FFFF_FFFFusize
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        address
    }
}

fn absolute(address: usize) -> usize {
    address + game_slide()
}

fn enforce_60_fps_cap() -> bool {
    static VERIFIED: Lazy<bool> = Lazy::new(|| {
        let cap = absolute(FPS_CAP_ADDR);
        segment_info(cap)
            .map(|(_, writable, executable)| writable && !executable)
            .unwrap_or(false)
    });

    if !*VERIFIED {
        return false;
    }

    unsafe {
        (absolute(FPS_CAP_ADDR) as *mut u32).write(60);
    }
    true
}

fn android_str_hash(value: &str) -> u32 {
    let mut hash = 0u32;
    for byte in value.bytes() {
        hash = hash.wrapping_add(byte as u32);
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash.wrapping_add(hash << 15)
}

fn parse_pattern(pattern: &str) -> Option<Vec<Option<u8>>> {
    let mut out = Vec::new();

    for part in pattern.split_whitespace() {
        if part == "?" || part == "??" {
            out.push(None);
            continue;
        }

        if part.len() != 2 {
            return None;
        }

        let value = u8::from_str_radix(part, 16).ok()?;
        out.push(Some(value));
    }

    (!out.is_empty()).then_some(out)
}

fn find_ios_pattern(pattern: &str, mut wanted_index: usize) -> Option<usize> {
    let pattern = parse_pattern(pattern)?;

    unsafe {
        let header = _dyld_get_image_header(game_image_index());
        if header.is_null() {
            return None;
        }

        let slide = _dyld_get_image_vmaddr_slide(game_image_index());
        let mut command = (header as *const u8).add(std::mem::size_of::<MachHeader64>());

        for _ in 0..(*header).ncmds {
            let load = &*(command as *const LoadCommand);
            if load.cmdsize < std::mem::size_of::<LoadCommand>() as u32 {
                return None;
            }

            if load.cmd == LC_SEGMENT_64
                && load.cmdsize >= std::mem::size_of::<SegmentCommand64>() as u32
            {
                let segment = &*(command as *const SegmentCommand64);

                if segment.initprot & VM_PROT_EXECUTE != 0
                    && segment.vmsize >= pattern.len() as u64
                    && segment.vmsize <= 128 * 1024 * 1024
                {
                    let start = (segment.vmaddr as isize + slide) as *const u8;
                    let size = segment.vmsize as usize;
                    let bytes = std::slice::from_raw_parts(start, size);

                    for offset in 0..=size - pattern.len() {
                        let matches = pattern.iter().enumerate().all(|(i, expected)| {
                            expected.map(|b| bytes[offset + i] == b).unwrap_or(true)
                        });

                        if matches {
                            if wanted_index == 0 {
                                return Some(start.add(offset) as usize);
                            }
                            wanted_index -= 1;
                        }
                    }
                }
            }

            command = command.add(load.cmdsize as usize);
        }
    }

    None
}


fn segment_name(bytes: &[u8; 16]) -> &str {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

fn find_macho_symbol(name: &str) -> Option<usize> {
    unsafe {
        let header = _dyld_get_image_header(game_image_index());
        if header.is_null() {
            return None;
        }

        let slide = _dyld_get_image_vmaddr_slide(game_image_index());
        let mut command = (header as *const u8).add(std::mem::size_of::<MachHeader64>());
        let mut symtab: Option<&SymtabCommand> = None;
        let mut linkedit: Option<&SegmentCommand64> = None;

        for _ in 0..(*header).ncmds {
            let load = &*(command as *const LoadCommand);
            if load.cmdsize < std::mem::size_of::<LoadCommand>() as u32 {
                return None;
            }

            if load.cmd == LC_SYMTAB
                && load.cmdsize >= std::mem::size_of::<SymtabCommand>() as u32
            {
                symtab = Some(&*(command as *const SymtabCommand));
            } else if load.cmd == LC_SEGMENT_64
                && load.cmdsize >= std::mem::size_of::<SegmentCommand64>() as u32
            {
                let segment = &*(command as *const SegmentCommand64);
                if segment_name(&segment.segname) == "__LINKEDIT" {
                    linkedit = Some(segment);
                }
            }

            command = command.add(load.cmdsize as usize);
        }

        let symtab = symtab?;
        let linkedit = linkedit?;

        let linkedit_base =
            (linkedit.vmaddr as isize + slide - linkedit.fileoff as isize) as *const u8;
        let symbols = linkedit_base.add(symtab.symoff as usize) as *const Nlist64;
        let strings = linkedit_base.add(symtab.stroff as usize);

        let mut candidates = Vec::with_capacity(2);
        candidates.push(name.to_string());
        candidates.push(format!("_{name}"));

        for index in 0..symtab.nsyms as usize {
            let entry = &*symbols.add(index);

            if entry.n_value == 0
                || entry.n_strx == 0
                || entry.n_strx >= symtab.strsize
                || entry.n_type & N_STAB != 0
            {
                continue;
            }

            let ptr = strings.add(entry.n_strx as usize).cast::<c_char>();
            let Ok(symbol) = CStr::from_ptr(ptr).to_str() else {
                continue;
            };

            if candidates.iter().any(|candidate| candidate == symbol) {
                return Some((entry.n_value as isize + slide) as usize);
            }
        }
    }

    None
}

fn segment_info(address: usize) -> Option<(usize, bool, bool)> {
    let address = canonical_runtime_address(address);

    unsafe {
        let header = _dyld_get_image_header(game_image_index());
        if header.is_null() {
            return None;
        }

        let slide = _dyld_get_image_vmaddr_slide(game_image_index());
        let mut command =
            (header as *const u8).add(std::mem::size_of::<MachHeader64>());

        for _ in 0..(*header).ncmds {
            let load = &*(command as *const LoadCommand);
            if load.cmdsize < std::mem::size_of::<LoadCommand>() as u32 {
                return None;
            }

            if load.cmd == LC_SEGMENT_64
                && load.cmdsize >= std::mem::size_of::<SegmentCommand64>() as u32
            {
                let segment = &*(command as *const SegmentCommand64);
                let start = canonical_runtime_address(
                    (segment.vmaddr as isize + slide) as usize
                );
                let end = start.saturating_add(segment.vmsize as usize);

                if address >= start && address < end {
                    return Some((
                        end.saturating_sub(address),
                        (segment.initprot & VM_PROT_WRITE) != 0,
                        (segment.initprot & VM_PROT_EXECUTE) != 0,
                    ));
                }
            }

            command = command.add(load.cmdsize as usize);
        }
    }

    None
}

fn is_executable_address(address: usize) -> bool {
    segment_info(address)
        .map(|(_, _, executable)| executable)
        .unwrap_or(false)
}

fn is_local_compat_callable(address: usize) -> bool {
    address == jailed_toggle_player_invincibility as usize
}

fn is_safe_callable(address: usize) -> bool {
    is_local_compat_callable(address) || is_executable_address(address)
}

fn find_native_game_symbol(name: &str) -> Option<usize> {
    let c_name = CString::new(name).ok()?;
    let ptr = unsafe {
        // Darwin RTLD_DEFAULT.
        dlsym((-2isize) as *mut c_void, c_name.as_ptr())
    };

    if !ptr.is_null() {
        return Some(ptr as usize);
    }

    find_macho_symbol(name)
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
static FXT_MAP: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_FUNCTION_TOKEN: AtomicU32 = AtomicU32::new(0xC000_0000);
static FUNCTION_TOKENS: Lazy<Mutex<HashMap<u32, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static INVINCIBILITY_COMPAT: Lazy<Mutex<Box<u8>>> =
    Lazy::new(|| Mutex::new(Box::new(0)));

extern "C" fn jailed_toggle_player_invincibility() {
    crate::jailed_cheats::run_index(7);
    let active = crate::jailed_cheats::active(7);
    **INVINCIBILITY_COMPAT.lock().unwrap() = if active { 1 } else { 0 };
}

fn invincibility_value_ptr() -> usize {
    let active = crate::jailed_cheats::active(7);
    let mut value = INVINCIBILITY_COMPAT.lock().unwrap();
    **value = if active { 1 } else { 0 };
    (&mut **value) as *mut u8 as usize
}

static PENDING_INVOKES: Lazy<Mutex<Vec<usize>>> = Lazy::new(|| Mutex::new(Vec::new()));

const MOBILE_MENU_ANDROID_SIZE: usize = 0x4c;
const MOBILE_MENU_ANDROID_TARGET_BLIP_OFFSET: usize = 0x48;
const RADAR_TRACE_COUNT: usize = 175;
const RADAR_TRACE_ANDROID_STRIDE: usize = 0x28;

static MOBILE_MENU_COMPAT: Lazy<Mutex<Box<[u8]>>> =
    Lazy::new(|| Mutex::new(vec![0u8; MOBILE_MENU_ANDROID_SIZE].into_boxed_slice()));
static RADAR_TRACE_COMPAT: Lazy<Mutex<Box<[u8]>>> = Lazy::new(|| {
    Mutex::new(
        vec![0u8; RADAR_TRACE_COUNT * RADAR_TRACE_ANDROID_STRIDE].into_boxed_slice()
    )
});
static WAYPOINT_SCAN_CACHE: Lazy<Mutex<(u32, Option<(f32, f32, f32)>)>> =
    Lazy::new(|| Mutex::new((u32::MAX, None)));

#[derive(Clone, Copy)]
struct RadarLayout {
    stride: usize,
    sprite_offset: usize,
}

const RADAR_LAYOUTS: [RadarLayout; 2] = [
    // Original 32-bit mobile tRadarTrace.
    RadarLayout { stride: 0x28, sprite_offset: 0x24 },
    // arm64 layout when the CEntryExit pointer expands to 8 bytes.
    RadarLayout { stride: 0x30, sprite_offset: 0x28 },
];

fn slice_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let data: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(data))
}

fn slice_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let data: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(data))
}

fn slice_f32(bytes: &[u8], offset: usize) -> Option<f32> {
    Some(f32::from_bits(slice_u32(bytes, offset)?))
}

fn radar_record_looks_valid(bytes: &[u8], start: usize, layout: RadarLayout) -> bool {
    if start.checked_add(layout.stride).map_or(true, |end| end > bytes.len()) {
        return false;
    }

    let Some(colour) = slice_u32(bytes, start) else {
        return false;
    };
    let Some(x) = slice_f32(bytes, start + 0x08) else {
        return false;
    };
    let Some(y) = slice_f32(bytes, start + 0x0c) else {
        return false;
    };
    let Some(z) = slice_f32(bytes, start + 0x10) else {
        return false;
    };
    let Some(radius) = slice_f32(bytes, start + 0x18) else {
        return false;
    };
    let Some(blip_size) = slice_u16(bytes, start + 0x1c) else {
        return false;
    };
    let Some(&sprite) = bytes.get(start + layout.sprite_offset) else {
        return false;
    };

    // These are deliberately broad sanity checks. They distinguish the radar
    // array from arbitrary writable data without assuming one exact game state.
    colour <= 8
        && x.is_finite()
        && y.is_finite()
        && z.is_finite()
        && radius.is_finite()
        && x.abs() <= 100_000.0
        && y.abs() <= 100_000.0
        && z.abs() <= 100_000.0
        && radius.abs() <= 100_000.0
        && blip_size <= 64
        && (sprite <= 64 || sprite >= 251)
}

fn waypoint_candidate_score(
    bytes: &[u8],
    record_start: usize,
    layout: RadarLayout,
) -> Option<(i32, f32, f32, f32)> {
    if !radar_record_looks_valid(bytes, record_start, layout) {
        return None;
    }

    if *bytes.get(record_start + layout.sprite_offset)? != 41 {
        return None;
    }

    let x = slice_f32(bytes, record_start + 0x08)?;
    let y = slice_f32(bytes, record_start + 0x0c)?;
    let z = slice_f32(bytes, record_start + 0x10)?;

    // San Andreas' playable map is roughly +/-3000. Leave headroom for mods,
    // but reject obvious non-coordinate values and the zeroed unused record.
    if x.abs() > 10_000.0
        || y.abs() > 10_000.0
        || z.abs() > 20_000.0
        || (x.abs() < 0.001 && y.abs() < 0.001)
    {
        return None;
    }

    let mut score = 20i32;

    // A real radar trace lives inside a contiguous array. Random byte 41 values
    // in __DATA are very unlikely to have valid tRadarTrace records on both sides.
    for delta in [-3isize, -2, -1, 1, 2, 3] {
        let byte_delta = delta.saturating_mul(layout.stride as isize);
        let neighbour = record_start as isize + byte_delta;
        if neighbour < 0 {
            continue;
        }

        if radar_record_looks_valid(bytes, neighbour as usize, layout) {
            score += 4;
        }
    }

    let colour = slice_u32(bytes, record_start).unwrap_or(u32::MAX);
    if colour == 8 {
        score += 4;
    }

    let blip_size = slice_u16(bytes, record_start + 0x1c).unwrap_or(u16::MAX);
    if blip_size <= 4 {
        score += 2;
    }

    Some((score, x, y, z))
}

fn scan_waypoint_from_game_data() -> Option<(f32, f32, f32)> {
    let mut best: Option<(i32, f32, f32, f32)> = None;

    unsafe {
        let header = _dyld_get_image_header(game_image_index());
        if header.is_null() {
            return None;
        }

        let slide = _dyld_get_image_vmaddr_slide(game_image_index());
        let mut command = (header as *const u8).add(std::mem::size_of::<MachHeader64>());

        for _ in 0..(*header).ncmds {
            let load = &*(command as *const LoadCommand);
            if load.cmdsize < std::mem::size_of::<LoadCommand>() as u32 {
                return None;
            }

            if load.cmd == LC_SEGMENT_64
                && load.cmdsize >= std::mem::size_of::<SegmentCommand64>() as u32
            {
                let segment = &*(command as *const SegmentCommand64);

                // The live radar pool is writable game data. Never scan executable
                // pages or giant mappings, and never dereference outside a segment.
                if segment.initprot & VM_PROT_WRITE != 0
                    && segment.vmsize >= 0x1000
                    && segment.vmsize <= 128 * 1024 * 1024
                {
                    let start = (segment.vmaddr as isize + slide) as *const u8;
                    let size = segment.vmsize as usize;
                    let bytes = std::slice::from_raw_parts(start, size);

                    for layout in RADAR_LAYOUTS {
                        if size <= layout.sprite_offset {
                            continue;
                        }

                        for sprite_pos in layout.sprite_offset..size {
                            if bytes[sprite_pos] != 41 {
                                continue;
                            }

                            let record_start = sprite_pos - layout.sprite_offset;
                            let Some(candidate) =
                                waypoint_candidate_score(bytes, record_start, layout)
                            else {
                                continue;
                            };

                            if best.map_or(true, |current| candidate.0 > current.0) {
                                best = Some(candidate);
                            }
                        }
                    }
                }
            }

            command = command.add(load.cmdsize as usize);
        }
    }

    // Require evidence that the candidate belongs to a contiguous radar array.
    // A single accidental byte value 41 must never be enough to move the player.
    match best {
        Some((score, x, y, z)) if score >= 36 => Some((x, y, z)),
        _ => None,
    }
}

fn target_blip_coords_from_game() -> Option<(f32, f32, f32)> {
    // Do not call opcode 0AB6 through the game's generic mobile handler here.
    // 0AB6 is a CLEO extension, not a native GTA SCM command. Calling the wrong
    // handler can return a stable but unrelated location, which is exactly what
    // caused teleport.csi to keep sending the player to the same place.
    //
    // Instead, locate the live waypoint record in GTA's writable radar pool.
    // Cache only within the exact same game tick: this avoids scanning the data
    // segments four times while one teleport script reads index/X/Y/icon, but
    // never reuses a waypoint from a previous frame after the user moves/removes it.
    let game_time = unsafe { read_global::<u32>(GAME_TIME_ADDR) };
    let mut cache = WAYPOINT_SCAN_CACHE.lock().unwrap();

    if cache.0 == game_time {
        return cache.1;
    }

    let coords = scan_waypoint_from_game_data();
    *cache = (game_time, coords);
    coords
}

fn native_ground_z_at(x: f32, y: f32) -> Option<f32> {
    // Standard GTA SCM opcode 02CE is native to the game, unlike CLEO opcode
    // 0AB6. Use the game's own command handler to obtain terrain height.
    let mut params = [0u8; 18];
    let mut at = 0usize;

    for value in [x, y, 2000.0f32] {
        params[at] = 0x06; // immediate real
        params[at + 1..at + 5].copy_from_slice(&value.to_bits().to_le_bytes());
        at += 5;
    }

    params[at] = 0x03; // local variable
    params[at + 1..at + 3].copy_from_slice(&0u16.to_le_bytes());

    let mut script = GameScript::new(params.as_ptr().cast::<u16>(), true);
    let opcode = 0x02ceu16;
    let handler_index = (opcode / 100) as usize;
    let handler_offset = handler_index * 2;
    let table = absolute(COMMAND_TABLE_ADDR) as *const usize;
    let handler_addr = unsafe { table.add(handler_offset).read() };

    if handler_addr == 0 {
        return None;
    }

    type Handler = fn(*mut GameScript, u16) -> u8;
    let handler: Handler = unsafe { std::mem::transmute(handler_addr) };
    let _ = handler(&mut script, opcode);

    let z = f32::from_bits(script.locals[0]);
    z.is_finite().then_some(z)
}

fn refresh_marker_compat() {
    let mut mobile = MOBILE_MENU_COMPAT.lock().unwrap();
    let mut radar = RADAR_TRACE_COMPAT.lock().unwrap();

    mobile.fill(0);
    radar.fill(0);

    let Some((x, y, z)) = target_blip_coords_from_game() else {
        return;
    };

    // Existing Android teleport scripts read only the low 16-bit radar-array
    // index from gMobileMenu+0x48. Give them a synthetic record at index 1.
    let synthetic_index: u16 = 1;
    mobile[MOBILE_MENU_ANDROID_TARGET_BLIP_OFFSET
        ..MOBILE_MENU_ANDROID_TARGET_BLIP_OFFSET + 2]
        .copy_from_slice(&synthetic_index.to_le_bytes());

    let base = synthetic_index as usize * RADAR_TRACE_ANDROID_STRIDE;

    // Android tRadarTrace: CVector position begins at +0x08 and the sprite id is
    // at +0x24. Icon 41 is the map waypoint used by the classic teleport script.
    radar[base + 0x08..base + 0x0c].copy_from_slice(&x.to_bits().to_le_bytes());
    radar[base + 0x0c..base + 0x10].copy_from_slice(&y.to_bits().to_le_bytes());
    radar[base + 0x10..base + 0x14].copy_from_slice(&z.to_bits().to_le_bytes());
    radar[base + 0x24] = 41;
}

fn refresh_android_adapter(name: &str) {
    match name {
        "gMobileMenu" | "_ZN6CRadar13ms_RadarTraceE" => refresh_marker_compat(),
        _ => {}
    }
}

fn register_function_token(real: usize) -> u32 {
    if real == 0 {
        return 0;
    }

    let mut functions = FUNCTION_TOKENS.lock().unwrap();
    if let Some((token, _)) = functions.iter().find(|(_, addr)| **addr == real) {
        return *token;
    }

    let token = NEXT_FUNCTION_TOKEN.fetch_add(0x100, Ordering::SeqCst);
    functions.insert(token, real);
    token
}

static CHEAT_FUNCTION_COMPAT: Lazy<Box<[u32]>> = Lazy::new(|| {
    let mut out = Vec::with_capacity(crate::jailed_cheats::CHEAT_COUNT);
    for index in 0..crate::jailed_cheats::CHEAT_COUNT {
        let real = crate::jailed_cheats::function_address(index);
        out.push(register_function_token(real));
    }
    out.into_boxed_slice()
});

fn register_symbol_region(name: &str, real_base: usize, writable: bool, span: u32) -> u32 {
    let mut regions = SYMBOL_REGIONS.lock().unwrap();

    if let Some(existing) = regions.iter().find(|region| region.name == name) {
        return existing.virtual_base;
    }

    let virtual_base = NEXT_SYMBOL_VBASE.fetch_add(SYMBOL_VIRTUAL_STRIDE, Ordering::SeqCst);
    regions.push(AddressRegion {
        virtual_base,
        real_base,
        span: span.max(1),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpecialScript {
    None,
    Fps60,
}

impl SpecialScript {
    fn from_name(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("60fps") && lower.ends_with(".csa") {
            Self::Fps60
        } else {
            Self::None
        }
    }
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
    context: [u64; 32],
    special: SpecialScript,
    text_style: TextDrawState,
    text_frame: Vec<crate::jailed::ScriptTextDraw>,
}

unsafe impl Send for Script {}

#[derive(Debug, Clone, Copy)]
enum NativeArg {
    Integer(u64),
    Float(u32),
}
#[derive(Debug, Clone, Copy)]
struct TextDrawState {
    scale_x: f32,
    scale_y: f32,
    rgba: [u8; 4],
    centered: bool,
    right_aligned: bool,
    outline: bool,
}

impl Default for TextDrawState {
    fn default() -> Self {
        Self {
            scale_x: 0.30,
            scale_y: 1.00,
            rgba: [255, 255, 255, 255],
            centered: false,
            right_aligned: false,
            outline: false,
        }
    }
}


impl Script {
    fn new(kind: Kind, bytes: Vec<u8>, name: String) -> Self {
        let ip = bytes.as_ptr().cast::<u16>();
        let special = SpecialScript::from_name(&name);

        Self {
            kind,
            bytes,
            game: GameScript::new(ip, false),
            name,
            // Keep arbitrary Android CSA scripts on safe-start, but the known
            // 60 FPS compatibility adapter is an iOS-side cap writer and is safe to
            // enable automatically just like original CLEO iOS defaults to 60 FPS.
            enabled: special == SpecialScript::Fps60,
            error: None,
            virtual_base: NEXT_SCRIPT_VBASE.fetch_add(SCRIPT_VIRTUAL_STRIDE, Ordering::SeqCst),
            context: [0; 32],
            special,
            text_style: TextDrawState::default(),
            text_frame: Vec::new(),
        }
    }

    fn reset(&mut self, active: bool) {
        let base = self.bytes.as_ptr().cast::<u16>();
        self.game = GameScript::new(base, active);
        self.error = None;
        self.context = [0; 32];
        self.text_style = TextDrawState::default();
        self.text_frame.clear();
    }

    fn stop_with_error(&mut self, message: String) {
        self.game.active = false;
        runtime_log(&format!("[ERROR] {}: {}", self.name, message));
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

    fn update_bool_flag(&mut self, value: bool) {
        let update: fn(*mut GameScript, bool) =
            unsafe { std::mem::transmute(absolute(UPDATE_COMPARE_FLAG_ADDR)) };
        update(&mut self.game, value);
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
            refresh_android_adapter(&region.name);
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

    fn read_virtual_u8(&self, address: u32) -> Option<u8> {
        let (ptr, _) = self.resolve_virtual_address(address, 1)?;
        Some(unsafe { ptr.read() })
    }

    fn read_virtual_c_string_advance(&self, address: &mut u32) -> Option<String> {
        let start = *address;
        let value = self.read_virtual_c_string(start)?;
        *address = address.wrapping_add(value.len() as u32 + 1);
        Some(value)
    }

    fn resolve_callable(&self, address: u32) -> Option<usize> {
        if let Some(real) = FUNCTION_TOKENS.lock().unwrap().get(&address).copied() {
            return is_safe_callable(real).then_some(real);
        }

        let real = self.resolve_virtual_address(address, 1)?.0 as usize;

        // Never execute script bytes or writable/data mappings as code.
        // Android 0DD2/0DDE scripts frequently manipulate pointers; treating an
        // arbitrary virtual address as a callable function is an immediate crash.
        is_safe_callable(real).then_some(real)
    }

    fn call_integer_function(&mut self, address: u32, args: &[u64]) -> Option<u64> {
        let real = self.resolve_callable(address)?;

        unsafe {
            Some(match args.len() {
                0 => {
                    let f: extern "C" fn() -> u64 = std::mem::transmute(real);
                    f()
                }
                1 => {
                    let f: extern "C" fn(u64) -> u64 = std::mem::transmute(real);
                    f(args[0])
                }
                2 => {
                    let f: extern "C" fn(u64, u64) -> u64 = std::mem::transmute(real);
                    f(args[0], args[1])
                }
                3 => {
                    let f: extern "C" fn(u64, u64, u64) -> u64 = std::mem::transmute(real);
                    f(args[0], args[1], args[2])
                }
                4 => {
                    let f: extern "C" fn(u64, u64, u64, u64) -> u64 = std::mem::transmute(real);
                    f(args[0], args[1], args[2], args[3])
                }
                5 => {
                    let f: extern "C" fn(u64, u64, u64, u64, u64) -> u64 =
                        std::mem::transmute(real);
                    f(args[0], args[1], args[2], args[3], args[4])
                }
                6 => {
                    let f: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 =
                        std::mem::transmute(real);
                    f(args[0], args[1], args[2], args[3], args[4], args[5])
                }
                7 => {
                    let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64 =
                        std::mem::transmute(real);
                    f(args[0], args[1], args[2], args[3], args[4], args[5], args[6])
                }
                8 => {
                    let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                        std::mem::transmute(real);
                    f(
                        args[0], args[1], args[2], args[3],
                        args[4], args[5], args[6], args[7],
                    )
                }
                _ => return None,
            })
        }
    }

    fn call_mixed_function(
        &mut self,
        address: u32,
        args: &[NativeArg],
    ) -> Option<(u64, u32)> {
        let real = self.resolve_callable(address)?;

        #[cfg(target_arch = "aarch64")]
        {
            let mut integer_regs = [0u64; 8];
            let mut float_regs = [0u64; 8];
            let mut integer_count = 0usize;
            let mut float_count = 0usize;

            for arg in args {
                match *arg {
                    NativeArg::Integer(value) => {
                        if integer_count >= integer_regs.len() {
                            return None;
                        }
                        integer_regs[integer_count] = value;
                        integer_count += 1;
                    }
                    NativeArg::Float(bits) => {
                        if float_count >= float_regs.len() {
                            return None;
                        }
                        // AArch64 passes a 32-bit float in the low half of Vn.
                        float_regs[float_count] = bits as u64;
                        float_count += 1;
                    }
                }
            }

            let mut result_x0 = 0u64;
            let mut result_d0 = 0u64;

            unsafe {
                cleo_android_call_arm64(
                    real,
                    integer_regs.as_ptr(),
                    float_regs.as_ptr(),
                    &mut result_x0,
                    &mut result_d0,
                );
            }

            return Some((result_x0, result_d0 as u32));
        }

        #[cfg(not(target_arch = "aarch64"))]
        {
            let integer_args: Option<Vec<u64>> = args
                .iter()
                .map(|arg| match arg {
                    NativeArg::Integer(value) => Some(*value),
                    NativeArg::Float(_) => None,
                })
                .collect();

            self.call_integer_function(address, &integer_args?).map(|value| (value, 0))
        }
    }

    fn read_8byte_string_param(&mut self) -> Option<Option<String>> {
        let base = self.game.base_ip as usize;
        let current = self.game.ip as usize;
        let offset = current.checked_sub(base)?;

        if offset >= self.bytes.len() {
            return None;
        }

        unsafe {
            let p = self.game.ip.cast::<u8>();
            let tag = p.read();

            if tag == 0 {
                self.game.ip = p.add(1).cast::<u16>();
                return Some(None);
            }

            if tag != 0x09 || offset.saturating_add(9) > self.bytes.len() {
                return None;
            }

            let bytes = std::slice::from_raw_parts(p.add(1), 8);
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            let value = String::from_utf8(bytes[..end].to_vec()).ok()?;
            self.game.ip = p.add(9).cast::<u16>();
            Some(Some(value))
        }
    }

    fn translate_fxt(key: &str) -> String {
        FXT_MAP
            .lock()
            .unwrap()
            .get(&key.to_ascii_uppercase())
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    fn lookup_fxt(key: &str) -> Option<String> {
        FXT_MAP
            .lock()
            .unwrap()
            .get(&key.to_ascii_uppercase())
            .cloned()
    }

    fn format_fxt_numbers(mut text: String, numbers: &[i32]) -> String {
        for number in numbers {
            if let Some(index) = text.find("~1~") {
                text.replace_range(index..index + 3, &number.to_string());
            }
        }
        text
    }

    fn mirror_text_draw_state(&mut self, opcode: u16) {
        let saved_ip = self.game.ip;

        match opcode {
            0x033f => {
                self.collect_value_args(2);
                let params = Self::script_params();
                self.text_style.scale_x =
                    f32::from_bits(unsafe { params.read() }).clamp(0.05, 4.0);
                self.text_style.scale_y =
                    f32::from_bits(unsafe { params.add(1).read() }).clamp(0.05, 4.0);
            }
            0x0340 => {
                self.collect_value_args(4);
                let params = Self::script_params();
                for index in 0..4usize {
                    self.text_style.rgba[index] =
                        unsafe { params.add(index).read() }.min(255) as u8;
                }
            }
            0x0342 => {
                self.collect_value_args(1);
                self.text_style.centered = unsafe { Self::script_params().read() } != 0;
                if self.text_style.centered {
                    self.text_style.right_aligned = false;
                }
            }
            0x03e4 => {
                self.collect_value_args(1);
                self.text_style.right_aligned = unsafe { Self::script_params().read() } != 0;
                if self.text_style.right_aligned {
                    self.text_style.centered = false;
                }
            }
            0x081c => {
                self.collect_value_args(5);
                self.text_style.outline = unsafe { Self::script_params().read() } != 0;
            }
            0x03f0 => {
                self.collect_value_args(1);
                let enabled = unsafe { Self::script_params().read() } != 0;
                if !enabled {
                    crate::jailed::render_script_text_frame(std::mem::take(&mut self.text_frame));
                }
            }
            _ => return,
        }

        self.game.ip = saved_ip;
    }

    fn push_custom_text(&mut self, x: f32, y: f32, text: String) {
        self.text_frame.push(crate::jailed::ScriptTextDraw {
            x,
            y,
            scale_x: self.text_style.scale_x,
            scale_y: self.text_style.scale_y,
            rgba: self.text_style.rgba,
            centered: self.text_style.centered,
            right_aligned: self.text_style.right_aligned,
            outline: self.text_style.outline,
            text,
        });
    }

    fn update_custom_fxt_draw_opcode(&mut self, opcode: u16) -> Option<bool> {
        let saved_ip = self.game.ip;

        match opcode {
            0x033e => {
                self.collect_value_args(2);
                let params = Self::script_params();
                let x = f32::from_bits(unsafe { params.read() });
                let y = f32::from_bits(unsafe { params.add(1).read() });
                let key = match self.read_8byte_string_param() {
                    Some(Some(value)) => value,
                    _ => {
                        self.game.ip = saved_ip;
                        return None;
                    }
                };

                let Some(text) = Self::lookup_fxt(&key) else {
                    self.game.ip = saved_ip;
                    return None;
                };

                self.push_custom_text(x, y, text);
                Some(false)
            }
            0x045a => {
                self.collect_value_args(2);
                let params = Self::script_params();
                let x = f32::from_bits(unsafe { params.read() });
                let y = f32::from_bits(unsafe { params.add(1).read() });
                let key = match self.read_8byte_string_param() {
                    Some(Some(value)) => value,
                    _ => {
                        self.game.ip = saved_ip;
                        return None;
                    }
                };
                self.collect_value_args(1);
                let number = unsafe { Self::script_params().read() } as i32;

                let Some(text) = Self::lookup_fxt(&key) else {
                    self.game.ip = saved_ip;
                    return None;
                };

                self.push_custom_text(x, y, Self::format_fxt_numbers(text, &[number]));
                Some(false)
            }
            0x045b => {
                self.collect_value_args(2);
                let params = Self::script_params();
                let x = f32::from_bits(unsafe { params.read() });
                let y = f32::from_bits(unsafe { params.add(1).read() });
                let key = match self.read_8byte_string_param() {
                    Some(Some(value)) => value,
                    _ => {
                        self.game.ip = saved_ip;
                        return None;
                    }
                };
                self.collect_value_args(2);
                let params = Self::script_params();
                let first = unsafe { params.read() } as i32;
                let second = unsafe { params.add(1).read() } as i32;

                let Some(text) = Self::lookup_fxt(&key) else {
                    self.game.ip = saved_ip;
                    return None;
                };

                self.push_custom_text(
                    x,
                    y,
                    Self::format_fxt_numbers(text, &[first, second]),
                );
                Some(false)
            }
            _ => None,
        }
    }

    fn android_symbol(&self, name: &str) -> Option<(usize, bool, u32)> {
        // Synthetic 32-bit views for data whose iOS arm64 layout differs from
        // the Android layout expected by existing CSA/CSI bytecode.
        match name {
            "_ZN10CPlayerPed22bDebugPlayerInvincibleE" => {
                return Some((invincibility_value_ptr(), false, 1));
            }
            "_ZN6CCheat25TogglePlayerInvincibilityEv" => {
                return Some((jailed_toggle_player_invincibility as usize, false, 1));
            }
            "_ZN6CCheat17m_aCheatFunctionsE" => {
                return Some((
                    CHEAT_FUNCTION_COMPAT.as_ptr() as usize,
                    false,
                    (CHEAT_FUNCTION_COMPAT.len() * std::mem::size_of::<u32>()) as u32,
                ));
            }
            "_ZN6CCheat15m_aCheatsActiveE" => {
                return Some((
                    crate::jailed_cheats::absolute(crate::jailed_cheats::CHEAT_ACTIVE_FLAGS),
                    true,
                    crate::jailed_cheats::CHEAT_COUNT as u32,
                ));
            }
            "gMobileMenu" => {
                refresh_marker_compat();
                let compat = MOBILE_MENU_COMPAT.lock().unwrap();
                return Some((
                    compat.as_ptr() as usize,
                    false,
                    MOBILE_MENU_ANDROID_SIZE as u32,
                ));
            }
            "_ZN6CRadar13ms_RadarTraceE" => {
                refresh_marker_compat();
                let compat = RADAR_TRACE_COMPAT.lock().unwrap();
                return Some((
                    compat.as_ptr() as usize,
                    false,
                    (RADAR_TRACE_COUNT * RADAR_TRACE_ANDROID_STRIDE) as u32,
                ));
            }
            _ => {}
        }

        // Direct mapping is safe for symbols whose consumer doesn't depend on a
        // 32-bit structure layout. Start with exported symbols, then use Mach-O's
        // local nlist table when the App Store binary kept the symbol locally.
        let ptr = find_native_game_symbol(name)?;
        let span = segment_info(ptr)
            .map(|(remaining, _, _)| remaining.min(SYMBOL_VIRTUAL_SPAN as usize) as u32)
            .unwrap_or(1)
            .max(1);
        Some((ptr, false, span))
    }

    fn update_android_opcode(&mut self, opcode: u16) -> Option<bool> {
        match opcode {
            // CLEO Android: store_target_marker_coords_to X Y Z // IF and SET
            //
            // This must be intercepted here. 0AB6 is a CLEO extension, not a
            // native GTA SCM opcode; forwarding it to GTA's generic mobile
            // handler can execute unrelated code.
            0x0ab6 => {
                let out_x = self.read_variable_arg::<*mut u32>();
                let out_y = self.read_variable_arg::<*mut u32>();
                let out_z = self.read_variable_arg::<*mut u32>();

                if let Some((x, y, radar_z)) = target_blip_coords_from_game() {
                    let z = native_ground_z_at(x, y).unwrap_or(radar_z);
                    unsafe {
                        out_x.write(x.to_bits());
                        out_y.write(y.to_bits());
                        out_z.write(z.to_bits());
                    }
                    self.update_bool_flag(true);
                } else {
                    unsafe {
                        out_x.write(0);
                        out_y.write(0);
                        out_z.write(0);
                    }
                    self.update_bool_flag(false);
                }

                Some(false)
            }

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

                let Some((real, writable, span)) = self.android_symbol(&name) else {
                    self.stop_with_error(format!(
                        "Android symbol needs iOS adapter: {name}"
                    ));
                    return Some(true);
                };

                let token = if is_safe_callable(real) {
                    register_function_token(real)
                } else {
                    register_symbol_region(&name, real, writable, span)
                };

                unsafe {
                    destination.write(token);
                }
                Some(false)
            }

            // context_call
            0x0dd2 => {
                self.collect_value_args(1);
                let address = unsafe { Self::script_params().read() };
                let args = self.context[..8].to_vec();

                let Some(result) = self.call_integer_function(address, &args) else {
                    self.stop_with_error(format!(
                        "Android context call target {address:#010x} is not callable on iOS"
                    ));
                    return Some(true);
                };

                self.context[0] = result;
                Some(false)
            }

            // context_set_reg
            0x0dd3 => {
                self.collect_value_args(2);
                let params = Self::script_params();
                let reg = unsafe { params.read() } as usize;
                let value = unsafe { params.add(1).read() } as u64;

                if reg >= self.context.len() {
                    self.stop_with_error(format!("Android context register {reg} is invalid"));
                    return Some(true);
                }

                self.context[reg] = value;
                Some(false)
            }

            // context_get_reg
            0x0dd4 => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(1);
                let reg = unsafe { Self::script_params().read() } as usize;

                if reg >= self.context.len() {
                    self.stop_with_error(format!("Android context register {reg} is invalid"));
                    return Some(true);
                }

                unsafe {
                    destination.write(self.context[reg] as u32);
                }
                Some(false)
            }

            // get_platform. Report Android for compatibility so scripts follow
            // their Android code path, which this module translates to iOS.
            0x0dd5 => {
                let destination = self.read_variable_arg::<*mut u32>();
                unsafe {
                    destination.write(1);
                }
                Some(false)
            }

            // get_game_version. 17 = GTASA 2.00-or-higher in CLEO Android.
            0x0dd6 => {
                let destination = self.read_variable_arg::<*mut u32>();
                unsafe {
                    destination.write(17);
                }
                Some(false)
            }

            // get_image_base. This is a virtual Android image token, not an iOS
            // pointer. Fixed Android offsets still require a translation adapter.
            0x0dd7 => {
                let destination = self.read_variable_arg::<*mut u32>();
                unsafe {
                    destination.write(ANDROID_IMAGE_VBASE);
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
                    self.stop_with_error(format!(
                        "Android fixed image offset {address:#x} needs an iOS translation"
                    ));
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

            // write_mem_addr
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
                    self.stop_with_error(format!(
                        "Android fixed image offset {address:#x} needs an iOS translation"
                    ));
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
                        "Android 0DD9 tried to write an unverified iOS region".to_string(),
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

            // search_mem. Android machine-code signatures normally do not match
            // arm64 iOS, but iOS signatures are supported by the compatibility scanner.
            0x0dda => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(2);
                let params = Self::script_params();
                let pattern_address = unsafe { params.read() };
                let index = unsafe { params.add(1).read() } as usize;

                let Some(pattern) = self.read_virtual_c_string(pattern_address) else {
                    self.stop_with_error("Android 0DDA received an invalid pattern string".to_string());
                    return Some(true);
                };

                let value = if let Some(real) = find_ios_pattern(&pattern, index) {
                    register_symbol_region(
                        &format!("pattern:{pattern}:{index}"),
                        real,
                        false,
                        0x10000,
                    )
                } else {
                    0
                };

                unsafe {
                    destination.write(value);
                }
                Some(false)
            }

            // get_game_ver_ex. Report a stable Android-compatible GTASA 2.x identity.
            0x0ddb => {
                let id = self.read_variable_arg::<*mut u32>();
                let version = self.read_variable_arg::<*mut u32>();
                let version_code = self.read_variable_arg::<*mut u32>();

                unsafe {
                    id.write(android_str_hash("com.rockstargames.gtasa"));
                    version.write(android_str_hash("2.00"));
                    version_code.write(20);
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

            // call_func. Supports the integer/pointer ABI directly. Android-style
            // floating-point varargs need a dedicated arm64 FP trampoline and are
            // rejected instead of being called with the wrong ABI.
            0x0dde => {
                self.collect_value_args(2);
                let params = Self::script_params();
                let address = unsafe { params.read() };
                let add_image_base = unsafe { params.add(1).read() } != 0;

                if add_image_base {
                    self.stop_with_error(
                        "Android 0DDE add_ib=1 needs an iOS function translation".to_string(),
                    );
                    return Some(true);
                }

                let mut args: Vec<NativeArg> = Vec::new();
                let mut integer_result_ptr: Option<*mut u32> = None;
                let mut float_result_ptr: Option<*mut u32> = None;

                loop {
                    let Some(kind) = self.read_8byte_string_param() else {
                        self.stop_with_error(
                            "Android 0DDE has an invalid typed parameter list".to_string(),
                        );
                        return Some(true);
                    };

                    let Some(kind) = kind else {
                        break;
                    };

                    match kind.to_ascii_lowercase().as_str() {
                        "i" => {
                            self.collect_value_args(1);
                            args.push(NativeArg::Integer(
                                unsafe { Self::script_params().read() } as u64,
                            ));
                        }
                        "f" => {
                            self.collect_value_args(1);
                            args.push(NativeArg::Float(
                                unsafe { Self::script_params().read() },
                            ));
                        }
                        "ref" => {
                            let ptr = self.read_variable_arg::<*mut u32>();
                            args.push(NativeArg::Integer(ptr as usize as u64));
                        }
                        "resi" => {
                            integer_result_ptr = Some(self.read_variable_arg::<*mut u32>());
                        }
                        "resf" => {
                            float_result_ptr = Some(self.read_variable_arg::<*mut u32>());
                        }
                        other => {
                            self.stop_with_error(format!(
                                "Android 0DDE unknown parameter type '{other}'"
                            ));
                            return Some(true);
                        }
                    }

                    let integer_count = args
                        .iter()
                        .filter(|arg| matches!(arg, NativeArg::Integer(_)))
                        .count();
                    let float_count = args
                        .iter()
                        .filter(|arg| matches!(arg, NativeArg::Float(_)))
                        .count();

                    if integer_count > 8 || float_count > 8 {
                        self.stop_with_error(
                            "Android 0DDE exceeds the arm64 register argument limit"
                                .to_string(),
                        );
                        return Some(true);
                    }
                }

                let Some((integer_result, float_result)) =
                    self.call_mixed_function(address, &args)
                else {
                    self.stop_with_error(format!(
                        "Android 0DDE target {address:#010x} is not callable on iOS"
                    ));
                    return Some(true);
                };

                if let Some(ptr) = integer_result_ptr {
                    unsafe {
                        ptr.write(integer_result as u32);
                    }
                }

                if let Some(ptr) = float_result_ptr {
                    unsafe {
                        ptr.write(float_result);
                    }
                }

                Some(false)
            }

            // get_touch_point_state
            0x0de0 => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(2);
                let params = Self::script_params();
                let zone = unsafe { params.read() };
                let min_time = unsafe { params.add(1).read() };

                unsafe {
                    destination.write(
                        crate::jailed_touch::point_touched_timed(zone, min_time) as u32
                    );
                }
                Some(false)
            }

            // get_touch_slide_state
            0x0de1 => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(4);
                let params = Self::script_params();
                let from = unsafe { params.read() };
                let to = unsafe { params.add(1).read() };
                let min_time = unsafe { params.add(2).read() };
                let max_time = unsafe { params.add(3).read() };

                unsafe {
                    destination.write(
                        crate::jailed_touch::slide_done(from, to, min_time, max_time) as u32
                    );
                }
                Some(false)
            }

            // Android menu/back button state.
            0x0de2 => {
                let destination = self.read_variable_arg::<*mut u32>();
                unsafe {
                    destination.write(crate::jailed_touch::menu_button_state() as u32);
                }
                Some(false)
            }

            0x0de3 => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(1);
                let min_time = unsafe { Self::script_params().read() };
                unsafe {
                    destination.write(
                        crate::jailed_touch::menu_button_pressed_timed(min_time) as u32
                    );
                }
                Some(false)
            }

            // PSP control opcodes are meaningful only on PSP. Android CLEO returns 0.
            0x0de4 => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(1);
                unsafe { destination.write(0); }
                Some(false)
            }

            0x0de5 => {
                let destination = self.read_variable_arg::<*mut u32>();
                self.collect_value_args(2);
                unsafe { destination.write(0); }
                Some(false)
            }

            // invokable_script_stats
            0x0dee => {
                let count_ptr = self.read_variable_arg::<*mut u32>();
                let page_ptr = self.read_variable_arg::<*mut u32>();
                let count = CSI_COUNT.load(Ordering::SeqCst);
                unsafe {
                    count_ptr.write(count);
                    page_ptr.write(count / 12 + 1);
                }
                Some(false)
            }

            // start_invokable_script. Requests are deferred until the current
            // script iteration finishes so we never recursively lock SCRIPTS.
            0x0def => {
                let result = self.read_variable_arg::<*mut i32>();
                self.collect_value_args(1);
                let id = unsafe { Self::script_params().read() } as usize;
                PENDING_INVOKES.lock().unwrap().push(id);
                unsafe { result.write(0); }
                Some(false)
            }

            // Android CLEO's "show menu arrow" hint is not needed on iOS.
            0x0df0 | 0x0df1 => Some(false),

            // create_menu
            0x0df2 => {
                self.collect_value_args(2);
                let params = Self::script_params();
                let mut address = unsafe { params.read() };
                let mut item_count = unsafe { params.add(1).read() } as usize;

                if item_count > 256 {
                    self.stop_with_error(format!(
                        "Android menu requested {item_count} items; maximum safe count is 256"
                    ));
                    return Some(true);
                }

                let Some(flags) = self.read_virtual_u8(address) else {
                    self.stop_with_error("Android 0DF2 menu descriptor is invalid".to_string());
                    return Some(true);
                };
                address = address.wrapping_add(4);

                let Some(title) = self.read_virtual_c_string_advance(&mut address) else {
                    self.stop_with_error("Android 0DF2 menu title is invalid".to_string());
                    return Some(true);
                };
                let Some(close) = self.read_virtual_c_string_advance(&mut address) else {
                    self.stop_with_error("Android 0DF2 close title is invalid".to_string());
                    return Some(true);
                };

                let use_gxt = flags & 1 != 0;
                let mut items = Vec::new();

                while item_count > 0 {
                    let Some(item) = self.read_virtual_c_string_advance(&mut address) else {
                        break;
                    };
                    if item.is_empty() {
                        break;
                    }

                    items.push(if use_gxt {
                        Self::translate_fxt(&item)
                    } else {
                        item
                    });
                    item_count -= 1;
                }

                crate::jailed::show_android_menu(title, close, items);
                Some(false)
            }

            0x0df3 => {
                crate::jailed::hide_android_menu();
                Some(false)
            }

            0x0df4 => {
                let destination = self.read_variable_arg::<*mut i32>();
                self.collect_value_args(1);
                let max_time = unsafe { Self::script_params().read() };
                unsafe {
                    destination.write(crate::jailed::android_menu_take_selected(max_time));
                }
                Some(false)
            }

            0x0df5 => {
                self.collect_value_args(1);
                let index = unsafe { Self::script_params().read() as i32 };
                crate::jailed::android_menu_set_active(index);
                Some(false)
            }

            0x0df6 => {
                let destination = self.read_variable_arg::<*mut i32>();
                unsafe {
                    destination.write(crate::jailed::android_menu_get_active());
                }
                Some(false)
            }

            _ => None,
        }
    }

    fn update_one(&mut self) -> bool {
        let Some(offset) = (self.game.ip as usize).checked_sub(self.game.base_ip as usize) else {
            self.stop_with_error("Script instruction pointer moved before start of file".to_string());
            return true;
        };

        if offset.saturating_add(2) > self.bytes.len() {
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

        // CLEO Android replaces both ENDTHREAD (004E) and ENDCUSTOMTHREAD
        // (05DC). Never let GTA free memory owned by our Rust script object.
        if opcode == 0x004e || opcode == 0x05dc {
            self.reset(false);
            return true;
        }

        if let Some(can_interrupt) = self.update_android_opcode(opcode) {
            return can_interrupt;
        }

        self.mirror_text_draw_state(opcode);

        if let Some(can_interrupt) = self.update_custom_fxt_draw_opcode(opcode) {
            return can_interrupt;
        }

        // Never pass an unknown CLEO-Android opcode into GTA's generic mobile
        // extended-opcode handler. The numeric range belongs to CLEO Android;
        // forwarding an unsupported one can jump through unrelated handler logic
        // and is a common source of hard crashes. Quarantine the script instead.
        if (0x0dd0..=0x0dff).contains(&opcode) {
            self.stop_with_error(format!(
                "Unsupported Android CLEO opcode {opcode:#06x}; script quarantined"
            ));
            return true;
        }

        // CLEO Android overloads KEY/NOT_KEY (00E1/80E1) for touch zones.
        // Its point_touched() remains true for 150 ms after any touch event, so
        // simultaneous combinations such as zones 2+4 work reliably.
        if opcode == 0x00e1 {
            self.collect_value_args(2);
            let zone = unsafe { Self::script_params().add(1).read() };
            self.update_bool_flag(crate::jailed_touch::point_touched_recent(zone));
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

        if !is_executable_address(handler_addr) {
            self.stop_with_error(format!(
                "Unsafe handler address {handler_addr:#x} for opcode {opcode:#06x}; script quarantined"
            ));
            return true;
        }

        if self.game.stack_pos as usize > self.game.call_stack.len() {
            self.stop_with_error(format!(
                "Invalid script call-stack depth {} before opcode {opcode:#06x}",
                self.game.stack_pos
            ));
            return true;
        }

        let handler: Handler = unsafe { std::mem::transmute(handler_addr) };
        let interrupted = handler(&mut self.game, opcode) != 0;

        let base = self.game.base_ip as usize;
        let current = self.game.ip as usize;
        let valid_ip = current
            .checked_sub(base)
            .map(|offset| offset <= self.bytes.len())
            .unwrap_or(false);

        if !valid_ip {
            self.stop_with_error(format!(
                "Opcode {opcode:#06x} moved the instruction pointer outside the script; quarantined"
            ));
            return true;
        }

        if self.game.stack_pos as usize > self.game.call_stack.len() {
            self.stop_with_error(format!(
                "Opcode {opcode:#06x} corrupted the script call stack; quarantined"
            ));
            return true;
        }

        interrupted
    }

    fn update(&mut self) {
        if !self.game.active {
            return;
        }

        if self.special == SpecialScript::Fps60 {
            if !enforce_60_fps_cap() {
                self.stop_with_error(
                    "60 FPS adapter could not verify the iOS frame-cap variable".to_string(),
                );
            }
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
static CSI_COUNT: AtomicU32 = AtomicU32::new(0);
static CSA_AUTOSTART_DONE: AtomicBool = AtomicBool::new(false);
static CSA_START_AFTER: AtomicU32 = AtomicU32::new(0);
const CSA_STARTUP_DELAY_MS: u32 = 2000;

fn cleo_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.set_file_name("Documents");
    path.push("CLEO");
    path
}

fn runtime_log(message: &str) {
    let root = cleo_dir();
    let _ = fs::create_dir_all(&root);
    let path = root.join("jailed_runtime.log");

    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

fn collect_fxt_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            collect_fxt_files(&path, out);
            continue;
        }

        if path
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.eq_ignore_ascii_case("fxt"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

fn reload_fxt(root: &Path) {
    let mut files = Vec::new();
    collect_fxt_files(root, &mut files);
    files.sort_by_key(|p| p.display().to_string().to_lowercase());

    let mut map = HashMap::new();

    for path in files {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);

        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            let split_at = line
                .char_indices()
                .find(|(_, c)| c.is_whitespace())
                .map(|(i, _)| i);

            let Some(split_at) = split_at else {
                continue;
            };

            let key = line[..split_at].trim();
            let value = line[split_at..].trim();

            if !key.is_empty() && !value.is_empty() {
                // GXT keys are ASCII identifiers and game scripts do not
                // consistently preserve case across mobile mods.
                map.insert(key.to_ascii_uppercase(), value.to_string());
            }
        }
    }

    *FXT_MAP.lock().unwrap() = map;
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

    reload_fxt(&root);

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

    CSI_COUNT.store(
        scripts.iter().filter(|script| script.kind == Kind::Csi).count() as u32,
        Ordering::SeqCst,
    );
    *SCRIPTS.lock().unwrap() = scripts;
}

fn begin_game_session() {
    *WAYPOINT_SCAN_CACHE.lock().unwrap() = (u32::MAX, None);
    CSA_AUTOSTART_DONE.store(false, Ordering::SeqCst);

    let now = unsafe { read_global::<u32>(GAME_TIME_ADDR) };
    CSA_START_AFTER.store(now.saturating_add(CSA_STARTUP_DELAY_MS), Ordering::SeqCst);

    let mut scripts = SCRIPTS.lock().unwrap();

    // Never run arbitrary Android scripts on the exact frame GTA enters
    // gameplay. The 60 FPS adapter is different: it executes no Android
    // bytecode and only writes the verified iOS frame-cap variable, so it can
    // be active immediately and keep the cap pinned from the first game frame.
    for script in scripts.iter_mut() {
        let active_now =
            script.enabled && script.special == SpecialScript::Fps60;
        script.reset(active_now);
    }
}

fn end_game_session() {
    *WAYPOINT_SCAN_CACHE.lock().unwrap() = (u32::MAX, None);
    crate::jailed::hide_script_text_overlay();
    CSA_AUTOSTART_DONE.store(false, Ordering::SeqCst);
    CSA_START_AFTER.store(0, Ordering::SeqCst);
    unsafe {
        let cap = absolute(FPS_CAP_ADDR);
        if segment_info(cap).map(|(_, writable, _)| writable).unwrap_or(false) {
            (cap as *mut u32).write(30);
        }
    }
    let mut scripts = SCRIPTS.lock().unwrap();
    for script in scripts.iter_mut() {
        script.game.active = false;
    }
    crate::jailed::hide_android_menu();
    crate::jailed_touch::reset();
    crate::jailed_cheats::clear_queue();
}

pub fn tick() {
    if !INITIALISED.load(Ordering::SeqCst) {
        return;
    }

    crate::jailed::begin_script_text_tick();

    let game_state = unsafe { read_global::<u32>(GAME_STATE_ADDR) };
    let now_in_game = game_state == 9;
    let was_in_game = IN_GAME.swap(now_in_game, Ordering::SeqCst);

    if now_in_game && !was_in_game {
        begin_game_session();
    } else if !now_in_game && was_in_game {
        end_game_session();
    }

    if !now_in_game {
        crate::jailed::end_script_text_tick();
        return;
    }

    // Original CLEO iOS defaults to a 60 FPS cap. Enforce the same verified
    // iOS game variable every jailed runtime tick so GTA cannot restore 30 FPS
    // after loading, pausing, opening a menu, or executing an Android script.
    let _ = enforce_60_fps_cap();

    // Execute built-in cheats only from the jailed game-runtime tick, after the
    // UIKit overlay has closed. This keeps weapon/vehicle cheats on the main
    // game thread instead of firing directly from a button callback.
    crate::jailed_cheats::process_queue();

    let mut scripts = SCRIPTS.lock().unwrap();

    if !CSA_AUTOSTART_DONE.load(Ordering::SeqCst) {
        let game_time = unsafe { read_global::<u32>(GAME_TIME_ADDR) };
        let start_after = CSA_START_AFTER.load(Ordering::SeqCst);

        if start_after != 0 && game_time >= start_after {
            for script in scripts
                .iter_mut()
                .filter(|s| s.kind == Kind::Csa && s.enabled)
            {
                if !script.game.active {
                    script.reset(true);
                }
            }

            CSA_AUTOSTART_DONE.store(true, Ordering::SeqCst);
        }
    }

    for script in scripts.iter_mut() {
        if script.kind == Kind::Csa && !script.enabled {
            continue;
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            script.update();
        }));

        if result.is_err() {
            script.stop_with_error(
                "Script runtime panic was contained; script quarantined instead of crashing the app"
                    .to_string(),
            );
        }
    }

    let pending = {
        let mut queue = PENDING_INVOKES.lock().unwrap();
        std::mem::take(&mut *queue)
    };

    for id in pending {
        if let Some(script) = scripts
            .iter_mut()
            .filter(|script| script.kind == Kind::Csi)
            .nth(id)
        {
            if !script.game.active {
                script.reset(true);
            }
        }
    }

    drop(scripts);
    crate::jailed::end_script_text_tick();
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

    // Never restart a CSI while its previous invocation is still alive.
    // Resetting GameScript in the middle of VehicleSpawn/other menu scripts can
    // invalidate their model/menu state and is a direct crash risk.
    if script.game.active {
        return false;
    }

    // A normal CSI termination (004E) already resets IP/locals to the start.
    // Match original CLEO iOS semantics and simply reactivate it. Only perform
    // a full reset when retrying a script that previously stopped with an error.
    if script.error.is_some() {
        crate::jailed::hide_android_menu();
        script.reset(true);
    } else {
        script.game.active = true;
    }

    runtime_log(&format!("[CSI] activated {}", script.name));
    true
}

pub fn toggle_csa(index: usize) -> bool {
    let in_game = IN_GAME.load(Ordering::SeqCst);
    let mut scripts = SCRIPTS.lock().unwrap();
    let Some(script) = scripts.iter_mut().filter(|s| s.kind == Kind::Csa).nth(index) else {
        return false;
    };

    script.enabled = !script.enabled;
    runtime_log(&format!(
        "[CSA] {} -> {}",
        script.name,
        if script.enabled { "enabled" } else { "disabled" }
    ));

    if script.enabled && in_game {
        script.reset(true);
        if script.special == SpecialScript::Fps60 {
            unsafe {
                (absolute(FPS_CAP_ADDR) as *mut u32).write(60);
            }
        }
    } else if !script.enabled {
        script.game.active = false;
        if script.special == SpecialScript::Fps60 {
            unsafe {
                (absolute(FPS_CAP_ADDR) as *mut u32).write(30);
            }
        }
    }

    true
}

pub fn is_in_game() -> bool {
    IN_GAME.load(Ordering::SeqCst)
}

pub fn init() {
    reload_scripts();
    runtime_log(&format!(
        "[INIT] game_image={} slide={:#x} scripts={} csi={}",
        game_image_index(),
        game_slide(),
        SCRIPTS.lock().unwrap().len(),
        CSI_COUNT.load(Ordering::SeqCst),
    ));
    INITIALISED.store(true, Ordering::SeqCst);
}
