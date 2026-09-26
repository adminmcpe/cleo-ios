//! Touch compatibility for CLEO Android opcodes in the jailed iOS build.

use once_cell::sync::Lazy;
use std::{
    sync::Mutex,
    time::Instant,
};

const TOUCH_EXIST_MS: u64 = 150;
const MENU_BUTTON_PULSE_MS: u64 = 250;

#[derive(Debug, Clone, Copy, Default)]
struct TouchSample {
    zone: u32,
    time_ms: u64,
}

#[derive(Debug, Default)]
struct State {
    down: TouchSample,
    up: TouchSample,
    current_zone: u32,
    is_down: bool,
    touch_points: [u64; 10],
    menu_button_down: bool,
    menu_button_time_ms: u64,
}

static START: Lazy<Instant> = Lazy::new(Instant::now);
static STATE: Lazy<Mutex<State>> = Lazy::new(|| Mutex::new(State::default()));

pub fn now_ms() -> u64 {
    START.elapsed().as_millis() as u64
}

fn zone_for_point(x: f64, y: f64, width: f64, height: f64) -> u32 {
    if width <= 0.0 || height <= 0.0 {
        return 0;
    }

    let xn = (x / width).clamp(0.0, 0.999_999);
    let yn = (y / height).clamp(0.0, 0.999_999);
    let col = (xn * 3.0).floor() as u32;
    let row = (yn * 3.0).floor() as u32;

    // CLEO Android numbering is column-major:
    // 1 4 7
    // 2 5 8
    // 3 6 9
    col * 3 + row + 1
}

pub fn touch_began(x: f64, y: f64, width: f64, height: f64) {
    let zone = zone_for_point(x, y, width, height);
    let now = now_ms();
    let mut state = STATE.lock().unwrap();

    state.down = TouchSample { zone, time_ms: now };
    state.current_zone = zone;
    state.is_down = true;
    if (1..=9).contains(&zone) {
        state.touch_points[zone as usize] = now;
    }
}

pub fn touch_moved(x: f64, y: f64, width: f64, height: f64) {
    let zone = zone_for_point(x, y, width, height);
    let now = now_ms();
    let mut state = STATE.lock().unwrap();

    if state.is_down {
        state.current_zone = zone;
        if (1..=9).contains(&zone) {
            state.touch_points[zone as usize] = now;
        }
    }
}

pub fn touch_ended(x: f64, y: f64, width: f64, height: f64) {
    let zone = zone_for_point(x, y, width, height);
    let now = now_ms();
    let mut state = STATE.lock().unwrap();

    state.up = TouchSample { zone, time_ms: now };
    state.current_zone = zone;
    state.is_down = false;
    if (1..=9).contains(&zone) {
        state.touch_points[zone as usize] = now;
    }
}

pub fn zone_pressed(zone: u32) -> bool {
    let state = STATE.lock().unwrap();
    state.is_down && state.current_zone == zone
}

pub fn point_touched_timed(zone: u32, min_time_ms: u32) -> bool {
    let now = now_ms();
    let state = STATE.lock().unwrap();

    state.is_down
        && state.down.zone == zone
        && state.down.time_ms + min_time_ms as u64 <= now
}

pub fn point_touched_recent(zone: u32) -> bool {
    let now = now_ms();
    let state = STATE.lock().unwrap();

    (1..=9).contains(&zone)
        && state.touch_points[zone as usize] + TOUCH_EXIST_MS > now
}

pub fn slide_done(from: u32, to: u32, min_time_ms: u32, max_time_ms: u32) -> bool {
    let now = now_ms();
    let state = STATE.lock().unwrap();

    if state.down.zone != from
        || state.up.zone != to
        || state.up.time_ms <= state.down.time_ms
        || state.up.time_ms + TOUCH_EXIST_MS <= now
    {
        return false;
    }

    let elapsed = state.up.time_ms - state.down.time_ms;
    elapsed >= min_time_ms as u64 && elapsed <= max_time_ms as u64
}

pub fn pulse_menu_button() {
    let mut state = STATE.lock().unwrap();
    state.menu_button_down = true;
    state.menu_button_time_ms = now_ms();
}

pub fn menu_button_state() -> bool {
    let now = now_ms();
    let mut state = STATE.lock().unwrap();

    if state.menu_button_down && state.menu_button_time_ms + MENU_BUTTON_PULSE_MS <= now {
        state.menu_button_down = false;
    }

    state.menu_button_down
}

pub fn menu_button_pressed_timed(min_time_ms: u32) -> bool {
    let now = now_ms();
    let mut state = STATE.lock().unwrap();

    if state.menu_button_down && state.menu_button_time_ms + MENU_BUTTON_PULSE_MS <= now {
        state.menu_button_down = false;
    }

    state.menu_button_down && state.menu_button_time_ms + min_time_ms as u64 <= now
}

pub fn reset() {
    *STATE.lock().unwrap() = State::default();
}
