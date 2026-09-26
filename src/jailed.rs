//! Jailbreak-free CLEO menu proof-of-concept.
//!
//! This build intentionally avoids executable-memory/game-code hooks. It recreates
//! CLEO's UIKit menu shell and connects it to the jailed CSI/CSA runtime.

use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{self, Object, Sel},
    sel,
};
use once_cell::sync::{Lazy, OnceCell};
use std::{
    ffi::CString,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
};

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

impl CGRect {
    fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            origin: CGPoint { x, y },
            size: CGSize { width, height },
        }
    }
}

const TAG_CLOSE: i64 = 1;
const TAG_TAB_CSI: i64 = 10;
const TAG_TAB_CSA: i64 = 11;
const TAG_TAB_CHEATS: i64 = 12;
const TAG_TAB_OPTIONS: i64 = 13;
const TAG_CSI_BASE: i64 = 1000;
const TAG_CSA_BASE: i64 = 2000;
const TAG_CHEAT_BASE: i64 = 3000;
const TAG_ANDROID_ITEM_BASE: i64 = 10_000;
const TAG_ANDROID_CLOSE: i64 = 19_999;

static GESTURE_TARGET: OnceCell<usize> = OnceCell::new();
static OVERLAY: AtomicUsize = AtomicUsize::new(0);
static CONTENT_VIEW: AtomicUsize = AtomicUsize::new(0);
static TAB_BUTTONS: Lazy<Mutex<[usize; 4]>> = Lazy::new(|| Mutex::new([0; 4]));
static SELECTED_TAB: AtomicUsize = AtomicUsize::new(TAG_TAB_CSI as usize);
static TIMER_INSTALLED: AtomicBool = AtomicBool::new(false);
static ANDROID_MENU_OVERLAY: AtomicUsize = AtomicUsize::new(0);
static SCRIPT_TEXT_OVERLAY: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptTextDraw {
    pub x: f32,
    pub y: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub rgba: [u8; 4],
    pub centered: bool,
    pub right_aligned: bool,
    pub outline: bool,
    pub text: String,
}

static LAST_SCRIPT_TEXT_FRAME: Lazy<Mutex<Vec<ScriptTextDraw>>> =
    Lazy::new(|| Mutex::new(Vec::new()));


#[derive(Debug)]
struct AndroidMenuState {
    selected: i32,
    selected_time_ms: u64,
    active_item: i32,
}

impl Default for AndroidMenuState {
    fn default() -> Self {
        Self {
            selected: -1,
            selected_time_ms: 0,
            active_item: 0,
        }
    }
}

static ANDROID_MENU_STATE: Lazy<Mutex<AndroidMenuState>> =
    Lazy::new(|| Mutex::new(AndroidMenuState::default()));

fn ns_string(value: &str) -> *const Object {
    unsafe {
        let value = CString::new(value).unwrap_or_else(|_| CString::new("?").unwrap());
        msg_send![class!(NSString), stringWithUTF8String: value.as_ptr()]
    }
}

fn set_bg(view: *mut Object, white: f64, alpha: f64) {
    unsafe {
        let colour: *mut Object =
            msg_send![class!(UIColor), colorWithWhite: white alpha: alpha];
        let _: () = msg_send![view, setBackgroundColor: colour];
    }
}

fn clean_android_menu_text(value: &str) -> String {
    // Android CLEO/GTA font strings contain formatting tokens such as ~h~,
    // ~y~, ~g~, ~r~, ~s~ and substitution markers such as ~1~. UIKit does
    // not interpret them, so strip only short tilde-delimited control tokens.
    // Malformed/ordinary tildes are preserved.
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] == '~' {
            let max_end = (i + 13).min(chars.len());
            if let Some(end) = ((i + 1)..max_end).find(|&j| chars[j] == '~') {
                i = end + 1;
                continue;
            }
        }

        out.push(chars[i]);
        i += 1;
    }

    out.trim().to_string()
}


fn clear_view_subviews(view: *mut Object) {
    if view.is_null() {
        return;
    }

    unsafe {
        let subviews: *mut Object = msg_send![view, subviews];
        let copied: *mut Object = msg_send![subviews, copy];
        let count: usize = msg_send![copied, count];

        for index in 0..count {
            let child: *mut Object = msg_send![copied, objectAtIndex: index];
            let _: () = msg_send![child, removeFromSuperview];
        }

        let _: () = msg_send![copied, release];
    }
}

pub fn hide_script_text_overlay() {
    LAST_SCRIPT_TEXT_FRAME.lock().unwrap().clear();

    let current = SCRIPT_TEXT_OVERLAY.swap(0, Ordering::SeqCst);
    if current == 0 {
        return;
    }

    unsafe {
        let view = current as *mut Object;
        let _: () = msg_send![view, removeFromSuperview];
        let _: () = msg_send![view, release];
    }
}

