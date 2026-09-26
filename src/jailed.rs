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
use once_cell::sync::OnceCell;
use std::{
    ffi::CString,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
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

static GESTURE_TARGET: OnceCell<usize> = OnceCell::new();
static OVERLAY: AtomicUsize = AtomicUsize::new(0);
static CONTENT_VIEW: AtomicUsize = AtomicUsize::new(0);
static SELECTED_TAB: AtomicUsize = AtomicUsize::new(TAG_TAB_CSI as usize);
static TIMER_INSTALLED: AtomicBool = AtomicBool::new(false);

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
        let row = CGRect::new(width * 0.03, y, width * 0.94, 72.0);

        let container: *mut Object = msg_send![class!(UIView), alloc];
        let container: *mut Object = msg_send![container, initWithFrame: row];
        set_bg(container, 1.0, 0.08);

        add_label(
            container,
            CGRect::new(14.0, 7.0, row.size.width * 0.58, 27.0),
            title,
            18.0,
            0,
            0.96,
        );
        add_label(
            container,
            CGRect::new(14.0, 35.0, row.size.width * 0.68, 28.0),
            detail,
            12.0,
            0,
            0.58,
        );
        add_label(
            container,
            CGRect::new(row.size.width * 0.70, 7.0, row.size.width * 0.26, 54.0),
            value,
            15.0,
            2,
            0.90,
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
        let row = CGRect::new(width * 0.03, y, width * 0.94, 72.0);

        let button: *mut Object = msg_send![class!(UIButton), alloc];
        let button: *mut Object = msg_send![button, initWithFrame: row];
        let _: () = msg_send![button, setTag: tag];
        set_bg(button, 1.0, 0.08);

        add_label(
            button,
            CGRect::new(14.0, 7.0, row.size.width * 0.58, 27.0),
            title,
            18.0,
            0,
            0.96,
        );
        add_label(
            button,
            CGRect::new(14.0, 35.0, row.size.width * 0.68, 28.0),
            detail,
            12.0,
            0,
            0.58,
        );
        add_label(
            button,
            CGRect::new(row.size.width * 0.70, 7.0, row.size.width * 0.26, 54.0),
            value,
            15.0,
            2,
            0.90,
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
            CGRect::new(width * 0.06, 95.0, width * 0.88, 100.0),
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

    let mut y = 78.0;
    for (index, script) in scripts.into_iter().take(12).enumerate() {
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
        y += 78.0;
    }
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

    unsafe {
        let bounds: CGRect = msg_send![content, bounds];
        let width = bounds.size.width;

        match SELECTED_TAB.load(Ordering::SeqCst) as i64 {
            TAG_TAB_CSI => {
                add_label(
                    content,
                    CGRect::new(width * 0.04, 14.0, width * 0.92, 48.0),
                    "CSI Scripts",
                    28.0,
                    0,
                    1.0,
                );
                add_script_rows(content, width, "csi");
            }
            TAG_TAB_CSA => {
                add_label(
                    content,
                    CGRect::new(width * 0.04, 14.0, width * 0.92, 48.0),
                    "CSA Scripts",
                    28.0,
                    0,
                    1.0,
                );
                add_script_rows(content, width, "csa");
            }
            TAG_TAB_CHEATS => {
                add_label(
                    content,
                    CGRect::new(width * 0.04, 14.0, width * 0.92, 48.0),
                    "Cheats",
                    28.0,
                    0,
                    1.0,
                );
                add_row(
                    content,
                    78.0,
                    width,
                    "Built-in cheat runtime",
                    "Next porting stage after menu validation",
                    "Pending",
                );
                add_row(
                    content,
                    156.0,
                    width,
                    "Game-safe execution",
                    "Will use a UIKit-driven tick instead of hlhook",
                    "Pending",
                );
            }
            _ => {
                add_label(
                    content,
                    CGRect::new(width * 0.04, 14.0, width * 0.92, 48.0),
                    "Options",
                    28.0,
                    0,
                    1.0,
                );
                add_row(
                    content,
                    78.0,
                    width,
                    "Menu Gesture",
                    "Gesture used to open the CLEO menu",
                    "Swipe Down",
                );
                add_row(
                    content,
                    156.0,
                    width,
                    "Runtime Mode",
                    "UIKit timer + native SCM opcode handlers",
                    if crate::jailed_runtime::is_in_game() { "Jailed / In Game" } else { "Jailed / Waiting" },
                );
                add_row(
                    content,
                    234.0,
                    width,
                    "CLEO Base",
                    "Source branch used for this port",
                    "2.6.0",
                );
            }
        }
    }
}

extern "C" fn handle_cleo_swipe(_this: &Object, _cmd: Sel, _gesture: *mut Object) {
    toggle_menu();
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

        if (TAG_CSA_BASE..(TAG_CSA_BASE + 1000)).contains(&tag) {
            let index = (tag - TAG_CSA_BASE) as usize;
            crate::jailed_runtime::toggle_csa(index);
            render_selected_tab();
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
        let _: () = msg_send![window, addGestureRecognizer: recognizer];
        let _: () = msg_send![recognizer, release];

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
        let _: *mut Object = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: (1.0f64 / 60.0f64)
            target: target as *mut Object
            selector: sel!(cleoRuntimeTick:)
            userInfo: std::ptr::null_mut::<Object>()
            repeats: true
        ];
    }
}

fn add_tab_button(
    parent: *mut Object,
    target: *mut Object,
    frame: CGRect,
    title: &str,
    tag: i64,
) {
    unsafe {
        let button: *mut Object = msg_send![class!(UIButton), alloc];
        let button: *mut Object = msg_send![button, initWithFrame: frame];
        let _: () = msg_send![button, setTitle: ns_string(title) forState: 0u64];
        let _: () = msg_send![button, setTag: tag];
        set_bg(button, 1.0, 0.12);

        let label: *mut Object = msg_send![button, titleLabel];
        let font: *mut Object = msg_send![class!(UIFont), boldSystemFontOfSize: 15.0f64];
        let _: () = msg_send![label, setFont: font];

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

fn show_menu() {
    if OVERLAY.load(Ordering::SeqCst) != 0 {
        return;
    }

    unsafe {
        let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
        let window: *mut Object = msg_send![app, keyWindow];

        if window.is_null() {
            return;
        }

        let bounds: CGRect = msg_send![window, bounds];

        let overlay: *mut Object = msg_send![class!(UIView), alloc];
        let overlay: *mut Object = msg_send![overlay, initWithFrame: bounds];
        set_bg(overlay, 0.0, 0.78);

        let target = GESTURE_TARGET
            .get()
            .copied()
            .unwrap_or_else(|| {
                let t: *mut Object = msg_send![target_class(), new];
                t as usize
            }) as *mut Object;

        let tab_h = 54.0;
        let close_h = 58.0;
        let tab_w = bounds.size.width / 4.0;

        add_tab_button(
            overlay,
            target,
            CGRect::new(0.0, 0.0, tab_w, tab_h),
            "CSI",
            TAG_TAB_CSI,
        );
        add_tab_button(
            overlay,
            target,
            CGRect::new(tab_w, 0.0, tab_w, tab_h),
            "CSA",
            TAG_TAB_CSA,
        );
        add_tab_button(
            overlay,
            target,
            CGRect::new(tab_w * 2.0, 0.0, tab_w, tab_h),
            "Cheats",
            TAG_TAB_CHEATS,
        );
        add_tab_button(
            overlay,
            target,
            CGRect::new(tab_w * 3.0, 0.0, tab_w, tab_h),
            "Options",
            TAG_TAB_OPTIONS,
        );

        let content_frame =
            CGRect::new(0.0, tab_h, bounds.size.width, bounds.size.height - tab_h - close_h);
        let content: *mut Object = msg_send![class!(UIView), alloc];
        let content: *mut Object = msg_send![content, initWithFrame: content_frame];
        set_bg(content, 0.0, 0.16);
        let _: () = msg_send![overlay, addSubview: content];
        CONTENT_VIEW.store(content as usize, Ordering::SeqCst);
        let _: () = msg_send![content, release];

        let close: *mut Object = msg_send![class!(UIButton), alloc];
        let close: *mut Object = msg_send![
            close,
            initWithFrame: CGRect::new(
                0.0,
                bounds.size.height - close_h,
                bounds.size.width,
                close_h
            )
        ];
        let _: () = msg_send![close, setTitle: ns_string("Close") forState: 0u64];
        let _: () = msg_send![close, setTag: TAG_CLOSE];

        let red: *mut Object =
            msg_send![class!(UIColor), colorWithRed: 1.0f64 green: 0.23f64 blue: 0.30f64 alpha: 0.38f64];
        let _: () = msg_send![close, setBackgroundColor: red];

        let close_label: *mut Object = msg_send![close, titleLabel];
        let close_font: *mut Object = msg_send![class!(UIFont), boldSystemFontOfSize: 18.0f64];
        let _: () = msg_send![close_label, setFont: close_font];

        let _: () = msg_send![
            close,
            addTarget: target
            action: sel!(handleCleoMenuButton:)
            forControlEvents: 1u64 << 6
        ];

        let _: () = msg_send![overlay, addSubview: close];
        let _: () = msg_send![close, release];

        let _: () = msg_send![window, addSubview: overlay];

        OVERLAY.store(overlay as usize, Ordering::SeqCst);
        render_selected_tab();
    }
}

fn hide_menu() {
    let current = OVERLAY.swap(0, Ordering::SeqCst);
    CONTENT_VIEW.store(0, Ordering::SeqCst);

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
