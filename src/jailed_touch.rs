//! Touch compatibility for CLEO Android opcodes in the jailed iOS build.
//!
//! CLEO Android keeps two independent finger histories and exposes a 3x3
//! touch-zone grid to scripts. Mirror that behaviour instead of collapsing
//! every gesture to one finger.

use once_cell::sync::Lazy;
use std::{
    sync::Mutex,
    time::Instant,
};

const TOUCH_EXIST_MS: u64 = 150;
const MENU_BUTTON_PULSE_MS: u64 = 250;
const MAX_FINGERS: usize = 2;

#[derive(Debug, Clone, Copy, Default)]
struct TouchSample {
    zone: u32,
    time_ms: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct FingerState {
    down: TouchSample,
    up: TouchSample,
    current_zone: u32,
    active: bool,
}

#[derive(Debug, Default)]
struct State {
    fingers: [FingerState; MAX_FINGERS],
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

fn mark_zone(state: &mut State, zone: u32, now: u64) {
    if (1..=9).contains(&zone) {
        state.touch_points[zone as usize] = now;
    }
}

/// Update the complete set of currently active touches. UIKit may reorder touch
/// indices, so keep at most the two fingers supported by CLEO Android and treat
/// a newly appearing slot as a fresh TOUCH_DOWN.
pub fn touches_changed(points: &[(f64, f64)], width: f64, height: f64) {
    let now = now_ms();
    let mut state = STATE.lock().unwrap();

    for index in 0..MAX_FINGERS {
        if let Some(&(x, y)) = points.get(index) {
            let zone = zone_for_point(x, y, width, height);
            let was_active = state.fingers[index].active;

            if !was_active {
                state.fingers[index].down = TouchSample { zone, time_ms: now };
            }

            state.fingers[index].current_zone = zone;
            state.fingers[index].active = true;
            mark_zone(&mut state, zone, now);
        } else if state.fingers[index].active {
            let zone = state.fingers[index].current_zone;
            state.fingers[index].up = TouchSample { zone, time_ms: now };
            state.fingers[index].active = false;
            mark_zone(&mut state, zone, now);
        }
    }
}

/// Finish every tracked touch when UIKit reports ended/cancelled/failed.
pub fn touches_ended() {
    let now = now_ms();
    let mut state = STATE.lock().unwrap();

    for index in 0..MAX_FINGERS {
        if state.fingers[index].active {
            let zone = state.fingers[index].current_zone;
            state.fingers[index].up = TouchSample { zone, time_ms: now };
            state.fingers[index].active = false;
            mark_zone(&mut state, zone, now);
        }
    }
}

pub fn zone_pressed(zone: u32) -> bool {
    STATE
        .lock()
        .unwrap()
        .fingers
        .iter()
        .any(|finger| finger.active && finger.current_zone == zone)
}

pub fn point_touched_timed(zone: u32, min_time_ms: u32) -> bool {
    let now = now_ms();
    STATE
        .lock()
        .unwrap()
        .fingers
        .iter()
        .any(|finger| {
            finger.active
                && finger.down.zone == zone
                && finger.down.time_ms.saturating_add(min_time_ms as u64) <= now
        })
}

pub fn point_touched_recent(zone: u32) -> bool {
    let now = now_ms();
    let state = STATE.lock().unwrap();

    (1..=9).contains(&zone)
        && state.touch_points[zone as usize].saturating_add(TOUCH_EXIST_MS) > now
}

pub fn slide_done(from: u32, to: u32, min_time_ms: u32, max_time_ms: u32) -> bool {
    let now = now_ms();
    STATE
        .lock()
        .unwrap()
        .fingers
        .iter()
        .any(|finger| {
            if finger.down.zone != from
                || finger.up.zone != to
                || finger.up.time_ms <= finger.down.time_ms
                || finger.up.time_ms.saturating_add(TOUCH_EXIST_MS) <= now
            {
                return false;
            }

            let elapsed = finger.up.time_ms - finger.down.time_ms;
            elapsed >= min_time_ms as u64 && elapsed <= max_time_ms as u64
        })
}

pub fn pulse_menu_button() {
    let mut state = STATE.lock().unwrap();
    state.menu_button_down = true;
    state.menu_button_time_ms = now_ms();
}

pub fn menu_button_state() -> bool {
    let now = now_ms();
    let mut state = STATE.lock().unwrap();

    if state.menu_button_down
        && state.menu_button_time_ms.saturating_add(MENU_BUTTON_PULSE_MS) <= now
    {
        state.menu_button_down = false;
    }

    state.menu_button_down
}

pub fn menu_button_pressed_timed(min_time_ms: u32) -> bool {
    let now = now_ms();
    let mut state = STATE.lock().unwrap();

    if state.menu_button_down
        && state.menu_button_time_ms.saturating_add(MENU_BUTTON_PULSE_MS) <= now
    {
        state.menu_button_down = false;
    }

    state.menu_button_down
        && state
            .menu_button_time_ms
            .saturating_add(min_time_ms as u64)
            <= now
}

pub fn reset() {
    *STATE.lock().unwrap() = State::default();
}
