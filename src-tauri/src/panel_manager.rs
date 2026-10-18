use objc2::{
    class,
    declare::ClassBuilder,
    msg_send,
    runtime::{AnyClass as Class, AnyObject as Object, Bool, Sel},
    sel,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use std::ptr;
use std::sync::Once;
use tracing::{debug, info};

#[allow(clippy::upper_case_acronyms)]
type BOOL = Bool;
const YES: Bool = Bool::YES;
const NO: Bool = Bool::NO;

/// Geometry rectangle used for cross-coordinate conversions and window frames.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Converts Spotlight bounds in Quartz/CoreGraphics coordinate space
/// (origin at top-left of the primary display, Y increasing downward)
/// to AppKit coordinate space (origin at bottom-left of the primary display, Y increasing upward)
/// for our overlay panel positioned immediately below Spotlight's header.
///
/// Required Specification Formula (Section 4.1):
/// X_panel = X_spot (centered if search-bar only)
/// W_panel = W_spot (or 1200 if search-bar only)
/// H_panel = H_spot - H_header (or 692 if search-bar only)
/// Y_panel = H_primary - (Y_spot + H_header + H_panel)
pub fn quartz_to_appkit_panel_frame(
    spotlight_quartz: Rect,
    header_offset: f64,
    primary_screen_height: f64,
) -> Rect {
    let (w_panel, h_panel) = if spotlight_quartz.height > 120.0 {
        (
            spotlight_quartz.width,
            spotlight_quartz.height - header_offset,
        )
    } else {
        (spotlight_quartz.width, 509.0)
    };
    let x_panel = spotlight_quartz.x;
    let y_panel = primary_screen_height - (spotlight_quartz.y + header_offset + h_panel);

    Rect {
        x: x_panel,
        y: y_panel,
        width: w_panel,
        height: h_panel,
    }
}

/// Returns (is_mouse_inside_panel, is_left_mouse_button_down) in AppKit screen coordinates.
pub fn get_mouse_interaction_state(panel_frame: Rect) -> (bool, bool) {
    unsafe {
        let mouse_loc: NSPoint = msg_send![class!(NSEvent), mouseLocation];
        let pressed_buttons: usize = msg_send![class!(NSEvent), pressedMouseButtons];
        let is_button_down = (pressed_buttons & 1) != 0;
        let in_panel = mouse_loc.x >= panel_frame.x
            && mouse_loc.x <= panel_frame.x + panel_frame.width
            && mouse_loc.y >= panel_frame.y
            && mouse_loc.y <= panel_frame.y + panel_frame.height;
        (in_panel, is_button_down)
    }
}

/// Retrieves the primary display's pixel height via NSScreen::screens()[0].frame().size.height.
pub fn get_primary_screen_height() -> f64 {
    unsafe {
        let screens: *mut Object = msg_send![class!(NSScreen), screens];
        if screens.is_null() {
            return 1440.0;
        }
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return 1440.0;
        }
        let primary: *mut Object = msg_send![screens, objectAtIndex: 0usize];
        let frame: NSRect = msg_send![primary, frame];
        frame.size.height
    }
}

extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(
        dict: core_foundation::dictionary::CFDictionaryRef,
        rect: *mut core_graphics::geometry::CGRect,
    ) -> bool;
    fn object_setClass(obj: *mut Object, cls: *const Class) -> *const Class;
}

/// Queries on-screen windows to find the active Spotlight / Siri search bar window.
/// Returns (bounds, layer) if the search bar is currently visible on screen.
pub fn get_active_spotlight_window(spotlight_pid: i32) -> Option<(Rect, i32)> {
    unsafe {
        use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex};
        use core_foundation::dictionary::{CFDictionaryGetValue, CFDictionaryRef};
        use core_foundation::number::{kCFNumberSInt32Type, CFNumberGetValue, CFNumberRef};
        use core_graphics::geometry::{CGPoint, CGRect, CGSize};
        use core_graphics::window::{
            kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowListOptionOnScreenOnly,
            kCGWindowOwnerPID, CGWindowListCopyWindowInfo,
        };

        let array_ref =
            CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
        if array_ref.is_null() {
            return None;
        }

        let count = CFArrayGetCount(array_ref);
        let mut found = None;

        for i in 0..count {
            let dict_ptr = CFArrayGetValueAtIndex(array_ref, i) as CFDictionaryRef;
            if dict_ptr.is_null() {
                continue;
            }

            let pid_val =
                CFDictionaryGetValue(dict_ptr, kCGWindowOwnerPID as *const _) as CFNumberRef;
            if !pid_val.is_null() {
                let mut pid: i32 = 0;
                if CFNumberGetValue(pid_val, kCFNumberSInt32Type, &mut pid as *mut _ as *mut _)
                    && pid == spotlight_pid
                {
                    let layer_val =
                        CFDictionaryGetValue(dict_ptr, kCGWindowLayer as *const _) as CFNumberRef;
                    let bounds_val = CFDictionaryGetValue(dict_ptr, kCGWindowBounds as *const _);

                    if !layer_val.is_null() && !bounds_val.is_null() {
                        let mut layer: i32 = 0;
                        CFNumberGetValue(
                            layer_val,
                            kCFNumberSInt32Type,
                            &mut layer as *mut _ as *mut _,
                        );

                        let mut rect = CGRect {
                            origin: CGPoint::new(0.0, 0.0),
                            size: CGSize::new(0.0, 0.0),
                        };
                        if layer > 0
                            && CGRectMakeWithDictionaryRepresentation(bounds_val as _, &mut rect)
                            && rect.size.width >= 350.0
                            && rect.size.width <= 1600.0
                            && rect.size.height >= 30.0
                            && rect.size.height <= 1200.0
                        {
                            found = Some((
                                Rect::new(
                                    rect.origin.x,
                                    rect.origin.y,
                                    rect.size.width,
                                    rect.size.height,
                                ),
                                layer,
                            ));
                            break;
                        }
                    }
                }
            }
        }

        core_foundation::base::CFRelease(array_ref as *const _);
        found
    }
}