pub fn render_script_text_frame(frame: Vec<ScriptTextDraw>) {
    // The normal CLEO menu should always stay above script-owned HUD/menu text.
    if OVERLAY.load(Ordering::SeqCst) != 0 || ANDROID_MENU_OVERLAY.load(Ordering::SeqCst) != 0 {
        return;
    }

    {
        let mut last = LAST_SCRIPT_TEXT_FRAME.lock().unwrap();
        if *last == frame {
            return;
        }
        *last = frame.clone();
    }

    if frame.is_empty() {
        hide_script_text_overlay();
        return;
    }

    unsafe {
        let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
        let window: *mut Object = msg_send![app, keyWindow];
        if window.is_null() {
            return;
        }

        let bounds: CGRect = msg_send![window, bounds];
        let overlay = {
            let existing = SCRIPT_TEXT_OVERLAY.load(Ordering::SeqCst) as *mut Object;
            if existing.is_null() {
                let view: *mut Object = msg_send![class!(UIView), alloc];
                let view: *mut Object = msg_send![view, initWithFrame: bounds];
                let _: () = msg_send![view, setBackgroundColor: std::ptr::null_mut::<Object>()];
                let _: () = msg_send![view, setUserInteractionEnabled: false];
                let _: () = msg_send![window, addSubview: view];
                SCRIPT_TEXT_OVERLAY.store(view as usize, Ordering::SeqCst);
                view
            } else {
                let _: () = msg_send![existing, setFrame: bounds];
                existing
            }
        };

        clear_view_subviews(overlay);

        // GTA:SA's script text/box coordinates use a 640x448 virtual canvas.
        let sx = bounds.size.width / 640.0;
        let sy = bounds.size.height / 448.0;

        for item in frame {
            let clean = clean_android_menu_text(&item.text);
            if clean.is_empty() {
                continue;
            }

            let mut alignment = 0i64; // left
            let (virtual_x, virtual_width) = if item.right_aligned {
                alignment = 2;
                (item.x as f64 - 260.0, 260.0)
            } else if item.centered && item.x >= 90.0 {
                alignment = 1;
                (item.x as f64 - 220.0, 440.0)
            } else {
                (item.x as f64, 360.0)
            };

            let font_size = (12.0 * item.scale_y.max(0.65) as f64).clamp(9.0, 30.0);
            let x = virtual_x * sx;
            let y = (item.y as f64 * sy) - font_size * 0.60;
            let width = (virtual_width * sx).max(44.0);
            let height = (font_size * 1.45).max(18.0);

            let label: *mut Object = msg_send![class!(UILabel), alloc];
            let label: *mut Object =
                msg_send![label, initWithFrame: CGRect::new(x, y, width, height)];
            let _: () = msg_send![label, setText: ns_string(&clean)];
            let _: () = msg_send![label, setTextAlignment: alignment];
            let _: () = msg_send![label, setNumberOfLines: 1i64];

            let colour: *mut Object = msg_send![
                class!(UIColor),
                colorWithRed: item.rgba[0] as f64 / 255.0
                green: item.rgba[1] as f64 / 255.0
                blue: item.rgba[2] as f64 / 255.0
                alpha: item.rgba[3] as f64 / 255.0
            ];
            let _: () = msg_send![label, setTextColor: colour];

            let font: *mut Object =
                msg_send![class!(UIFont), boldSystemFontOfSize: font_size];
            let _: () = msg_send![label, setFont: font];

            if item.outline {
                let shadow: *mut Object =
                    msg_send![class!(UIColor), colorWithWhite: 0.0f64 alpha: 1.0f64];
                let _: () = msg_send![label, setShadowColor: shadow];
                let _: () = msg_send![label, setShadowOffset: CGSize { width: 1.0, height: 1.0 }];
            }

            let _: () = msg_send![overlay, addSubview: label];
            let _: () = msg_send![label, release];
        }
    }
}

const IOS_CLEO_ROW_HEIGHT: f64 = 50.0;
const IOS_CLEO_TAB_HEIGHT: f64 = 50.0;
const IOS_CLEO_CLOSE_HEIGHT: f64 = 35.0;

fn create_blur_view(frame: CGRect) -> *mut Object {
    unsafe {
        // CLEO iOS 2.6 uses UIBlurEffectStyle 3 (extra dark).
        let effect: *mut Object = msg_send![class!(UIBlurEffect), effectWithStyle: 3u64];
        let view: *mut Object = msg_send![class!(UIVisualEffectView), alloc];
        let view: *mut Object = msg_send![view, initWithEffect: effect];
        let _: () = msg_send![view, setFrame: frame];
        view
    }
}

fn set_tab_style(button: *mut Object, selected: bool) {
    if button.is_null() {
        return;
    }

    unsafe {
        let text_alpha = if selected { 0.95 } else { 0.40 };
        let background_alpha = if selected { 0.20 } else { 0.10 };

        let title_colour: *mut Object =
            msg_send![class!(UIColor), colorWithWhite: 1.0f64 alpha: text_alpha];
        let background: *mut Object =
            msg_send![class!(UIColor), colorWithWhite: 0.0f64 alpha: background_alpha];

        let _: () = msg_send![button, setTitleColor: title_colour forState: 0u64];
        let _: () = msg_send![button, setBackgroundColor: background];
    }
}

