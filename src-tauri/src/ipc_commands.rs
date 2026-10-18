use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};
use tracing::{error, info};

use crate::app_scanner::AppEntry;
use crate::icon_cache::IconCache;
use crate::key_intercept::set_folder_rename_editing;
use crate::layout_store::{LayoutConfig, LayoutItem, LayoutStore};
use crate::panel_manager::order_out_panel;

pub struct AppState {
    pub layout: Mutex<LayoutConfig>,
    pub store: LayoutStore,
    pub icon_cache: IconCache,
}

#[tauri::command]
pub fn get_layout(state: State<'_, AppState>) -> Result<LayoutConfig, String> {
    let layout = state.layout.lock().map_err(|e| e.to_string())?;
    Ok(layout.clone())
}

#[tauri::command]
pub fn get_icon_path(
    state: State<'_, AppState>,
    bundle_id: String,
    path: String,
) -> Result<String, String> {
    let app_path = PathBuf::from(&path);
    let mtime = std::fs::metadata(&app_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let app = AppEntry {
        bundle_id,
        name: String::new(),
        path: app_path,
        mtime,
    };

    let cached = state
        .icon_cache
        .get_or_create(&app)
        .map_err(|e| format!("Failed to get or create icon: {:#}", e))?;

    Ok(cached.to_string_lossy().to_string())
}

#[tauri::command]
pub fn launch_app(app_handle: AppHandle, path: String) -> Result<(), String> {
    info!("Launching app: {}", path);

    // 1. Hide overlay panel immediately
    crate::spotlight_hook::set_user_hidden_for_session(true);
    crate::key_intercept::set_overlay_visible(false);
    set_folder_rename_editing(false);
    if let Some(window) = app_handle.get_webview_window("app-grid-overlay") {
        order_out_panel(&window);
    }

    // 2. Dismiss native Spotlight/Launchpad underneath if still visible
    crate::spotlight_hook::dismiss_native_spotlight_if_active();

    // 3. Launch and activate target app in foreground via /usr/bin/open with NSWorkspace fallback
    let open_status = std::process::Command::new("/usr/bin/open")
        .arg(&path)
        .status();

    match open_status {
        Ok(status) if status.success() => Ok(()),
        _ => {
            unsafe {
                use objc2_app_kit::NSWorkspace;
                use objc2_foundation::{NSString, NSURL};

                let ns_path = NSString::from_str(&path);
                let url = NSURL::fileURLWithPath(&ns_path);
                let workspace = NSWorkspace::sharedWorkspace();
                let success: bool = objc2::msg_send![&*workspace, openURL: &*url];
                if !success {
                    error!(
                        "Both /usr/bin/open and NSWorkspace failed to open URL for: {}",
                        path
                    );
                    return Err(format!("Failed to open {}", path));
                }
            }
            Ok(())
        }
    }
}

#[tauri::command]
pub fn save_layout(
    state: State<'_, AppState>,
    layout: LayoutConfig,
) -> Result<LayoutConfig, String> {
    let mut guard = state.layout.lock().map_err(|e| e.to_string())?;
    *guard = layout;
    state
        .store
        .save(&guard)
        .map_err(|e| format!("Failed to save layout: {:#}", e))?;
    Ok(guard.clone())
}

#[tauri::command]
pub fn move_item(
    state: State<'_, AppState>,
    item_id: String,
    target_page: usize,
    target_index: usize,
) -> Result<LayoutConfig, String> {
    let mut layout = state.layout.lock().map_err(|e| e.to_string())?;
    layout
        .move_item(&item_id, target_page, target_index)
        .map_err(|e| e.to_string())?;

    state
        .store
        .save(&layout)
        .map_err(|e| format!("Failed to save layout: {:#}", e))?;

    Ok(layout.clone())
}

#[tauri::command]
pub fn create_folder(
    state: State<'_, AppState>,
    name: String,
    app_ids: Vec<String>,
) -> Result<LayoutConfig, String> {
    let mut layout = state.layout.lock().map_err(|e| e.to_string())?;
    layout
        .create_folder(&name, &app_ids)
        .map_err(|e| e.to_string())?;

    state
        .store
        .save(&layout)
        .map_err(|e| format!("Failed to save layout: {:#}", e))?;

    Ok(layout.clone())
}

#[tauri::command]
pub fn rename_folder(
    state: State<'_, AppState>,
    folder_id: String,
    new_name: String,
) -> Result<LayoutConfig, String> {
    let mut layout = state.layout.lock().map_err(|e| e.to_string())?;

    let mut found = false;
    for page in &mut layout.pages {
        for item in &mut page.items {
            if let LayoutItem::Folder(f) = item {
                if f.id == folder_id {
                    f.name = new_name.trim().to_string();
                    found = true;
                    break;
                }
            }
        }
        if found {
            break;
        }
    }

    if !found {
        return Err(format!("Folder with ID '{}' not found", folder_id));
    }

    state
        .store
        .save(&layout)
        .map_err(|e| format!("Failed to save layout: {:#}", e))?;

    Ok(layout.clone())
}

#[tauri::command]
pub fn set_editing_folder(editing: bool) {
    set_folder_rename_editing(editing);
}

#[tauri::command]
pub fn hide_overlay(app_handle: AppHandle) {
    crate::spotlight_hook::set_user_hidden_for_session(true);
    crate::key_intercept::set_overlay_visible(false);
    set_folder_rename_editing(false);
    if let Some(window) = app_handle.get_webview_window("app-grid-overlay") {
        order_out_panel(&window);
    }
}
