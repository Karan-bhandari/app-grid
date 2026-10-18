use core_foundation::base::CFRelease;
use core_foundation::runloop::{
    kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopSourceRef,
};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info};

pub static IS_EDITING_FOLDER_NAME: AtomicBool = AtomicBool::new(false);
pub static IS_OVERLAY_VISIBLE: AtomicBool = AtomicBool::new(false);
pub static HAS_KEYBOARD_SELECTION: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, serde::Serialize)]
pub struct FolderKeyEvent {
    pub key: String,
    pub key_code: i64,
    pub is_enter: bool,
    pub is_escape: bool,
    pub is_backspace: bool,
}

pub type KeyEventListener = Arc<dyn Fn(FolderKeyEvent) + Send + Sync + 'static>;

type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CFMachPortRef = *mut c_void;

const K_CG_HID_EVENT_TAP: u32 = 0;
const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const K_CG_EVENT_KEY_DOWN: u32 = 10;
const K_CG_KEYBOARD_EVENT_KEYCODE: u32 = 9;
const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: extern "C" fn(
            proxy: CGEventTapProxy,
            r#type: u32,
            event: CGEventRef,
            refcon: *mut c_void,
        ) -> CGEventRef,
        refcon: *mut c_void,
    ) -> CFMachPortRef;

    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);

    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;

    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;

    fn CGEventGetFlags(event: CGEventRef) -> u64;

    fn CGEventKeyboardGetUnicodeString(
        event: CGEventRef,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    );
}

pub struct KeyInterceptor {
    mach_port: CFMachPortRef,
}

unsafe impl Send for KeyInterceptor {}
unsafe impl Sync for KeyInterceptor {}

static INTERCEPTOR_INSTANCE: Mutex<Option<KeyInterceptor>> = Mutex::new(None);

pub fn is_key_intercept_initialized() -> bool {
    INTERCEPTOR_INSTANCE
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

pub fn set_folder_rename_editing(editing: bool) {
    if IS_EDITING_FOLDER_NAME.swap(editing, Ordering::SeqCst) != editing {
        info!("Folder rename interception state set to: {}", editing);
    }
}

pub fn is_folder_rename_editing() -> bool {
    IS_EDITING_FOLDER_NAME.load(Ordering::SeqCst)
}

pub fn set_overlay_visible(visible: bool) {
    IS_OVERLAY_VISIBLE.store(visible, Ordering::SeqCst);
    if !visible {
        HAS_KEYBOARD_SELECTION.store(false, Ordering::SeqCst);
    }
}

pub fn has_keyboard_selection() -> bool {
    HAS_KEYBOARD_SELECTION.load(Ordering::SeqCst)
}

extern "C" fn raw_event_tap_callback(
    _proxy: CGEventTapProxy,
    r#type: u32,
    event: CGEventRef,
    refcon: *mut c_void,
) -> CGEventRef {
    if r#type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT
        || r#type == K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        if let Ok(guard) = INTERCEPTOR_INSTANCE.lock() {
            if let Some(inst) = guard.as_ref() {
                unsafe {
                    CGEventTapEnable(inst.mach_port, true);
                }
            }
        }
        return event;
    }

    if event.is_null() || r#type != K_CG_EVENT_KEY_DOWN || refcon.is_null() {
        return event;
    }

    let listener = unsafe { &*(refcon as *const KeyEventListener) };

    if !IS_EDITING_FOLDER_NAME.load(Ordering::SeqCst) {
        unsafe {
            let key_code = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE);
            let flags = CGEventGetFlags(event);

            // Track Cmd+1 (Apps) vs Cmd+2/3/4 (Files/Actions/Clipboard)
            if (flags & 0x0010_0000) != 0 {
                match key_code {
                    18 => crate::spotlight_hook::set_user_hidden_for_session(false),
                    19..=21 => crate::spotlight_hook::set_user_hidden_for_session(true),
                    _ => {}
                }
                return event;
            }

            if key_code == 53 {
                HAS_KEYBOARD_SELECTION.store(false, Ordering::SeqCst);
                crate::spotlight_hook::set_user_hidden_for_session(true);
                return event;
            }

            // When overlay is visible, intercept arrow keys (123=Left, 124=Right, 125=Down, 126=Up)
            // and Enter/Return (36, 76) when a grid item is selected via keyboard.
            if IS_OVERLAY_VISIBLE.load(Ordering::SeqCst) {
                let arrow_key = match key_code {
                    123 => Some("ArrowLeft"),
                    124 => Some("ArrowRight"),
                    125 => Some("ArrowDown"),
                    126 => Some("ArrowUp"),
                    _ => None,
                };

                if let Some(arrow_name) = arrow_key {
                    debug!("Intercepted arrow key for overlay grid: {}", arrow_name);
                    HAS_KEYBOARD_SELECTION.store(true, Ordering::SeqCst);
                    listener(FolderKeyEvent {
                        key: arrow_name.to_string(),
                        key_code,
                        is_enter: false,
                        is_escape: false,
                        is_backspace: false,
                    });
                    // Consume arrow key so native Spotlight underneath does not move selection
                    return ptr::null_mut();
                }

                if (key_code == 36 || key_code == 76)
                    && HAS_KEYBOARD_SELECTION.load(Ordering::SeqCst)
                {
                    debug!("Intercepted Enter key for overlay grid selection");
                    listener(FolderKeyEvent {
                        key: "Enter".to_string(),
                        key_code,
                        is_enter: true,
                        is_escape: false,
                        is_backspace: false,
                    });
                    // Consume Enter key so native Spotlight does not launch its own selection
                    return ptr::null_mut();
                }

                // Any other key (e.g., typing a search query) clears keyboard selection and passes through to Spotlight
                HAS_KEYBOARD_SELECTION.store(false, Ordering::SeqCst);
            }
        }
        return event;
    }

    unsafe {
        let key_code = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE);

        let is_enter = key_code == 36 || key_code == 76;
        let is_escape = key_code == 53;
        let is_backspace = key_code == 51;

        let mut unicode_buf = [0u16; 8];
        let mut actual_len: usize = 0;
        CGEventKeyboardGetUnicodeString(
            event,
            unicode_buf.len(),
            &mut actual_len,
            unicode_buf.as_mut_ptr(),
        );

        let key_str = if actual_len > 0 {
            String::from_utf16_lossy(&unicode_buf[..actual_len])
        } else if is_enter {
            "Enter".to_string()
        } else if is_escape {
            "Escape".to_string()
        } else if is_backspace {
            "Backspace".to_string()
        } else {
            String::new()
        };

        debug!(
            "Intercepted key for folder rename: key='{}', code={}",
            key_str, key_code
        );

        listener(FolderKeyEvent {
            key: key_str,
            key_code,
            is_enter,
            is_escape,
            is_backspace,
        });

        if is_enter || is_escape {
            IS_EDITING_FOLDER_NAME.store(false, Ordering::SeqCst);
            info!("Folder rename completed; exited key interception mode");
        }

        // Return NULL to consume the event so Spotlight never receives it!
        ptr::null_mut()
    }
}

