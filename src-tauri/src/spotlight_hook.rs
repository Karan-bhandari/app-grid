use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use accessibility_sys::{
    kAXChildrenAttribute, kAXFocusedUIElementChangedNotification, kAXMainWindowChangedNotification,
    kAXMovedNotification, kAXResizedNotification, kAXRoleAttribute, kAXTrustedCheckOptionPrompt,
    kAXUIElementDestroyedNotification, kAXValueAttribute, kAXValueChangedNotification,
    kAXWindowCreatedNotification, kAXWindowsAttribute, pid_t, AXError, AXIsProcessTrusted,
    AXIsProcessTrustedWithOptions, AXObserverAddNotification, AXObserverCallback, AXObserverCreate,
    AXObserverGetRunLoopSource, AXObserverRef, AXObserverRemoveNotification,
    AXUIElementCopyAttributeValue, AXUIElementCreateApplication, AXUIElementRef,
};

use core_foundation::base::{kCFAllocatorDefault, CFRelease, CFTypeRef, TCFType};
use core_foundation::boolean::kCFBooleanTrue;
use core_foundation::dictionary::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionaryCreate,
};
use core_foundation::runloop::{
    kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopRemoveSource,
};
use core_foundation::string::{CFString, CFStringGetLength, CFStringRef};

use objc2_app_kit::NSRunningApplication;
use objc2_foundation::NSString;
use tracing::{debug, info};

use crate::panel_manager::Rect;

/// Events emitted by the Spotlight Accessibility observer state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum SpotlightEvent {
    /// Spotlight is visible with an empty search field -> show custom Launchpad grid
    Show { bounds: Rect, layer: i32 },
    /// Spotlight was closed, hidden, or user started typing a search query -> hide grid immediately
    Hide,
    /// Spotlight was dragged or resized -> update overlay frame at 120Hz
    MoveOrResize { bounds: Rect, layer: i32 },
}

pub type SpotlightEventHandler = Arc<dyn Fn(SpotlightEvent) + Send + Sync + 'static>;

/// Checks whether this process is granted macOS Accessibility (TCC) privileges.
/// If `prompt` is true and permission is not yet granted, macOS displays the System Settings prompt.
pub fn check_accessibility_permissions(prompt: bool) -> bool {
    unsafe {
        if !prompt {
            return AXIsProcessTrusted();
        }

        let keys = [kAXTrustedCheckOptionPrompt as *const c_void];
        let values = [kCFBooleanTrue as *const c_void];
        let options = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );

        let trusted = AXIsProcessTrustedWithOptions(options as _);
        if !options.is_null() {
            CFRelease(options as _);
        }
        trusted
    }
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

static CACHED_SPOTLIGHT_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Locates the running Spotlight / Siri process PID via Bundle ID `com.apple.campo`, `com.apple.Spotlight`, or `pgrep`.
pub fn find_spotlight_pid() -> Option<pid_t> {
    let cached = CACHED_SPOTLIGHT_PID.load(std::sync::atomic::Ordering::Relaxed);
    if cached > 0 && unsafe { kill(cached, 0) == 0 } {
        return Some(cached);
    }

    let bundle_ids = ["com.apple.campo", "com.apple.Spotlight"];
    for id_str in bundle_ids {
        let bundle_id = NSString::from_str(id_str);
        let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle_id);
        for app in apps.iter() {
            let pid = app.processIdentifier();
            if pid > 0 {
                CACHED_SPOTLIGHT_PID.store(pid, std::sync::atomic::Ordering::Relaxed);
                return Some(pid as pid_t);
            }
        }
    }

    // Fallback: check via pgrep
    for name in &["Siri AI", "Spotlight", "Siri"] {
        if let Ok(output) = std::process::Command::new("pgrep")
            .arg("-x")
            .arg(name)
            .output()
        {
            if let Ok(s) = std::str::from_utf8(&output.stdout) {
                if let Some(line) = s.lines().next() {
                    if let Ok(pid) = line.trim().parse::<pid_t>() {
                        if pid > 0 {
                            CACHED_SPOTLIGHT_PID.store(pid, std::sync::atomic::Ordering::Relaxed);
                            return Some(pid);
                        }
                    }
                }
            }
        }
    }

    CACHED_SPOTLIGHT_PID.store(0, std::sync::atomic::Ordering::Relaxed);
    None
}

