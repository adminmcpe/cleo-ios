//! Minimal jailbreak-free proof of concept.
//!
//! No game code is patched here. The only hook is Objective-C method swizzling,
//! which already proved functional in the user's CLEO 2.6.0 log.

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
    sync::atomic::{AtomicUsize, Ordering},
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

static GESTURE_TARGET: OnceCell<usize> = OnceCell::new();
static OVERLAY: AtomicUsize = AtomicUsize::new(0);

fn ns_string(value: &str) -> *const Object {
    unsafe {
        let value = CString::new(value).unwrap();
        msg_send![class!(NSString), stringWithUTF8String: value.as_ptr()]
    }
}

extern "C" fn handle_cleo_swipe(_this: &Object, _cmd: Sel, _gesture: *mut Object) {
    toggle_test_menu();
}

fn gesture_target_class() -> &'static runtime::Class {
    static CLASS: OnceCell<&'static runtime::Class> = OnceCell::new();

    CLASS.get_or_init(|| {
        let mut decl =
            ClassDecl::new("CLEOJailedGestureTarget", class!(NSObject)).expect("class allocation failed");

        unsafe {
            decl.add_method(
                sel!(handleCleoSwipe:),
                handle_cleo_swipe as extern "C" fn(&Object, Sel, *mut Object),
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

        let target_class = gesture_target_class();
        let target: *mut Object = msg_send![target_class, new];

        let recognizer: *mut Object = msg_send![class!(UISwipeGestureRecognizer), alloc];
        let recognizer: *mut Object =
            msg_send![recognizer, initWithTarget: target action: sel!(handleCleoSwipe:)];

        // UISwipeGestureRecognizerDirectionDown.
        let _: () = msg_send![recognizer, setDirection: 8usize];
        let _: () = msg_send![recognizer, setCancelsTouchesInView: false];
        let _: () = msg_send![window, addGestureRecognizer: recognizer];
        let _: () = msg_send![recognizer, release];

        let _ = GESTURE_TARGET.set(target as usize);
    }
}

fn toggle_test_menu() {
    unsafe {
        let current = OVERLAY.load(Ordering::SeqCst);

        if current != 0 {
            let view = current as *mut Object;
            let _: () = msg_send![view, removeFromSuperview];
            let _: () = msg_send![view, release];
            OVERLAY.store(0, Ordering::SeqCst);
            return;
        }

        let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
        let window: *mut Object = msg_send![app, keyWindow];

        if window.is_null() {
            return;
        }

        let bounds: CGRect = msg_send![window, bounds];

        let overlay: *mut Object = msg_send![class!(UIView), alloc];
        let overlay: *mut Object = msg_send![overlay, initWithFrame: bounds];

        let background: *mut Object =
            msg_send![class!(UIColor), colorWithWhite: 0.0f64 alpha: 0.82f64];
        let _: () = msg_send![overlay, setBackgroundColor: background];

        let frame = CGRect::new(
            bounds.size.width * 0.08,
            bounds.size.height * 0.28,
            bounds.size.width * 0.84,
            bounds.size.height * 0.44,
        );

        let label: *mut Object = msg_send![class!(UILabel), alloc];
        let label: *mut Object = msg_send![label, initWithFrame: frame];
        let _: () = msg_send![
            label,
            setText: ns_string("CLEO Jailed Menu\n\nSwipe gesture works.\nSwipe down again to close.")
        ];
        let _: () = msg_send![label, setNumberOfLines: 0i64];
        let _: () = msg_send![label, setTextAlignment: 1i64];

        let white: *mut Object = msg_send![class!(UIColor), whiteColor];
        let _: () = msg_send![label, setTextColor: white];

        let font: *mut Object = msg_send![class!(UIFont), boldSystemFontOfSize: 24.0f64];
        let _: () = msg_send![label, setFont: font];

        let _: () = msg_send![overlay, addSubview: label];
        let _: () = msg_send![label, release];
        let _: () = msg_send![window, addSubview: overlay];

        OVERLAY.store(overlay as usize, Ordering::SeqCst);
    }
}

extern "C" fn legal_splash_did_load(this: &mut Object, _cmd: Sel) {
    unsafe {
        // Call the original implementation after method exchange.
        let _: () = msg_send![this, cleoJailedOriginalViewDidLoad];
    }

    install_swipe_gesture();
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
