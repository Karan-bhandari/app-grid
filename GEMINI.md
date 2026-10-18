# GEMINI.md — App Grid

## Project Overview

App Grid is a macOS menu-bar utility (`LSUIElement`) built with Tauri v2 (`2.12`), Rust (`objc2` `0.6`), and vanilla TypeScript (`7.0`) / Vite (`8.3`). It monitors the native macOS Spotlight window and attaches a non-activating `NSPanel` below Spotlight to display an interactive, multi-page application and folder grid.

## Architecture

- **`src-tauri/src/main.rs`**: Application entrypoint, CLI subcommands (`list`, `folder create`, `move`, `import-legacy`, `scan-benchmark`), tray menu setup, state management (`AppState`), filesystem watcher orchestration, and Spotlight event dispatch.
- **`src-tauri/src/spotlight_hook.rs`**: Observes `com.apple.Spotlight` / `com.apple.campo` via macOS Accessibility API (`AXObserver`) and `CGWindowListCopyWindowInfo` on a dedicated `CFRunLoop` thread. Emits `SpotlightEvent::{Show, Hide, MoveOrResize, SearchQuery}` including window layer metadata.
- **`src-tauri/src/panel_manager.rs`**: Subclasses `NSPanel` (`AppGridNonActivatingPanel`) with `NSWindowStyleMaskNonactivatingPanel`, `CanJoinAllSpaces`, `FullScreenAuxiliary`, and dynamic corner-radius masking, positioning the overlay in AppKit coordinates below Spotlight.
- **`src-tauri/src/key_intercept.rs`**: Global `CGEventTap` (`kCGHIDEventTap`) that intercepts arrow keys, `Enter`, `Escape`, and `Tab` when the grid is active without stealing keyboard focus from Spotlight.
- **`src-tauri/src/app_scanner.rs`**: Scans `/Applications`, `/System/Applications`, `/System/Applications/Utilities`, `/System/Cryptexes/App/System/Applications`, and `~/Applications` for `.app` bundles (including iOS `WrappedBundle` apps on Apple Silicon), parses `Info.plist`, and watches directories via `notify` (`8.2`).
- **`src-tauri/src/icon_cache.rs`**: Rasterizes 256x256 RGBA PNG icons off-thread using `NSWorkspace::iconForFile` and `NSBitmapImageRep` into `~/Library/Caches/com.personal.app-grid/icons/` keyed by `<sha256(path)>_<mtime>.png`.
- **`src-tauri/src/layout_store.rs`**: Atomically persists pages, apps, and folders to `~/.config/app-grid/layout.json`, recovers from corrupted JSON, retains unmounted `/Volumes/*` apps, and reconciles installed/removed applications.
- **`src-tauri/src/legacy_import.rs`**: Reads the legacy macOS Dock Launchpad SQLite database (`rusqlite` `0.40`) to import existing app and folder arrangements.
- **`src-tauri/src/ipc_commands.rs`**: Tauri IPC command handlers (`get_layout`, `save_layout`, `get_icon_path`, `launch_app`, `move_item`, `create_folder`, `rename_folder`, `set_editing_folder`, `hide_overlay`).
- **`src-ui/main.ts` & `src-ui/styles.css`**: Frontend rendering, synchronous in-memory icon URL cache, drag-and-drop reordering, folder creation/popover/rename UI, search filtering, and keyboard highlight state.

## Key Constraints & Conventions

1. **Never Steal Focus from Spotlight**: The overlay window must remain a non-activating `NSPanel` (`AppGridNonActivatingPanel`). Do not call `set_focus()` on the main window during normal Spotlight tracking, or Spotlight will immediately dismiss itself.
2. **Code Signing & Bundle Identity**:
   - `src-tauri/Info.plist` sets `CFBundleName` and `CFBundleDisplayName` to `App Grid` and `CFBundleIdentifier` to `com.personal.spotlight-launchpad`.
   - Local release builds must be signed with the persistent Keychain identity `"SpotlightExtLocalDev"` and `--identifier "com.personal.spotlight-launchpad"` (`./scripts/build-and-sign.sh`) and registered via `lsregister -f` so macOS TCC (`AXIsProcessTrusted` and `CGEventTapCreate`) retains Accessibility permissions across rebuilds.
   - On macOS 27+, the Accessibility pane is labeled **Privacy & Security -> Device Control and Data Access** (TCC service is unchanged). Use this name in user-facing text.
3. **Minimal Disk Footprint**: Release builds via `./scripts/build-and-sign.sh` strip intermediate `src-tauri/target/` build artifacts after bundling `App Grid.app`.
4. **Dependency Alignment**: Keep `objc2` (`0.6`), `objc2-foundation` (`0.3`), `objc2-app-kit` (`0.3`), `dirs` (`7.0`), and `sha2` (`0.10`) aligned with `tauri` (`2.12`) and `window-vibrancy` (`0.8`) so Cargo does not compile duplicate versions of macOS or crypto crates.
5. **Formatting & Testing**:
   - Run `./scripts/format.sh` (or `npm run format`) to format Rust (`cargo fmt`) and frontend/config/Markdown files (`prettier`).
   - Run `cargo test --manifest-path src-tauri/Cargo.toml` (23 unit/integration tests) before finalizing backend changes.
6. **CI/CD Workflows**:
   - `.github/workflows/ci.yml`: Runs `cargo fmt`, `prettier`, `cargo clippy`, `cargo test`, and `npm run build` on push/PR.
   - `.github/workflows/release.yml`: Triggered on `v*` tags using `tauri-apps/tauri-action@v1` to build and upload `.dmg` and `.app.tar.gz` packages to GitHub Releases.
