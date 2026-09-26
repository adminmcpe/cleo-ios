//! Jailed/sideload proof-of-concept UI path.
//!
//! This module intentionally avoids CLEO's low-level game hooks.  It only uses
//! Objective-C/UIKit APIs so we can verify that a sideloaded build can stay
//! alive and receive a swipe gesture before reintroducing game integration.

use crate::meta::gui::{ns_string, CGRect};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Object, Sel},
    sel,
};
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicUsize, Ordering};

static GESTURE_TARGET: OnceCell<usize> = OnceCell::new();
static OVERLAY: AtomicUsize = AtomicUsize::new(0);

extern fn handle_cleo_swipe(_this: &Object, _cmd: Sel, _gesture: *mut Object) {
    log::info!("jailed swipe gesture received");
    toggle_test_menu();
}

fn gesture_target_class() -> &'static objc::runtime::Class {
    static CLASS: OnceCell<&'static objc::runtime::Class> = OnceCell::new();

    CLASS.get_or_init(|| {
        let mut decl =
            ClassDecl::new("CLEOJailedGestureTarget", class!(NSObject)).expect("class allocation failed");

        unsafe {
            decl.add_method(
                sel!(handleCleoSwipe:),
                handle_cleo_swipe as extern fn(&Object, Sel, *mut Object),
            );
        }

        decl.register()
    })
}

pub fn install_swipe_gesture(_source_view: *mut Object) {
    // LegalSplash's view is short-lived, so attach the recognizer to UIWindow.
    unsafe {
        let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
        let window: *mut Object = msg_send![app, keyWindow];

        if window.is_null() {
            log::error!("jailed gesture install failed: keyWindow is null");
            return;
        }

        if GESTURE_TARGET.get().is_some() {
            log::debug!("jailed swipe gesture already installed");
            return;
        }

        let target_class = gesture_target_class();
        let target: *mut Object = msg_send![target_class, new];

        let recognizer: *mut Object = msg_send![class!(UISwipeGestureRecognizer), alloc];
        let recognizer: *mut Object =
            msg_send![recognizer, initWithTarget: target action: sel!(handleCleoSwipe:)];

        // UISwipeGestureRecognizerDirectionDown == 1 << 3.
        let _: () = msg_send![recognizer, setDirection: 8usize];
        let _: () = msg_send![recognizer, setCancelsTouchesInView: false];
        let _: () = msg_send![window, addGestureRecognizer: recognizer];

        // UIWindow retains the recognizer; the recognizer retains its target.
        let _: () = msg_send![recognizer, release];

        let _ = GESTURE_TARGET.set(target as usize);
        log::info!("jailed one-finger swipe-down recognizer installed on keyWindow");
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
            log::info!("jailed test menu hidden");
            return;
        }

        let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
        let window: *mut Object = msg_send![app, keyWindow];

        if window.is_null() {
            log::error!("cannot show jailed test menu: keyWindow is null");
            return;
        }

        let bounds: CGRect = msg_send![window, bounds];

        let overlay: *mut Object = msg_send![class!(UIView), alloc];
        let overlay: *mut Object = msg_send![overlay, initWithFrame: bounds];

        let background: *mut Object =
            msg_send![class!(UIColor), colorWithWhite: 0.0f64 alpha: 0.82f64];
        let _: () = msg_send![overlay, setBackgroundColor: background];

        let label_frame = CGRect::new(
            bounds.size.width * 0.08,
            bounds.size.height * 0.28,
            bounds.size.width * 0.84,
            bounds.size.height * 0.44,
        );

        let label: *mut Object = msg_send![class!(UILabel), alloc];
        let label: *mut Object = msg_send![label, initWithFrame: label_frame];
        let _: () = msg_send![
            label,
            setText: ns_string("CLEO Jailed Menu\n\nGesture works without low-level hooks.\nSwipe down again to close.")
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
        log::info!("jailed test menu shown");
    }
}

pub fn init() {
    log::info!("jailed UIKit-only mode initialised; waiting for LegalSplash to install gesture");
}