fn refresh_tab_styles() {
    let selected = SELECTED_TAB.load(Ordering::SeqCst) as i64;
    let tags = [TAG_TAB_CSI, TAG_TAB_CSA, TAG_TAB_CHEATS, TAG_TAB_OPTIONS];
    let buttons = TAB_BUTTONS.lock().unwrap();

    for (index, button) in buttons.iter().enumerate() {
        set_tab_style(*button as *mut Object, selected == tags[index]);
    }
}

fn add_label(
    parent: *mut Object,
    frame: CGRect,
    text: &str,
    size: f64,
    alignment: i64,
    alpha: f64,
) -> *mut Object {
    unsafe {
        let label: *mut Object = msg_send![class!(UILabel), alloc];
        let label: *mut Object = msg_send![label, initWithFrame: frame];

        let _: () = msg_send![label, setText: ns_string(text)];
        let _: () = msg_send![label, setNumberOfLines: 0i64];
        let _: () = msg_send![label, setTextAlignment: alignment];

        let colour: *mut Object =
            msg_send![class!(UIColor), colorWithWhite: 1.0f64 alpha: alpha];
        let _: () = msg_send![label, setTextColor: colour];

        let font: *mut Object = msg_send![class!(UIFont), systemFontOfSize: size];
        let _: () = msg_send![label, setFont: font];

        let _: () = msg_send![parent, addSubview: label];
        let _: () = msg_send![label, release];

        label
    }
}

fn add_row(parent: *mut Object, y: f64, width: f64, title: &str, detail: &str, value: &str) {
    unsafe {
        let row = CGRect::new(0.0, y, width, IOS_CLEO_ROW_HEIGHT);

        let container: *mut Object = msg_send![class!(UIView), alloc];
        let container: *mut Object = msg_send![container, initWithFrame: row];
        set_bg(container, 0.0, 0.0);

        add_label(
            container,
            CGRect::new(width * 0.05, 2.0, width * 0.58, 27.0),
            title,
            16.0,
            0,
            0.95,
        );
        add_label(
            container,
            CGRect::new(width * 0.05, 25.0, width * 0.72, 20.0),
            detail,
            11.0,
            0,
            0.95,
        );
        add_label(
            container,
            CGRect::new(width * 0.63, 2.0, width * 0.32, 27.0),
            value,
            16.0,
            2,
            0.95,
        );

        let _: () = msg_send![parent, addSubview: container];
        let _: () = msg_send![container, release];
    }
}

fn add_action_row(
    parent: *mut Object,
    target: *mut Object,
    y: f64,
    width: f64,
    title: &str,
    detail: &str,
    value: &str,
    tag: i64,
) {
    unsafe {
        let row = CGRect::new(0.0, y, width, IOS_CLEO_ROW_HEIGHT);

        let button: *mut Object = msg_send![class!(UIButton), alloc];
        let button: *mut Object = msg_send![button, initWithFrame: row];
        let _: () = msg_send![button, setTag: tag];
        set_bg(button, 0.0, 0.0);

        add_label(
            button,
            CGRect::new(width * 0.05, 2.0, width * 0.58, 27.0),
            title,
            16.0,
            0,
            0.95,
        );
        add_label(
            button,
            CGRect::new(width * 0.05, 25.0, width * 0.72, 20.0),
            detail,
            11.0,
            0,
            0.95,
        );
        add_label(
            button,
            CGRect::new(width * 0.63, 2.0, width * 0.32, 27.0),
            value,
            16.0,
            2,
            0.95,
        );

        let _: () = msg_send![
            button,
            addTarget: target
            action: sel!(handleCleoMenuButton:)
            forControlEvents: 1u64 << 6
        ];

        let _: () = msg_send![parent, addSubview: button];
        let _: () = msg_send![button, release];
    }
}

fn set_scroll_height(parent: *mut Object, width: f64, height: f64) {
    unsafe {
        let _: () = msg_send![parent, setContentSize: CGSize { width, height }];
    }
}

fn add_script_rows(parent: *mut Object, width: f64, extension: &str) {
    let is_csa = extension.eq_ignore_ascii_case("csa");
    let scripts = if is_csa {
        crate::jailed_runtime::csa_statuses()
    } else {
        crate::jailed_runtime::csi_statuses()
    };

    if scripts.is_empty() {
        add_label(
            parent,
            CGRect::new(width * 0.06, 20.0, width * 0.88, 100.0),
            &format!("No .{} scripts found in Documents/CLEO.", extension),
            17.0,
            1,
            0.72,
        );
        return;
    }

    let target = GESTURE_TARGET
        .get()
        .copied()
        .unwrap_or(0) as *mut Object;

    let row_count = scripts.len();
    let mut y = 0.0;
    for (index, script) in scripts.into_iter().enumerate() {
        let (detail, value) = if let Some(error) = &script.error {
            (error.as_str(), "Error")
        } else if is_csa {
            (
                "Startup script - tap to enable/disable",
                if script.enabled { "Enabled" } else { "Disabled" },
            )
        } else {
            (
                "Invoked script - tap to run",
                if script.active { "Running" } else { "Not running" },
            )
        };

        let tag = if is_csa {
            TAG_CSA_BASE + index as i64
        } else {
            TAG_CSI_BASE + index as i64
        };

        add_action_row(
            parent,
            target,
            y,
            width,
            &script.name,
            detail,
            value,
            tag,
        );
        y += IOS_CLEO_ROW_HEIGHT;
    }

    set_scroll_height(parent, width, (row_count as f64 * IOS_CLEO_ROW_HEIGHT).max(IOS_CLEO_ROW_HEIGHT));
}

