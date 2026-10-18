# App Grid

macOS utility that attaches a Launchpad-style multi-page application and folder grid directly below the native Spotlight search bar.

## Features

- **Seamless Spotlight Attachment**: Tracks `com.apple.Spotlight` / `com.apple.campo` in real time. Appears automatically below an empty Spotlight bar (`Cmd+Space`) and hides the instant you start typing a search query.
- **Zero Focus Stealing**: Runs as a non-activating `NSPanel` (`NSWindowStyleMaskNonactivatingPanel`). Spotlight keeps keyboard focus at all times so you can start typing a query at any moment without clicking back into the search bar.
- **Multi-Page Grid & Folders**: 7x5 paginated grid with drag-and-drop reordering, folder creation, inline folder renaming, and page dots.
- **Legacy Launchpad Import**: Automatically imports your existing page and folder arrangement from the macOS Dock Launchpad SQLite database on first run.
- **Live App Reconciliation**: Watches `/Applications`, `/System/Applications`, `/System/Applications/Utilities`, and `~/Applications` (including Apple Silicon iOS `WrappedBundle` apps). Newly installed apps are appended automatically; uninstalled apps are removed while preserving apps on unmounted external drives (`/Volumes/*`).
- **Native 256x256 Icon Cache**: Rasterizes high-DPI bundle icons off-thread via `NSWorkspace` and caches them on disk by `<sha256(path)>_<mtime>.png`.

## Usage & Controls

### Keyboard

- **`Cmd + Space`**: Open Spotlight with the App Grid overlay attached below.
- **`Arrow Keys` (`Left`, `Right`, `Up`, `Down`)**: Navigate apps and folders across the grid (wrapping across pages at row edges) or inside an open folder popover.
- **`Enter` / `Return`**: Launch the selected application or open the selected folder.
- **`Escape`**: Dismiss Spotlight and the overlay (while renaming a folder, cancels the rename instead).
- **`Cmd + 1`**: Show the App Grid overlay in Spotlight's Applications view.
- **`Cmd + 2` / `Cmd + 3` / `Cmd + 4`**: Hide the overlay when switching to Spotlight's Files, Actions, or Clipboard tabs.
- **Any Typing Key**: Clears grid selection and passes keystrokes directly to Spotlight's search field, hiding the grid until the search field is cleared.

### Mouse & Trackpad

- **Click App / Folder**: Launch an application or open a folder popover.
- **Two-Finger Swipe (Left / Right or Up / Down)**: Switch pages.
- **Drag to Slot Edge**: Reorder an item on the current page, or hold near the left/right screen edge to drag across pages.
- **Drag Over Center of Another Item**: Hold briefly over another app or folder to create a new folder or add the app into an existing folder.
- **Drag Out of Open Folder**: Drag an app outside the folder popover backdrop to move it back onto the main page grid.
- **Rename Folder**: Click the folder title inside an open folder popover, type the new name, and press `Enter` (or `Escape` to cancel).
- **`+ New Folder` Button**: Groups the first two standalone apps on the current page into a "New Folder" and opens it for renaming.
- **Native View Button**: Hides the overlay for the current Spotlight session so you can peek at Apple's native Spotlight view.

## Permissions

App Grid requires one macOS permission:

- **Location**: **System Settings -> Privacy & Security -> Device Control and Data Access**
- **Why it is needed**: `AXObserver` detects Spotlight's window position and search text state; `CGEventTap` intercepts arrow keys and `Enter` while Spotlight keeps keyboard focus.
- **Granting it**: On first launch, macOS displays a prompt opening this settings pane. Toggle **App Grid** on, then relaunch the app.
- **Troubleshooting**: If the overlay appears but arrow keys move Spotlight's own selection, macOS has not trusted the current binary. Remove **App Grid** from the list with `-`, re-add it with `+`, and relaunch.

## Installation & Build

### Prerequisites

- macOS 27.0+
- Rust (stable) & Cargo (`tauri-cli` v2.12+)
- Node.js 24+

### Build from Source

```bash
# 1. Install frontend dependencies
npm install

# 2. Build release bundle, sign, register with LaunchServices, and strip intermediate artifacts
./scripts/build-and-sign.sh

# 3. Launch the built app
open "src-tauri/target/release/bundle/macos/App Grid.app"
```

To install permanently, copy `src-tauri/target/release/bundle/macos/App Grid.app` to `/Applications/App Grid.app`.

## CLI Subcommands

The binary inside `App Grid.app/Contents/MacOS/app-grid` also provides headless layout management and benchmarking commands:

```bash
APP_BIN="src-tauri/target/release/bundle/macos/App Grid.app/Contents/MacOS/app-grid"

# Print all pages, slots, apps, and folders
"$APP_BIN" list

# Create a folder from bundle IDs or app names
"$APP_BIN" folder create "Utilities" com.apple.Terminal com.apple.ActivityMonitor

# Move an app (by bundle ID) or folder (by folder ID) to a specific page and 0-based slot index
"$APP_BIN" move com.apple.Safari --page 0 --index 0

# Re-import layout from macOS legacy Launchpad SQLite database
"$APP_BIN" import-legacy

# Benchmark app bundle scanning and cold/warm 256x256 icon cache rendering
"$APP_BIN" scan-benchmark --clear
```

## Configuration & Storage Paths

- **Layout Configuration**: `~/.config/app-grid/layout.json`
  Stores grid dimensions (`columns`, `rows`, `icon_size_px`, `header_offset_pt`, `drop_folder_delay_ms`), ordered pages/folders, and `hidden_bundle_ids` (bundle IDs to exclude from the grid).
- **Icon Cache**: `~/Library/Caches/com.personal.app-grid/icons/`
  Stores pre-rendered 256x256 RGBA PNG icons. Safe to delete at any time; icons are regenerated on demand.

## Development

```bash
# Run in development mode with hot reload
cargo tauri dev

# Run the 23-test Rust unit and integration suite
cargo test --manifest-path src-tauri/Cargo.toml

# Format Rust (cargo fmt) and TypeScript/CSS/HTML/JSON/Markdown (prettier)
npm run format
```

`./scripts/build-and-sign.sh` signs with the Keychain identity `SpotlightExtLocalDev` when present (falling back to ad-hoc). Use a stable signing identity so macOS keeps the Accessibility grant across rebuilds; ad-hoc signatures change every build and invalidate it.