static REGISTER_CLASS_ONCE: Once = Once::new();
const PANEL_CLASS_NAME: &std::ffi::CStr = c"AppGridNonActivatingPanel";

/// Registers our custom NSPanel subclass implementing Section 4.3 focus overrides:
/// - canBecomeKeyWindow -> false
/// - canBecomeMainWindow -> false
/// - becomesKeyOnlyIfNeeded -> false
/// - _preventsActivation -> true
/// - hidesOnDeactivate -> false
pub fn ensure_non_activating_panel_class() -> &'static Class {
    REGISTER_CLASS_ONCE.call_once(|| {
        if Class::get(PANEL_CLASS_NAME).is_some() {
            return;
        }

        let mut decl = ClassBuilder::new(PANEL_CLASS_NAME, class!(NSPanel))
            .unwrap_or_else(|| panic!("Failed to declare {:?} class", PANEL_CLASS_NAME));

        extern "C" fn can_become_key_window(_: *mut Object, _: Sel) -> BOOL {
            NO
        }

        extern "C" fn can_become_main_window(_: *mut Object, _: Sel) -> BOOL {
            NO
        }

        extern "C" fn becomes_key_only_if_needed(_: *mut Object, _: Sel) -> BOOL {
            NO
        }

        extern "C" fn prevents_activation(_: *mut Object, _: Sel) -> BOOL {
            YES
        }

        extern "C" fn hides_on_deactivate(_: *mut Object, _: Sel) -> BOOL {
            NO
        }

        unsafe {
            decl.add_method(
                sel!(canBecomeKeyWindow),
                can_become_key_window as extern "C" fn(*mut Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(canBecomeMainWindow),
                can_become_main_window as extern "C" fn(*mut Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(becomesKeyOnlyIfNeeded),
                becomes_key_only_if_needed as extern "C" fn(*mut Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(_preventsActivation),
                prevents_activation as extern "C" fn(*mut Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(hidesOnDeactivate),
                hides_on_deactivate as extern "C" fn(*mut Object, Sel) -> BOOL,
            );
        }

        decl.register();
    });

    Class::get(PANEL_CLASS_NAME).expect("Class must exist after registration")
}

/// Configures a Tauri WebviewWindow as a borderless non-activating NSPanel according to Section 4.3:
/// 1. NSApp.setActivationPolicy(NSApplicationActivationPolicyAccessory)
/// 2. Style Mask: NSWindowStyleMaskBorderless | NSWindowStyleMaskNonactivatingPanel (1 << 7 = 128)
/// 3. Collection Behavior: CanJoinAllSpaces (1 << 0) | FullScreenAuxiliary (1 << 8) | Stationary (1 << 4) | IgnoresCycle (1 << 6)
/// 4. Subclasses window to AppGridNonActivatingPanel (never steals key focus)
pub fn setup_non_activating_panel<R: tauri::Runtime>(window: &tauri::WebviewWindow<R>) {
    unsafe {
        // 1. Activation policy: accessory (no dock icon, never activates over key apps)
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        // NSApplicationActivationPolicyAccessory = 1
        let _: () = msg_send![app, setActivationPolicy: 1isize];

        // 2. Swizzle window class to our non-activating panel class
        let nswindow = window.ns_window().expect("Failed to get ns_window") as *mut Object;
        let panel_class = ensure_non_activating_panel_class();
        object_setClass(nswindow, panel_class as *const Class);

        // 3. Style mask: NSWindowStyleMaskBorderless (0) | NSWindowStyleMaskNonactivatingPanel (1 << 7 = 128)
        let style_mask: usize = 1 << 7;
        let _: () = msg_send![nswindow, setStyleMask: style_mask];
        let _: () = msg_send![nswindow, _setPreventsActivation: YES];

        // 4. Collection behaviors
        // NSWindowCollectionBehaviorCanJoinAllSpaces (1 << 0)
        // NSWindowCollectionBehaviorFullScreenAuxiliary (1 << 8)
        // NSWindowCollectionBehaviorStationary (1 << 4)
        // NSWindowCollectionBehaviorIgnoresCycle (1 << 6)
        let collection_behavior: usize = (1 << 0) | (1 << 8) | (1 << 4) | (1 << 6);
        let _: () = msg_send![nswindow, setCollectionBehavior: collection_behavior];

        // 5. Additional focus & appearance properties
        let _: () = msg_send![nswindow, setFloatingPanel: YES];
        let _: () = msg_send![nswindow, setHidesOnDeactivate: NO];
        let _: () = msg_send![nswindow, setBecomesKeyOnlyIfNeeded: NO];
        let _: () = msg_send![nswindow, setWorksWhenModal: YES];
        let _: () = msg_send![nswindow, setOpaque: NO];
        let _: () = msg_send![nswindow, setHasShadow: NO];
        let clear_color: *mut Object = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![nswindow, setBackgroundColor: clear_color];
        let _: () = msg_send![nswindow, invalidateShadow];
        let _: () = msg_send![nswindow, setAcceptsMouseMovedEvents: YES];

        // 6. Attach NSTrackingArea (ActiveAlways | InVisibleRect | MouseEnteredAndExited | MouseMoved)
        // to contentView so WKWebView receives hover & mouse events without activating the window.
        let content_view: *mut Object = msg_send![nswindow, contentView];
        if !content_view.is_null() {
            let bounds: NSRect = msg_send![content_view, bounds];
            // NSTrackingMouseEnteredAndExited (0x01) | NSTrackingMouseMoved (0x02)
            // | NSTrackingCursorUpdate (0x04) | NSTrackingActiveAlways (0x80)
            // | NSTrackingInVisibleRect (0x200)
            let options: usize = 0x01 | 0x02 | 0x04 | 0x80 | 0x200;
            let tracking_area_alloc: *mut Object = msg_send![class!(NSTrackingArea), alloc];
            let tracking_area: *mut Object = msg_send![
                tracking_area_alloc,
                initWithRect: bounds,
                options: options,
                owner: content_view,
                userInfo: ptr::null_mut::<Object>()
            ];
            if !tracking_area.is_null() {
                let _: () = msg_send![content_view, addTrackingArea: tracking_area];
            }
        }

        info!("Non-activating NSPanel initialized with styleMask 0x80 and Accessory activation policy");
    }
}

/// Applies corner mask (24px radius) to a view and its layer.
/// When round_top_corners is true (Spotlight mode): all 4 corners rounded (maskedCorners = 15).
/// When round_top_corners is false (Launchpad mode): only bottom-left and bottom-right rounded.
unsafe fn apply_corner_mask(view: *mut Object, round_top_corners: bool) {
    if view.is_null() {
        return;
    }
    let is_flipped: BOOL = msg_send![view, isFlipped];
    // In CALayer:
    // kCALayerMinXMinYCorner = 1
    // kCALayerMaxXMinYCorner = 2
    // kCALayerMinXMaxYCorner = 4
    // kCALayerMaxXMaxYCorner = 8
    // When is_flipped == YES: MinY is TOP (1 | 2 = 3), MaxY is BOTTOM (4 | 8 = 12)
    // When is_flipped == NO: MinY is BOTTOM (1 | 2 = 3), MaxY is TOP (4 | 8 = 12)
    let masked_corners: usize = if round_top_corners {
        15 // All 4 corners
    } else if is_flipped.as_bool() {
        12 // MaxY (bottom-left, bottom-right when flipped)
    } else {
        3 // MinY (bottom-left, bottom-right when unflipped)
    };
    let layer: *mut Object = msg_send![view, layer];
    if !layer.is_null() {
        let _: () = msg_send![layer, setCornerRadius: 24.0f64];
        let _: () = msg_send![layer, setMaskedCorners: masked_corners];
        let _: () = msg_send![layer, setMasksToBounds: YES];
    }
}

/// Applies AppKit frame coordinates, dynamic level, and corner radius mask to the NSWindow and its layers.
pub fn set_panel_frame_and_level<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
    frame: Rect,
    level: i32,
    round_top_corners: bool,
) {
    unsafe {
        let nswindow = window.ns_window().expect("ns_window") as *mut Object;
        let ns_rect = NSRect {
            origin: NSPoint {
                x: frame.x,
                y: frame.y,
            },
            size: NSSize {
                width: frame.width,
                height: frame.height,
            },
        };

        // setFrame:display:
        let _: () = msg_send![nswindow, setFrame: ns_rect, display: YES];
        // setLevel:
        let _: () = msg_send![nswindow, setLevel: level];

        // Apply dynamic corner radius to contentView and its subviews (NSVisualEffectView, WKWebView)
        let content_view: *mut Object = msg_send![nswindow, contentView];
        if !content_view.is_null() {
            let _: () = msg_send![content_view, setWantsLayer: YES];
            apply_corner_mask(content_view, round_top_corners);
            let subviews: *mut Object = msg_send![content_view, subviews];
            if !subviews.is_null() {
                let count: usize = msg_send![subviews, count];
                for i in 0..count {
                    let subview: *mut Object = msg_send![subviews, objectAtIndex: i];
                    if !subview.is_null() {
                        let _: () = msg_send![subview, setWantsLayer: YES];
                        apply_corner_mask(subview, round_top_corners);
                    }
                }
            }
        }

        debug!(
            "Panel frame updated to ({}, {}, {}, {}) at level {} (round_top_corners={})",
            frame.x, frame.y, frame.width, frame.height, level, round_top_corners
        );
    }
}

/// Orders the panel front without activating the application or stealing keyboard focus.
pub fn order_front_non_activating<R: tauri::Runtime>(window: &tauri::WebviewWindow<R>) {
    unsafe {
        let nswindow = window.ns_window().expect("ns_window") as *mut Object;
        let _: () = msg_send![nswindow, orderFrontRegardless];
    }
}

/// Orders the panel out immediately.
pub fn order_out_panel<R: tauri::Runtime>(window: &tauri::WebviewWindow<R>) {
    unsafe {
        let nswindow = window.ns_window().expect("ns_window") as *mut Object;
        let _: () = msg_send![nswindow, orderOut: ptr::null_mut::<Object>()];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UT-01: Quartz-to-AppKit Coordinate Flip (Single Display)
    /// Given primary screen 2560 x 1440, Spotlight at X=680, Y=220, W=1200, H=760, header offset 68.
    /// Expected assertion: panel_frame equals X=680, Y=460, W=1200, H=692.
    /// Calculation:
    /// W_panel = 1200
    /// H_panel = 760 - 68 = 692
    /// Y_panel = 1440 - (220 + 68 + 692) = 1440 - 980 = 460
    #[test]
    fn ut01_quartz_to_appkit_single_display() {
        let primary_height = 1440.0;
        let spotlight = Rect::new(680.0, 220.0, 1200.0, 760.0);
        let header_offset = 68.0;

        let panel_frame = quartz_to_appkit_panel_frame(spotlight, header_offset, primary_height);

        assert_eq!(
            panel_frame,
            Rect {
                x: 680.0,
                y: 460.0,
                width: 1200.0,
                height: 692.0,
            }
        );
    }

    /// UT-02: Multi-Monitor Negative/Offset Coordinates
    /// Secondary monitor placed above or to the left of primary display (Y_spot < 0 or X_spot < 0).
    /// Conversion uses primary screen height H_primary as reference origin (per AppKit multi-screen spec)
    /// and produces exact bounds on secondary display.
    #[test]
    fn ut02_multi_monitor_negative_offset_coordinates() {
        let primary_height = 1440.0;
        let header_offset = 68.0;

        // Case A: Secondary monitor to the left (X < 0)
        let spotlight_left = Rect::new(-1920.0 + 360.0, 100.0, 1200.0, 760.0);
        let frame_left =
            quartz_to_appkit_panel_frame(spotlight_left, header_offset, primary_height);
        assert_eq!(frame_left.x, -1560.0);
        assert_eq!(frame_left.width, 1200.0);
        assert_eq!(frame_left.height, 692.0);
        assert_eq!(frame_left.y, 1440.0 - (100.0 + 68.0 + 692.0)); // 580.0

        // Case B: Secondary monitor above primary (Y < 0 in Quartz)
        let spotlight_above = Rect::new(200.0, -1080.0 + 150.0, 1200.0, 760.0);
        let frame_above =
            quartz_to_appkit_panel_frame(spotlight_above, header_offset, primary_height);
        assert_eq!(frame_above.x, 200.0);
        assert_eq!(frame_above.width, 1200.0);
        assert_eq!(frame_above.height, 692.0);
        // Y in AppKit will be > H_primary because monitor is above
        let expected_y = 1440.0 - (-930.0 + 68.0 + 692.0);
        assert_eq!(frame_above.y, expected_y); // 1440 - (-170) = 1610.0
    }
}