fn add_cheat_rows(parent: *mut Object, width: f64) {
    let cheats = crate::jailed_cheats::statuses();

    if cheats.is_empty() {
        add_label(
            parent,
            CGRect::new(width * 0.06, 20.0, width * 0.88, 100.0),
            "No named cheats found.",
            17.0,
            1,
            0.72,
        );
        return;
    }

    let target = GESTURE_TARGET
        .get()
        .copied()
        .unwrap_or(0) as *mut Object;

    let row_count = cheats.len();
    let mut y = 0.0;

    for (index, cheat) in cheats.into_iter().enumerate() {
        let value = if cheat.queued {
            if cheat.will_be_active { "Queued On" } else { "Queued Off" }
        } else if cheat.active {
            "On"
        } else {
            "Off"
        };

        add_action_row(
            parent,
            target,
            y,
            width,
            cheat.code,
            "Built-in GTA:SA cheat - tap to queue",
            value,
            TAG_CHEAT_BASE + index as i64,
        );

        y += IOS_CLEO_ROW_HEIGHT;
    }

    set_scroll_height(parent, width, (row_count as f64 * IOS_CLEO_ROW_HEIGHT).max(IOS_CLEO_ROW_HEIGHT));
}

fn clear_content() {
    let content = CONTENT_VIEW.load(Ordering::SeqCst) as *mut Object;
    if content.is_null() {
        return;
    }

    unsafe {
        let subviews: *mut Object = msg_send![content, subviews];
        let copied: *mut Object = msg_send![subviews, copy];
        let count: usize = msg_send![copied, count];

        for i in 0..count {
            let view: *mut Object = msg_send![copied, objectAtIndex: i];
            let _: () = msg_send![view, removeFromSuperview];
        }

        let _: () = msg_send![copied, release];
    }
}

fn render_selected_tab() {
    let content = CONTENT_VIEW.load(Ordering::SeqCst) as *mut Object;
    if content.is_null() {
        return;
    }

    clear_content();
    refresh_tab_styles();

    unsafe {
        let bounds: CGRect = msg_send![content, bounds];
        let width = bounds.size.width;
        let _: () = msg_send![content, setContentOffset: CGPoint { x: 0.0, y: 0.0 } animated: false];
        set_scroll_height(content, width, bounds.size.height);

        match SELECTED_TAB.load(Ordering::SeqCst) as i64 {
            TAG_TAB_CSI => add_script_rows(content, width, "csi"),
            TAG_TAB_CSA => add_script_rows(content, width, "csa"),
            TAG_TAB_CHEATS => add_cheat_rows(content, width),
            _ => {
                add_row(
                    content,
                    0.0,
                    width,
                    "Menu Gesture",
                    "Gesture used to open the CLEO menu",
                    "Swipe Down",
                );
                add_row(
                    content,
                    IOS_CLEO_ROW_HEIGHT,
                    width,
                    "Runtime Mode",
                    "UIKit timer + native SCM opcode handlers",
                    if crate::jailed_runtime::is_in_game() { "Jailed / In Game" } else { "Jailed / Waiting" },
                );
                add_row(
                    content,
                    IOS_CLEO_ROW_HEIGHT * 2.0,
                    width,
                    "CLEO Base",
                    "Source branch used for this port",
                    "2.6.0",
                );
                set_scroll_height(content, width, IOS_CLEO_ROW_HEIGHT * 3.0);
            }
        }
    }
}

extern "C" fn handle_cleo_swipe(_this: &Object, _cmd: Sel, _gesture: *mut Object) {
    crate::jailed_touch::pulse_menu_button();

    // Android CLEO scripts use a menu/back-button press as a cancel action.
    // Do not open our own CLEO menu over a script-owned Android compatibility menu.
    if ANDROID_MENU_OVERLAY.load(Ordering::SeqCst) != 0 {
        return;
    }

    // The global swipe recognizer is also active while the CLEO UIScrollView is
    // visible. Treat swipe-down as an *open-only* gesture so dragging the script
    // list downward can never dismiss the menu. The dedicated Close button is
    // the only way to close while the overlay is visible.
    if OVERLAY.load(Ordering::SeqCst) != 0 {
        return;
    }

    show_menu();
}

