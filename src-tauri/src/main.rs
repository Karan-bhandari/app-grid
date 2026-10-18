use clap::{Parser, Subcommand};
use std::fs;
use std::time::Instant;

mod app_scanner;
mod icon_cache;
pub mod ipc_commands;
pub mod key_intercept;
mod layout_store;
mod legacy_import;
pub mod panel_manager;
pub mod spotlight_hook;

use layout_store::{LayoutConfig, LayoutItem, LayoutStore, LoadOutcome};

#[derive(Parser, Debug)]
#[command(name = "app-grid", about = "App Grid for macOS Spotlight")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List all pages, items, and folders in the current layout
    List,
    /// Folder management commands
    Folder {
        #[command(subcommand)]
        command: FolderCommands,
    },
    /// Move an item (app or folder) to a specific page and slot index
    Move {
        /// Bundle ID (for an app) or folder ID
        item_id: String,
        /// Target page index (0-based)
        #[arg(long)]
        page: usize,
        /// Target slot index on the page (0-based)
        #[arg(long)]
        index: usize,
    },
    /// Import layout from Apple's legacy Launchpad SQLite database
    ImportLegacy,
    /// Benchmark app scanning and 256x256 icon cache warming
    ScanBenchmark {
        /// Clear the icon cache directory before benchmarking
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Subcommand, Debug)]
enum FolderCommands {
    /// Create a new folder containing the specified apps
    Create {
        /// Name of the new folder
        name: String,
        /// List of bundle IDs or app names to group into the folder
        #[arg(required = true)]
        apps: Vec<String>,
    },
}

pub fn load_or_init_layout() -> anyhow::Result<LayoutConfig> {
    let store = LayoutStore::new(LayoutStore::default_path());
    let outcome = store.load()?;
    let installed = app_scanner::scan_all();

    let (mut config, was_missing_or_recovered) = match outcome {
        LoadOutcome::Loaded(c) => (c, false),
        LoadOutcome::Recovered(c) => {
            tracing::warn!("Recovered layout from corrupted file or default");
            (c, true)
        }
        LoadOutcome::Missing => {
            tracing::info!("Layout config missing, attempting legacy import or initial scan");
            let lookup = |bid: &str| {
                installed
                    .iter()
                    .find(|a| a.bundle_id == bid || a.name.eq_ignore_ascii_case(bid))
                    .map(|a| a.path.clone())
            };
            let imported = if let Some(legacy_path) = legacy_import::legacy_db_path() {
                match legacy_import::import_from_path(&legacy_path, &lookup) {
                    Ok(Some(c)) => Some(c),
                    _ => None,
                }
            } else {
                None
            };

            let initial = imported.unwrap_or_else(|| layout_store::initial_from_apps(&installed));
            (initial, true)
        }
    };

    let report = layout_store::reconcile(&mut config, &installed);
    let changed = was_missing_or_recovered
        || !report.added.is_empty()
        || !report.removed.is_empty()
        || !report.pruned_folders.is_empty();

    if changed {
        store.save(&config)?;
    }

    Ok(config)
}