extern "C" {
    fn CGEventCreateKeyboardEvent(
        source: *const std::ffi::c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut std::ffi::c_void;
    fn CGEventPostToPid(pid: pid_t, event: *mut std::ffi::c_void);
}

static SUPPRESS_UNTIL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn suppress_overlay_for_ms(duration_ms: u64) {
    SUPPRESS_UNTIL_MS.store(now_ms() + duration_ms, Ordering::SeqCst);
}

unsafe fn post_escape_to_pid(pid: pid_t) {
    let key_down = CGEventCreateKeyboardEvent(std::ptr::null(), 53, true);
    if !key_down.is_null() {
        CGEventPostToPid(pid, key_down);
        CFRelease(key_down);
    }
    let key_up = CGEventCreateKeyboardEvent(std::ptr::null(), 53, false);
    if !key_up.is_null() {
        CGEventPostToPid(pid, key_up);
        CFRelease(key_up);
    }
}

/// Dismisses native Spotlight/Launchpad immediately by posting Escape (key code 53) directly to its PID,
/// suppressing overlay popups during the transition and retrying on a background thread to ensure complete closure.
pub fn dismiss_native_spotlight_if_active() {
    suppress_overlay_for_ms(1200);

    if let Some(pid) = find_spotlight_pid() {
        if crate::panel_manager::get_active_spotlight_window(pid).is_some() {
            unsafe {
                post_escape_to_pid(pid);
            }
        }

        std::thread::spawn(move || {
            for _ in 0..8 {
                std::thread::sleep(std::time::Duration::from_millis(55));
                if let Some(cur_pid) = find_spotlight_pid() {
                    if crate::panel_manager::get_active_spotlight_window(cur_pid).is_some() {
                        unsafe {
                            post_escape_to_pid(cur_pid);
                        }
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        });
    }
}

/// Helper to safely copy an AX attribute from an AXUIElementRef.
unsafe fn copy_ax_attribute(element: AXUIElementRef, attribute: CFStringRef) -> Option<CFTypeRef> {
    let mut value_ref: CFTypeRef = ptr::null_mut();
    let err = AXUIElementCopyAttributeValue(element, attribute, &mut value_ref);
    if err == 0 && !value_ref.is_null() {
        Some(value_ref)
    } else {
        None
    }
}

use std::sync::atomic::{AtomicBool, Ordering};

pub static USER_HIDDEN_FOR_SESSION: AtomicBool = AtomicBool::new(false);

pub fn set_user_hidden_for_session(hidden: bool) {
    USER_HIDDEN_FOR_SESSION.store(hidden, Ordering::SeqCst);
}

/// Checks whether a single AXUIElement is an AXTextField/AXSearchField and returns Some(len > 0) if so.
unsafe fn check_search_field_element(element: AXUIElementRef) -> Option<bool> {
    if element.is_null() {
        return None;
    }
    let role_attr = CFString::from_static_string(kAXRoleAttribute);
    let role_ref = copy_ax_attribute(element, role_attr.as_concrete_TypeRef() as _)?;
    let role_str = role_ref as CFStringRef;
    let role_rust = if !role_str.is_null() {
        CFString::wrap_under_get_rule(role_str).to_string()
    } else {
        String::new()
    };
    CFRelease(role_ref);

    if role_rust == "AXTextField" || role_rust == "AXSearchField" {
        let val_attr = CFString::from_static_string(kAXValueAttribute);
        if let Some(val_ref) = copy_ax_attribute(element, val_attr.as_concrete_TypeRef() as _) {
            let val_str = val_ref as CFStringRef;
            let len = if !val_str.is_null() {
                CFStringGetLength(val_str)
            } else {
                0
            };
            CFRelease(val_ref);
            return Some(len > 0);
        }
        return Some(false);
    }
    None
}

/// Recursively inspects the UI hierarchy of Spotlight to check if any search text field
/// contains non-empty text.
/// Returns true if an active search query is detected (length > 0).
///
/// # Safety
///
/// The caller must ensure that `element` is a valid `AXUIElementRef` or null pointer.
pub unsafe fn has_active_search_query(element: AXUIElementRef, depth: usize) -> bool {
    if depth > 4 || element.is_null() {
        return false;
    }

    if let Some(has_text) = check_search_field_element(element) {
        return has_text;
    }

    if depth == 0 {
        // O(1) fast path: check focused UI element directly on the application
        let focused_attr = CFString::from_static_string("AXFocusedUIElement");
        if let Some(focused_ref) =
            copy_ax_attribute(element, focused_attr.as_concrete_TypeRef() as _)
        {
            let res = check_search_field_element(focused_ref as AXUIElementRef);
            CFRelease(focused_ref);
            if let Some(has_text) = res {
                return has_text;
            }
        }

        // Also inspect AXWindows on the application element (top bar only)
        let windows_attr = CFString::from_static_string(kAXWindowsAttribute);
        if let Some(windows_ref) =
            copy_ax_attribute(element, windows_attr.as_concrete_TypeRef() as _)
        {
            use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
            let count = CFArrayGetCount(windows_ref as CFArrayRef).min(4);
            for i in 0..count {
                let win = CFArrayGetValueAtIndex(windows_ref as CFArrayRef, i) as AXUIElementRef;
                if !win.is_null() && has_active_search_query(win, depth + 1) {
                    CFRelease(windows_ref);
                    return true;
                }
            }
            CFRelease(windows_ref);
        }
    }

    // Inspect first few children (search bar is always at the top of the window hierarchy)
    let children_attr = CFString::from_static_string(kAXChildrenAttribute);
    if let Some(children_ref) = copy_ax_attribute(element, children_attr.as_concrete_TypeRef() as _)
    {
        use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
        let count = CFArrayGetCount(children_ref as CFArrayRef).min(6);
        for i in 0..count {
            let child = CFArrayGetValueAtIndex(children_ref as CFArrayRef, i) as AXUIElementRef;
            if !child.is_null() && has_active_search_query(child, depth + 1) {
                CFRelease(children_ref);
                return true;
            }
        }
        CFRelease(children_ref);
    }

    false
}

static LAST_SHOWN_BOUNDS: std::sync::Mutex<Option<(Rect, i32)>> = std::sync::Mutex::new(None);

/// Evaluates the current Spotlight state using both CGWindowList (for window presence & bounds)
/// and Accessibility (for search field text), honoring the session hide flag and active mouse interaction.
pub fn evaluate_spotlight_state(pid: pid_t) -> SpotlightEvent {
    if now_ms() < SUPPRESS_UNTIL_MS.load(Ordering::SeqCst) {
        if let Ok(mut g) = LAST_SHOWN_BOUNDS.lock() {
            *g = None;
        }
        return SpotlightEvent::Hide;
    }

    if let Some((bounds, layer)) = crate::panel_manager::get_active_spotlight_window(pid) {
        if USER_HIDDEN_FOR_SESSION.load(Ordering::SeqCst) {
            if let Ok(mut g) = LAST_SHOWN_BOUNDS.lock() {
                *g = None;
            }
            return SpotlightEvent::Hide;
        }

        let has_query = unsafe {
            let app_element = AXUIElementCreateApplication(pid);
            if !app_element.is_null() {
                let h = has_active_search_query(app_element, 0);
                CFRelease(app_element as _);
                h
            } else {
                false
            }
        };

        if has_query {
            if let Ok(mut g) = LAST_SHOWN_BOUNDS.lock() {
                *g = None;
            }
            SpotlightEvent::Hide
        } else {
            if let Ok(mut g) = LAST_SHOWN_BOUNDS.lock() {
                *g = Some((bounds, layer));
            }
            SpotlightEvent::Show { bounds, layer }
        }
    } else {
        if USER_HIDDEN_FOR_SESSION.load(Ordering::SeqCst) {
            if let Ok(mut g) = LAST_SHOWN_BOUNDS.lock() {
                *g = None;
            }
            USER_HIDDEN_FOR_SESSION.store(false, Ordering::SeqCst);
            crate::key_intercept::set_folder_rename_editing(false);
            return SpotlightEvent::Hide;
        }

        let prev_bounds = LAST_SHOWN_BOUNDS.lock().ok().and_then(|g| *g);
        if let Some((bounds, layer)) = prev_bounds {
            let primary_height = crate::panel_manager::get_primary_screen_height();
            let panel_frame =
                crate::panel_manager::quartz_to_appkit_panel_frame(bounds, 68.0, primary_height);
            let (in_panel, is_button_down) =
                crate::panel_manager::get_mouse_interaction_state(panel_frame);
            if !in_panel && is_button_down {
                crate::key_intercept::set_folder_rename_editing(false);
            } else if in_panel || crate::key_intercept::is_folder_rename_editing() {
                return SpotlightEvent::Show { bounds, layer };
            }
            if let Ok(mut g) = LAST_SHOWN_BOUNDS.lock() {
                *g = None;
            }
        }

        USER_HIDDEN_FOR_SESSION.store(false, Ordering::SeqCst);
        crate::key_intercept::set_folder_rename_editing(false);
        SpotlightEvent::Hide
    }
}

struct ObserverState {
    observer: AXObserverRef,
    app_element: AXUIElementRef,
    source: core_foundation::runloop::CFRunLoopSourceRef,
}

static NOTIFICATIONS: &[&str] = &[
    kAXWindowCreatedNotification,
    kAXUIElementDestroyedNotification,
    kAXMovedNotification,
    kAXResizedNotification,
    kAXValueChangedNotification,
    kAXFocusedUIElementChangedNotification,
    kAXMainWindowChangedNotification,
];

extern "C" fn observer_callback(
    _observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
) {
    if refcon.is_null() || element.is_null() {
        return;
    }

    let handler_arc = unsafe { &*(refcon as *const SpotlightEventHandler) };
    let notif_name = unsafe { CFString::wrap_under_get_rule(notification).to_string() };

    debug!("Received Spotlight AX notification: {}", notif_name);

    if let Some(pid) = find_spotlight_pid() {
        let state = evaluate_spotlight_state(pid);
        if (notif_name == kAXMovedNotification || notif_name == kAXResizedNotification)
            && matches!(state, SpotlightEvent::Show { .. })
        {
            if let SpotlightEvent::Show { bounds, layer } = state {
                handler_arc(SpotlightEvent::MoveOrResize { bounds, layer });
                return;
            }
        }
        handler_arc(state);
    } else {
        handler_arc(SpotlightEvent::Hide);
    }
}

/// Attaches an AXObserver to Spotlight and delivers events through the provided callback.
pub struct SpotlightHook {
    state: Option<ObserverState>,
    handler: SpotlightEventHandler,
    current_pid: Option<pid_t>,
}

impl SpotlightHook {
    pub fn new<F: Fn(SpotlightEvent) + Send + Sync + 'static>(handler: F) -> Self {
        Self {
            state: None,
            handler: Arc::new(handler),
            current_pid: None,
        }
    }

    /// Attempts to attach the observer to Spotlight.app.
    pub fn attach(&mut self) -> anyhow::Result<()> {
        let pid = find_spotlight_pid()
            .ok_or_else(|| anyhow::anyhow!("Spotlight.app is not currently running"))?;

        if let Some(cur_pid) = self.current_pid {
            if cur_pid == pid && self.state.is_some() {
                return Ok(());
            }
            self.detach();
        }

        info!("Attaching AXObserver to Spotlight.app (PID: {})", pid);

        unsafe {
            let app_element = AXUIElementCreateApplication(pid);
            if app_element.is_null() {
                anyhow::bail!("Failed to create AXUIElement for Spotlight PID {}", pid);
            }

            let mut observer: AXObserverRef = ptr::null_mut();
            let err: AXError =
                AXObserverCreate(pid, observer_callback as AXObserverCallback, &mut observer);

            if err != 0 || observer.is_null() {
                CFRelease(app_element as _);
                anyhow::bail!("AXObserverCreate failed with error code: {}", err);
            }

            let handler_box = Box::new(self.handler.clone());
            let refcon = Box::into_raw(handler_box) as *mut c_void;

            for &notif in NOTIFICATIONS {
                let cf_notif = CFString::from_static_string(notif);
                let add_err = AXObserverAddNotification(
                    observer,
                    app_element,
                    cf_notif.as_concrete_TypeRef() as _,
                    refcon,
                );
                if add_err != 0 {
                    debug!(
                        "Could not subscribe to notification {} (error: {})",
                        notif, add_err
                    );
                }
            }

            let source = AXObserverGetRunLoopSource(observer);
            if source.is_null() {
                CFRelease(observer as _);
                CFRelease(app_element as _);
                anyhow::bail!("Failed to get RunLoopSource from AXObserver");
            }

            CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);

            self.state = Some(ObserverState {
                observer,
                app_element,
                source,
            });
            self.current_pid = Some(pid);

            info!("Successfully attached AXObserver to Spotlight.app");
            Ok(())
        }
    }

    /// Detaches and frees AXObserver resources.
    pub fn detach(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe {
                CFRunLoopRemoveSource(CFRunLoopGetMain(), state.source, kCFRunLoopCommonModes);
                for &notif in NOTIFICATIONS {
                    let cf_notif = CFString::from_static_string(notif);
                    let _ = AXObserverRemoveNotification(
                        state.observer,
                        state.app_element,
                        cf_notif.as_concrete_TypeRef() as _,
                    );
                }
                CFRelease(state.observer as _);
                CFRelease(state.app_element as _);
            }
            self.current_pid = None;
            info!("Detached AXObserver from Spotlight.app");
        }
    }

    /// Checks the current Spotlight window state immediately on demand.
    pub fn query_current_state(&self) -> Option<SpotlightEvent> {
        let pid = self.current_pid.or_else(find_spotlight_pid)?;
        Some(evaluate_spotlight_state(pid))
    }

    /// Returns the currently hooked PID, if any.
    pub fn current_pid(&self) -> Option<pid_t> {
        self.current_pid
    }
}

unsafe impl Send for SpotlightHook {}

impl Drop for SpotlightHook {
    fn drop(&mut self) {
        self.detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_spotlight_pid() {
        if let Some(pid) = find_spotlight_pid() {
            assert!(pid > 0);
        }
    }

    #[test]
    fn test_check_accessibility_permissions() {
        // Verify checking permissions without prompt returns a boolean without crashing
        let _trusted = check_accessibility_permissions(false);
    }
}