extern "C" fn handle_cleo_touch(_this: &Object, _cmd: Sel, gesture: *mut Object) {
    // Ignore gameplay touch-zone reporting while either CLEO UI is covering the game.
    if OVERLAY.load(Ordering::SeqCst) != 0
        || ANDROID_MENU_OVERLAY.load(Ordering::SeqCst) != 0
    {
        return;
    }

    unsafe {
        let state: i64 = msg_send![gesture, state];
        let view: *mut Object = msg_send![gesture, view];
        if view.is_null() {
            return;
        }

        let bounds: CGRect = msg_send![view, bounds];

        match state {
            1 | 2 => {
                let count: usize = msg_send![gesture, numberOfTouches];
                let mut points = Vec::with_capacity(count.min(2));

                for index in 0..count.min(2) {
                    let point: CGPoint =
                        msg_send![gesture, locationOfTouch: index inView: view];
                    points.push((point.x, point.y));
                }

                crate::jailed_touch::touches_changed(
                    &points,
                    bounds.size.width,
                    bounds.size.height,
                );
            }
            3 | 4 | 5 => crate::jailed_touch::touches_ended(),
            _ => {}
        }
    }
}

extern "C" fn allow_simultaneous_gestures(
    _this: &Object,
    _cmd: Sel,
    _first: *mut Object,
    _second: *mut Object,
) -> runtime::BOOL {
    runtime::YES
}

extern "C" fn runtime_timer_tick(_this: &Object, _cmd: Sel, _timer: *mut Object) {
    crate::jailed_runtime::tick();
}

extern "C" fn handle_menu_button(_this: &Object, _cmd: Sel, button: *mut Object) {
    unsafe {
        let tag: i64 = msg_send![button, tag];

        if tag == TAG_CLOSE {
            hide_menu();
            return;
        }

        if (TAG_TAB_CSI..=TAG_TAB_OPTIONS).contains(&tag) {
            SELECTED_TAB.store(tag as usize, Ordering::SeqCst);
            render_selected_tab();
            return;
        }

        if (TAG_CSI_BASE..TAG_CSA_BASE).contains(&tag) {
            let index = (tag - TAG_CSI_BASE) as usize;
            if crate::jailed_runtime::activate_csi(index) {
                hide_menu();
            } else {
                render_selected_tab();
            }
            return;
        }

        if (TAG_CSA_BASE..TAG_CHEAT_BASE).contains(&tag) {
            let index = (tag - TAG_CSA_BASE) as usize;
            crate::jailed_runtime::toggle_csa(index);
            render_selected_tab();
            return;
        }

        if (TAG_CHEAT_BASE..(TAG_CHEAT_BASE + 1000)).contains(&tag) {
            let index = (tag - TAG_CHEAT_BASE) as usize;
            crate::jailed_cheats::toggle_queue(index);
            render_selected_tab();
            return;
        }

        if (TAG_ANDROID_ITEM_BASE..TAG_ANDROID_CLOSE).contains(&tag) {
            let index = (tag - TAG_ANDROID_ITEM_BASE) as i32;
            let mut state = ANDROID_MENU_STATE.lock().unwrap();
            state.selected = index;
            state.active_item = index;
            state.selected_time_ms = crate::jailed_touch::now_ms();
            return;
        }

        if tag == TAG_ANDROID_CLOSE {
            let mut state = ANDROID_MENU_STATE.lock().unwrap();
            state.selected = -2;
            state.selected_time_ms = crate::jailed_touch::now_ms();
        }
    }
}