/// Initializes the global CGEventTap hook listening for KeyDown events.
pub fn init_key_intercept<F: Fn(FolderKeyEvent) + Send + Sync + 'static>(
    listener: F,
) -> anyhow::Result<()> {
    let mut guard = INTERCEPTOR_INSTANCE.lock().unwrap();
    if guard.is_some() {
        return Ok(());
    }

    let listener_arc: KeyEventListener = Arc::new(listener);
    let refcon = Box::into_raw(Box::new(listener_arc)) as *mut c_void;

    let event_mask: u64 = 1 << K_CG_EVENT_KEY_DOWN;

    unsafe {
        let mut mach_port = CGEventTapCreate(
            K_CG_HID_EVENT_TAP,
            K_CG_HEAD_INSERT_EVENT_TAP,
            K_CG_EVENT_TAP_OPTION_DEFAULT,
            event_mask,
            raw_event_tap_callback,
            refcon,
        );

        if mach_port.is_null() {
            mach_port = CGEventTapCreate(
                K_CG_SESSION_EVENT_TAP,
                K_CG_HEAD_INSERT_EVENT_TAP,
                K_CG_EVENT_TAP_OPTION_DEFAULT,
                event_mask,
                raw_event_tap_callback,
                refcon,
            );
        }

        if mach_port.is_null() {
            let _ = Box::from_raw(refcon as *mut KeyEventListener);
            anyhow::bail!("Failed to create CGEventTap (Accessibility permission may be required)");
        }

        let source = CFMachPortCreateRunLoopSource(ptr::null(), mach_port, 0);
        if source.is_null() {
            CFRelease(mach_port);
            let _ = Box::from_raw(refcon as *mut KeyEventListener);
            anyhow::bail!("Failed to create RunLoopSource for event tap mach port");
        }

        CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(mach_port, true);

        *guard = Some(KeyInterceptor { mach_port });
        info!("CGEventTap key interceptor successfully registered on main runloop");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atomic_state_toggle() {
        set_folder_rename_editing(false);
        assert!(!is_folder_rename_editing());
        set_folder_rename_editing(true);
        assert!(is_folder_rename_editing());
        set_folder_rename_editing(false);
        assert!(!is_folder_rename_editing());
    }
}