fn run_gui() {
    use tauri::{Emitter, Manager};

    let has_perms = spotlight_hook::check_accessibility_permissions(true);
    if !has_perms {
        tracing::warn!("Accessibility not granted. Enable App Grid in System Settings -> Privacy & Security -> Device Control and Data Access.");
    }

    let store = LayoutStore::new(LayoutStore::default_path());
    let layout_config = load_or_init_layout().unwrap_or_else(|_| LayoutConfig::default());
    let icon_cache = icon_cache::IconCache::new().expect("Failed to initialize icon cache");

    let app_state = ipc_commands::AppState {
        layout: std::sync::Mutex::new(layout_config),
        store,
        icon_cache,
    };

    tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            ipc_commands::get_layout,
            ipc_commands::get_icon_path,
            ipc_commands::launch_app,
            ipc_commands::save_layout,
            ipc_commands::move_item,
            ipc_commands::create_folder,
            ipc_commands::rename_folder,
            ipc_commands::set_editing_folder,
            ipc_commands::hide_overlay,
        ])
        .setup(|app| {
            let window = app
                .get_webview_window("app-grid-overlay")
                .expect("app-grid-overlay window not found");

            // Configure non-activating NSPanel flags
            panel_manager::setup_non_activating_panel(&window);

            // Apply Liquid Glass backdrop material
            let _ = window_vibrancy::apply_vibrancy(
                &window,
                window_vibrancy::NSVisualEffectMaterial::HudWindow,
                None,
                None,
            );

            // Setup Key Interception (CGEventTap) for folder renaming
            let win_keys = window.clone();
            let _ = key_intercept::init_key_intercept(move |key_event| {
                let _ = win_keys.emit("folder_key_event", key_event);
            });

            // Setup Spotlight Accessibility Hook with automatic retry and 30ms polling synchronization
            let win_spot = window.clone();
            std::thread::spawn(move || {
                let last_state: std::sync::Arc<
                    std::sync::Mutex<Option<spotlight_hook::SpotlightEvent>>,
                > = std::sync::Arc::new(std::sync::Mutex::new(None));

                let dispatch_event = {
                    let win = win_spot.clone();
                    let last_state = last_state.clone();
                    std::sync::Arc::new(move |event: spotlight_hook::SpotlightEvent| {
                        {
                            let mut guard = last_state.lock().unwrap();
                            if guard.as_ref() == Some(&event) {
                                return;
                            }
                            *guard = Some(event.clone());
                        }

                        let win_main = win.clone();
                        let win_inner = win.clone();
                        let _ = win_main.run_on_main_thread(move || {
                            let primary_height = panel_manager::get_primary_screen_height();
                            match event {
                                spotlight_hook::SpotlightEvent::Show { bounds, layer } => {
                                    key_intercept::set_overlay_visible(true);
                                    let panel_frame = panel_manager::quartz_to_appkit_panel_frame(
                                        bounds,
                                        68.0,
                                        primary_height,
                                    );
                                    let round_top_corners =
                                        bounds.height <= 120.0 || bounds.width < 740.0;
                                    panel_manager::set_panel_frame_and_level(
                                        &win_inner,
                                        panel_frame,
                                        layer + 1,
                                        round_top_corners,
                                    );
                                    panel_manager::order_front_non_activating(&win_inner);
                                }
                                spotlight_hook::SpotlightEvent::Hide => {
                                    key_intercept::set_overlay_visible(false);
                                    panel_manager::order_out_panel(&win_inner);
                                    let _ = win_inner.emit("overlay_hidden", ());
                                }
                                spotlight_hook::SpotlightEvent::MoveOrResize { bounds, layer } => {
                                    let panel_frame = panel_manager::quartz_to_appkit_panel_frame(
                                        bounds,
                                        68.0,
                                        primary_height,
                                    );
                                    let round_top_corners =
                                        bounds.height <= 120.0 || bounds.width < 740.0;
                                    panel_manager::set_panel_frame_and_level(
                                        &win_inner,
                                        panel_frame,
                                        layer + 1,
                                        round_top_corners,
                                    );
                                }
                            }
                        });
                    })
                };

                let cb_dispatch = dispatch_event.clone();
                let mut hook = spotlight_hook::SpotlightHook::new(move |event| {
                    cb_dispatch(event);
                });

                loop {
                    if !key_intercept::is_key_intercept_initialized() {
                        let win_keys_retry = win_spot.clone();
                        let _ = key_intercept::init_key_intercept(move |key_event| {
                            let _ = win_keys_retry.emit("folder_key_event", key_event);
                        });
                    }

                    if let Some(pid) = spotlight_hook::find_spotlight_pid() {
                        if hook.current_pid() != Some(pid) {
                            if let Err(e) = hook.attach() {
                                tracing::debug!("Hook attach error: {}", e);
                            }
                        }
                        if let Some(state) = hook.query_current_state() {
                            dispatch_event(state);
                        }
                    } else if hook.current_pid().is_some() {
                        hook.detach();
                        dispatch_event(spotlight_hook::SpotlightEvent::Hide);
                    }

                    let sleep_ms = if key_intercept::IS_OVERLAY_VISIBLE
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        40
                    } else {
                        250
                    };
                    std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                }
            });

            // Setup filesystem live watcher for application directory changes
            let win_watcher = window.clone();
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let default_dirs = app_scanner::default_app_dirs();
                let _watcher = app_scanner::watch(&default_dirs, move |_watch_event| {
                    let installed = app_scanner::scan_all();
                    let mut updated_layout = None;
                    {
                        let state = handle.state::<ipc_commands::AppState>();
                        if let Ok(mut layout) = state.layout.lock() {
                            let report = layout_store::reconcile(&mut layout, &installed);
                            if !report.added.is_empty()
                                || !report.removed.is_empty()
                                || !report.pruned_folders.is_empty()
                            {
                                let _ = state.store.save(&layout);
                                updated_layout = Some(layout.clone());
                            }
                        };
                    }

                    if let Some(l) = updated_layout {
                        let _ = win_watcher.emit("layout_updated", &l);
                    }
                });
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error running tauri app");
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::List) => {
            let config = load_or_init_layout()?;
            println!(
                "Spotlight Launchpad Layout (v{}, {} pages):",
                config.version,
                config.pages.len()
            );
            for page in &config.pages {
                println!("Page {} ({} items):", page.page_index, page.items.len());
                for (idx, item) in page.items.iter().enumerate() {
                    match item {
                        LayoutItem::App(app) => {
                            println!(
                                "  [{:>2}] App: {} ({}) - {}",
                                idx,
                                app.name,
                                app.bundle_id,
                                app.path.display()
                            );
                        }
                        LayoutItem::Folder(folder) => {
                            println!(
                                "  [{:>2}] Folder: '{}' (id: {}, {} apps):",
                                idx,
                                folder.name,
                                folder.id,
                                folder.apps.len()
                            );
                            for app in &folder.apps {
                                println!("       - {} ({})", app.name, app.bundle_id);
                            }
                        }
                    }
                }
            }
        }
        Some(Commands::Folder {
            command: FolderCommands::Create { name, apps },
        }) => {
            let store = LayoutStore::new(LayoutStore::default_path());
            let mut config = load_or_init_layout()?;
            let folder_id = config.create_folder(&name, &apps)?;
            store.save(&config)?;
            println!(
                "Created folder '{}' with id '{}' containing {} apps.",
                name,
                folder_id,
                apps.len()
            );
        }
        Some(Commands::Move {
            item_id,
            page,
            index,
        }) => {
            let store = LayoutStore::new(LayoutStore::default_path());
            let mut config = load_or_init_layout()?;
            config.move_item(&item_id, page, index)?;
            store.save(&config)?;
            println!(
                "Moved item '{}' to page {}, index {}.",
                item_id, page, index
            );
        }
        Some(Commands::ImportLegacy) => {
            let store = LayoutStore::new(LayoutStore::default_path());
            let installed = app_scanner::scan_all();
            let lookup = |bid: &str| {
                installed
                    .iter()
                    .find(|a| a.bundle_id == bid || a.name.eq_ignore_ascii_case(bid))
                    .map(|a| a.path.clone())
            };

            let mut config = if let Some(legacy_path) = legacy_import::legacy_db_path() {
                println!(
                    "Found legacy Launchpad database at: {}",
                    legacy_path.display()
                );
                match legacy_import::import_from_path(&legacy_path, &lookup)? {
                    Some(c) => {
                        println!(
                            "Successfully parsed legacy database ({} pages).",
                            c.pages.len()
                        );
                        c
                    }
                    None => {
                        println!(
                            "Legacy database had no installed apps or could not be read; initializing from clean scan."
                        );
                        layout_store::initial_from_apps(&installed)
                    }
                }
            } else {
                println!("No legacy Launchpad database found; initializing from clean scan.");
                layout_store::initial_from_apps(&installed)
            };

            let report = layout_store::reconcile(&mut config, &installed);
            store.save(&config)?;
            println!(
                "Layout saved. Reconcile report: {} added, {} removed, {} folders pruned.",
                report.added.len(),
                report.removed.len(),
                report.pruned_folders.len()
            );
        }
        Some(Commands::ScanBenchmark { clear }) => {
            let cache_dir = dirs::cache_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join("com.personal.app-grid/icons");

            if clear && cache_dir.exists() {
                fs::remove_dir_all(&cache_dir)?;
                println!("Cleared icon cache at: {}", cache_dir.display());
            }

            // Cold run
            let cold_start = Instant::now();
            let cold_scan_start = Instant::now();
            let apps = app_scanner::scan_all();
            let cold_scan_ms = cold_scan_start.elapsed().as_millis();

            let icon_cache = icon_cache::IconCache::new()?;
            let cold_warm_start = Instant::now();
            let warm_results = icon_cache.warm(&apps);
            let cold_warm_ms = cold_warm_start.elapsed().as_millis();
            let cold_total_ms = cold_start.elapsed().as_millis();
            let success_count = warm_results.iter().filter(|r| r.is_ok()).count();

            println!(
                "Cold run: scanned {} apps in {}ms, warmed {} icons in {}ms (total: {}ms)",
                apps.len(),
                cold_scan_ms,
                success_count,
                cold_warm_ms,
                cold_total_ms
            );

            // Warm run
            let warm_start = Instant::now();
            let warm_scan_start = Instant::now();
            let apps_warm = app_scanner::scan_all();
            let warm_scan_ms = warm_scan_start.elapsed().as_millis();

            let warm_cache_start = Instant::now();
            let warm_results2 = icon_cache.warm(&apps_warm);
            let warm_cache_ms = warm_cache_start.elapsed().as_millis();
            let warm_total_ms = warm_start.elapsed().as_millis();
            let success_count2 = warm_results2.iter().filter(|r| r.is_ok()).count();

            println!(
                "Warm run: scanned {} apps in {}ms, loaded {} icons in {}ms (total: {}ms)",
                apps_warm.len(),
                warm_scan_ms,
                success_count2,
                warm_cache_ms,
                warm_total_ms
            );
        }
        None => {
            run_gui();
        }
    }

    Ok(())
}