fn target_class() -> &'static runtime::Class {
    static CLASS: OnceCell<&'static runtime::Class> = OnceCell::new();

    CLASS.get_or_init(|| {
        let mut decl =
            ClassDecl::new("CLEOJailedUITarget", class!(NSObject)).expect("class allocation failed");

        unsafe {
            decl.add_method(
                sel!(handleCleoSwipe:),
                handle_cleo_swipe as extern "C" fn(&Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(handleCleoTouch:),
                handle_cleo_touch as extern "C" fn(&Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(gestureRecognizer:shouldRecognizeSimultaneouslyWithGestureRecognizer:),
                allow_simultaneous_gestures
                    as extern "C" fn(&Object, Sel, *mut Object, *mut Object) -> runtime::BOOL,
            );
            decl.add_method(
                sel!(handleCleoMenuButton:),
                handle_menu_button as extern "C" fn(&Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(cleoRuntimeTick:),
                runtime_timer_tick as extern "C" fn(&Object, Sel, *mut Object),
            );
        }

        decl.register()
    })
}

fn install_swipe_gesture() {
    unsafe {
        let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
        let window: *mut Object = msg_send![app, keyWindow];

        if window.is_null() || GESTURE_TARGET.get().is_some() {
            return;
        }

        let target: *mut Object = msg_send![target_class(), new];

        let recognizer: *mut Object = msg_send![class!(UISwipeGestureRecognizer), alloc];
        let recognizer: *mut Object =
            msg_send![recognizer, initWithTarget: target action: sel!(handleCleoSwipe:)];

        let _: () = msg_send![recognizer, setDirection: 8usize];
        let _: () = msg_send![recognizer, setCancelsTouchesInView: false];
        let _: () = msg_send![recognizer, setDelegate: target];
        let _: () = msg_send![window, addGestureRecognizer: recognizer];
        let _: () = msg_send![recognizer, release];

        // UILongPressGestureRecognizer does NOT implement
        // setMinimumNumberOfTouches:/setMaximumNumberOfTouches:. Calling those
        // selectors aborts during LegalSplash viewDidLoad on iOS. Use two
        // recognizers with the valid numberOfTouchesRequired property instead:
        // one for normal one-finger CLEO input and one for Android-style
        // two-finger combinations.
        for required_touches in [1usize, 2usize] {
            let touch: *mut Object = msg_send![class!(UILongPressGestureRecognizer), alloc];
            let touch: *mut Object =
                msg_send![touch, initWithTarget: target action: sel!(handleCleoTouch:)];
            let _: () = msg_send![touch, setMinimumPressDuration: 0.0f64];
            let _: () = msg_send![touch, setAllowableMovement: 10000.0f64];
            let _: () = msg_send![touch, setNumberOfTouchesRequired: required_touches];
            let _: () = msg_send![touch, setCancelsTouchesInView: false];
            let _: () = msg_send![touch, setDelegate: target];
            let _: () = msg_send![window, addGestureRecognizer: touch];
            let _: () = msg_send![touch, release];
        }

        let _ = GESTURE_TARGET.set(target as usize);
    }
}

fn install_runtime_timer() {
    if TIMER_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    let Some(target) = GESTURE_TARGET.get().copied() else {
        TIMER_INSTALLED.store(false, Ordering::SeqCst);
        return;
    };

    unsafe {
        // Script text-draw opcodes are frame-scoped in GTA. Driving the jailed
        // runtime from CADisplayLink keeps RZL-style menus in sync with the
        // renderer instead of racing it from an independent NSTimer.
        let link: *mut Object = msg_send![
            class!(CADisplayLink),
            displayLinkWithTarget: target as *mut Object
            selector: sel!(cleoRuntimeTick:)
        ];
        let _: () = msg_send![link, setPreferredFramesPerSecond: 60i64];

        let run_loop: *mut Object = msg_send![class!(NSRunLoop), mainRunLoop];
        let mode = ns_string("NSRunLoopCommonModes");
        let _: () = msg_send![link, addToRunLoop: run_loop forMode: mode];
    }
}

fn add_tab_button(
    parent: *mut Object,
    target: *mut Object,
    frame: CGRect,
    title: &str,
    tag: i64,
) -> *mut Object {
    unsafe {
        let button: *mut Object = msg_send![class!(UIButton), alloc];
        let button: *mut Object = msg_send![button, initWithFrame: frame];
        let _: () = msg_send![button, setTitle: ns_string(title) forState: 0u64];
        let _: () = msg_send![button, setTag: tag];

        let label: *mut Object = msg_send![button, titleLabel];
        let font: *mut Object = msg_send![class!(UIFont), boldSystemFontOfSize: 16.0f64];
        let _: () = msg_send![label, setFont: font];
        let _: () = msg_send![label, setAdjustsFontSizeToFitWidth: true];

        set_tab_style(button, SELECTED_TAB.load(Ordering::SeqCst) as i64 == tag);

        let _: () = msg_send![
            button,
            addTarget: target
            action: sel!(handleCleoMenuButton:)
            forControlEvents: 1u64 << 6
        ];

        let _: () = msg_send![parent, addSubview: button];
        let _: () = msg_send![button, release];
        button
    }
}


fn add_android_menu_button(
    parent: *mut Object,
    target: *mut Object,
    frame: CGRect,
    title: &str,
    tag: i64,
) {
    unsafe {
        let button: *mut Object = msg_send![class!(UIButton), alloc];
        let button: *mut Object = msg_send![button, initWithFrame: frame];
        let _: () = msg_send![button, setTitle: ns_string("") forState: 0u64];
        let _: () = msg_send![button, setTag: tag];
        set_bg(button, 1.0, 0.10);

        // Render Android CLEO menu text with a UILabel instead of UIButton's
        // titleLabel. The latter can inherit a tint/configuration that makes
        // titles effectively invisible on modern iOS. UILabel rendering is the
        // same reliable path used by our main jailed CLEO menu.
        add_label(
            button,
            CGRect::new(8.0, 0.0, frame.size.width - 16.0, frame.size.height),
            title,
            17.0,
            1,
            0.96,
        );

        let _: () = msg_send![
            button,
            addTarget: target
            action: sel!(handleCleoMenuButton:)
            forControlEvents: 1u64 << 6
        ];

        let _: () = msg_send![parent, addSubview: button];
        let _: () = msg_send![button, release];
    }
}

pub fn show_android_menu(title: String, close_title: String, items: Vec<String>) {
    hide_android_menu();

    let title = clean_android_menu_text(&title);
    let close_title = clean_android_menu_text(&close_title);
    let items: Vec<String> = items
        .into_iter()
        .map(|item| clean_android_menu_text(&item))
        .collect();

    unsafe {
        let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
        let window: *mut Object = msg_send![app, keyWindow];
        if window.is_null() {
            return;
        }

        let bounds: CGRect = msg_send![window, bounds];
        let target = GESTURE_TARGET
            .get()
            .copied()
            .unwrap_or(0) as *mut Object;

        if target.is_null() {
            return;
        }

        let overlay: *mut Object = msg_send![class!(UIView), alloc];
        let overlay: *mut Object = msg_send![overlay, initWithFrame: bounds];
        set_bg(overlay, 0.0, 0.80);

        add_label(
            overlay,
            CGRect::new(bounds.size.width * 0.05, 16.0, bounds.size.width * 0.90, 50.0),
            &title,
            27.0,
            1,
            1.0,
        );

        let close_h = 58.0;
        let top = 72.0;
        let scroll_h = bounds.size.height - top - close_h;

        let scroll: *mut Object = msg_send![class!(UIScrollView), alloc];
        let scroll: *mut Object =
            msg_send![scroll, initWithFrame: CGRect::new(0.0, top, bounds.size.width, scroll_h)];
        let _: () = msg_send![scroll, setAlwaysBounceVertical: true];

        let mut y = 6.0;
        for (index, item) in items.iter().enumerate() {
            add_android_menu_button(
                scroll,
                target,
                CGRect::new(bounds.size.width * 0.06, y, bounds.size.width * 0.88, 54.0),
                item,
                TAG_ANDROID_ITEM_BASE + index as i64,
            );
            y += 60.0;
        }

        let _: () = msg_send![
            scroll,
            setContentSize: CGSize {
                width: bounds.size.width,
                height: y.max(scroll_h),
            }
        ];
        let _: () = msg_send![overlay, addSubview: scroll];
        let _: () = msg_send![scroll, release];

        let close: *mut Object = msg_send![class!(UIButton), alloc];
        let close: *mut Object = msg_send![
            close,
            initWithFrame: CGRect::new(
                0.0,
                bounds.size.height - close_h,
                bounds.size.width,
                close_h,
            )
        ];
        let _: () = msg_send![close, setTitle: ns_string("") forState: 0u64];
        let _: () = msg_send![close, setTag: TAG_ANDROID_CLOSE];
        let red: *mut Object =
            msg_send![class!(UIColor), colorWithRed: 1.0f64 green: 0.20f64 blue: 0.25f64 alpha: 0.36f64];
        let _: () = msg_send![close, setBackgroundColor: red];
        add_label(
            close,
            CGRect::new(8.0, 0.0, bounds.size.width - 16.0, close_h),
            &close_title,
            18.0,
            1,
            1.0,
        );
        let _: () = msg_send![
            close,
            addTarget: target
            action: sel!(handleCleoMenuButton:)
            forControlEvents: 1u64 << 6
        ];
        let _: () = msg_send![overlay, addSubview: close];
        let _: () = msg_send![close, release];

        let _: () = msg_send![window, addSubview: overlay];
        ANDROID_MENU_OVERLAY.store(overlay as usize, Ordering::SeqCst);

        let mut state = ANDROID_MENU_STATE.lock().unwrap();
        state.selected = -1;
        state.selected_time_ms = 0;
        if state.active_item < 0 || state.active_item as usize >= items.len() {
            state.active_item = 0;
        }
    }
}

pub fn hide_android_menu() {
    let current = ANDROID_MENU_OVERLAY.swap(0, Ordering::SeqCst);
    if current == 0 {
        return;
    }

    unsafe {
        let view = current as *mut Object;
        let _: () = msg_send![view, removeFromSuperview];
        let _: () = msg_send![view, release];
    }
}

pub fn android_menu_take_selected(max_time_ms: u32) -> i32 {
    let now = crate::jailed_touch::now_ms();
    let mut state = ANDROID_MENU_STATE.lock().unwrap();

    if state.selected_time_ms != 0
        && now <= state.selected_time_ms.saturating_add(max_time_ms as u64)
    {
        let selected = state.selected;
        state.selected_time_ms = 0;
        selected
    } else {
        -1
    }
}

pub fn android_menu_set_active(index: i32) {
    if index >= 0 {
        ANDROID_MENU_STATE.lock().unwrap().active_item = index;
    }
}

pub fn android_menu_get_active() -> i32 {
    ANDROID_MENU_STATE.lock().unwrap().active_item
}

fn show_menu() {
    if OVERLAY.load(Ordering::SeqCst) != 0 {
        return;
    }

    hide_script_text_overlay();

    unsafe {
        let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
        let window: *mut Object = msg_send![app, keyWindow];

        if window.is_null() {
            return;
        }

        let bounds: CGRect = msg_send![window, bounds];
        let blur_view = create_blur_view(bounds);
        let menu_parent: *mut Object = msg_send![blur_view, contentView];

        let target = GESTURE_TARGET
            .get()
            .copied()
            .unwrap_or_else(|| {
                let t: *mut Object = msg_send![target_class(), new];
                t as usize
            }) as *mut Object;

        let tab_w = bounds.size.width / 4.0;
        let tags = [TAG_TAB_CSI, TAG_TAB_CSA, TAG_TAB_CHEATS, TAG_TAB_OPTIONS];
        let titles = ["CSI", "CSA", "Cheats", "Options"];
        let mut buttons = [0usize; 4];

        for index in 0..4 {
            let button = add_tab_button(
                menu_parent,
                target,
                CGRect::new(
                    tab_w * index as f64,
                    0.0,
                    tab_w,
                    IOS_CLEO_TAB_HEIGHT,
                ),
                titles[index],
                tags[index],
            );
            buttons[index] = button as usize;
        }
        *TAB_BUTTONS.lock().unwrap() = buttons;

        let content_frame = CGRect::new(
            0.0,
            IOS_CLEO_TAB_HEIGHT,
            bounds.size.width,
            bounds.size.height - IOS_CLEO_TAB_HEIGHT - IOS_CLEO_CLOSE_HEIGHT,
        );
        let content: *mut Object = msg_send![class!(UIScrollView), alloc];
        let content: *mut Object = msg_send![content, initWithFrame: content_frame];
        set_bg(content, 0.0, 0.20);
        let _: () = msg_send![content, setAlwaysBounceVertical: true];
        let _: () = msg_send![menu_parent, addSubview: content];
        CONTENT_VIEW.store(content as usize, Ordering::SeqCst);
        let _: () = msg_send![content, release];

        let close: *mut Object = msg_send![class!(UIButton), alloc];
        let close: *mut Object = msg_send![
            close,
            initWithFrame: CGRect::new(
                0.0,
                bounds.size.height - IOS_CLEO_CLOSE_HEIGHT,
                bounds.size.width,
                IOS_CLEO_CLOSE_HEIGHT
            )
        ];
        let _: () = msg_send![close, setTitle: ns_string("Close") forState: 0u64];
        let _: () = msg_send![close, setTag: TAG_CLOSE];

        let red: *mut Object =
            msg_send![class!(UIColor), colorWithRed: 1.0f64 green: 0.23f64 blue: 0.30f64 alpha: 0.35f64];
        let _: () = msg_send![close, setBackgroundColor: red];

        let close_label: *mut Object = msg_send![close, titleLabel];
        let close_font: *mut Object = msg_send![class!(UIFont), boldSystemFontOfSize: 20.0f64];
        let _: () = msg_send![close_label, setFont: close_font];

        let _: () = msg_send![
            close,
            addTarget: target
            action: sel!(handleCleoMenuButton:)
            forControlEvents: 1u64 << 6
        ];
        let _: () = msg_send![menu_parent, addSubview: close];
        let _: () = msg_send![close, release];

        let _: () = msg_send![window, addSubview: blur_view];

        OVERLAY.store(blur_view as usize, Ordering::SeqCst);
        render_selected_tab();
    }
}

fn hide_menu() {
    let current = OVERLAY.swap(0, Ordering::SeqCst);
    CONTENT_VIEW.store(0, Ordering::SeqCst);
    *TAB_BUTTONS.lock().unwrap() = [0; 4];

    if current == 0 {
        return;
    }

    unsafe {
        let view = current as *mut Object;
        let _: () = msg_send![view, removeFromSuperview];
        let _: () = msg_send![view, release];
    }

}

fn toggle_menu() {
    if OVERLAY.load(Ordering::SeqCst) == 0 {
        show_menu();
    } else {
        hide_menu();
    }
}

extern "C" fn legal_splash_did_load(this: &mut Object, _cmd: Sel) {
    unsafe {
        let _: () = msg_send![this, cleoJailedOriginalViewDidLoad];
    }

    install_swipe_gesture();
    crate::jailed_runtime::init();
    install_runtime_timer();
}

fn hook_legal_splash() {
    unsafe {
        let class_name = CString::new("LegalSplash").unwrap();
        let class = runtime::objc_getClass(class_name.as_ptr());

        if class.is_null() {
            return;
        }

        let target_sel = sel!(viewDidLoad);
        let original_sel = sel!(cleoJailedOriginalViewDidLoad);
        let target_method = runtime::class_getInstanceMethod(class, target_sel);

        if target_method.is_null() {
            return;
        }

        let type_encoding = runtime::method_getTypeEncoding(target_method);

        let added = runtime::class_addMethod(
            class as *mut runtime::Class,
            original_sel,
            std::mem::transmute(legal_splash_did_load as extern "C" fn(&mut Object, Sel)),
            type_encoding,
        );

        if added == runtime::NO {
            return;
        }

        let replacement_method = runtime::class_getInstanceMethod(class, original_sel);
        runtime::method_exchangeImplementations(
            target_method as *mut runtime::Method,
            replacement_method as *mut runtime::Method,
        );
    }
}

pub fn init() {
    hook_legal_splash();
}
