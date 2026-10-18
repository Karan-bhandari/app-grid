use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app_scanner::AppEntry;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub version: u32,
    pub grid_config: GridConfig,
    pub pages: Vec<Page>,
    pub hidden_bundle_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridConfig {
    pub columns: u32,
    pub rows: u32,
    pub icon_size_px: u32,
    pub header_offset_pt: f64,
    pub drop_folder_delay_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub page_index: usize,
    pub items: Vec<LayoutItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayoutItem {
    App(AppRef),
    Folder(Folder),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppRef {
    pub bundle_id: String,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub apps: Vec<AppRef>,
}

impl From<&AppEntry> for AppRef {
    fn from(entry: &AppEntry) -> Self {
        Self {
            bundle_id: entry.bundle_id.clone(),
            name: entry.name.clone(),
            path: entry.path.clone(),
        }
    }
}

impl From<AppEntry> for AppRef {
    fn from(entry: AppEntry) -> Self {
        Self {
            bundle_id: entry.bundle_id,
            name: entry.name,
            path: entry.path,
        }
    }
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            columns: 7,
            rows: 5,
            icon_size_px: 72,
            header_offset_pt: 68.0,
            drop_folder_delay_ms: 250,
        }
    }
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            version: 1,
            grid_config: GridConfig::default(),
            pages: Vec::new(),
            hidden_bundle_ids: Vec::new(),
        }
    }
}

pub struct LayoutStore {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoadOutcome {
    Loaded(LayoutConfig),
    Recovered(LayoutConfig),
    Missing,
}

impl LayoutStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config/app-grid/layout.json")
    }

    pub fn load(&self) -> anyhow::Result<LoadOutcome> {
        if !self.path.exists() {
            return Ok(LoadOutcome::Missing);
        }

        let content = match fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadOutcome::Missing);
            }
            Err(e) => return Err(e.into()),
        };

        match serde_json::from_str::<LayoutConfig>(&content) {
            Ok(config) => Ok(LoadOutcome::Loaded(config)),
            Err(parse_err) => {
                tracing::warn!(
                    "Failed to parse layout config at {:?}: {}. Recovering to default and backing up.",
                    self.path,
                    parse_err
                );
                let bak_path = self.path.with_file_name(format!(
                    "{}.bak",
                    self.path.file_name().unwrap_or_default().to_string_lossy()
                ));
                if let Err(copy_err) = fs::copy(&self.path, &bak_path) {
                    tracing::error!(
                        "Failed to copy corrupt layout file to {:?}: {}",
                        bak_path,
                        copy_err
                    );
                }
                Ok(LoadOutcome::Recovered(LayoutConfig::default()))
            }
        }
    }

    pub fn save(&self, config: &LayoutConfig) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = self.path.with_file_name(format!(
            "{}.tmp",
            self.path.file_name().unwrap_or_default().to_string_lossy()
        ));

        let json_str = serde_json::to_string_pretty(config)?;

        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            file.write_all(json_str.as_bytes())?;
            file.sync_all()?;
        }

        fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub pruned_folders: Vec<String>,
    pub retained_unmounted: Vec<String>,
}

fn is_on_unmounted_volume(path: &Path) -> bool {
    let mut components = path.components();
    if let (
        Some(std::path::Component::RootDir),
        Some(std::path::Component::Normal(vol_dir)),
        Some(std::path::Component::Normal(vol_name)),
    ) = (components.next(), components.next(), components.next())
    {
        if vol_dir == "Volumes" {
            let vol_root = PathBuf::from("/Volumes").join(vol_name);
            return !vol_root.exists();
        }
    }
    false
}

pub fn reconcile(config: &mut LayoutConfig, installed: &[AppEntry]) -> ReconcileReport {
    let mut report = ReconcileReport::default();
    let installed_paths: HashSet<&Path> = installed.iter().map(|e| e.path.as_path()).collect();
    let hidden_set: HashSet<&str> = config
        .hidden_bundle_ids
        .iter()
        .map(|s| s.as_str())
        .collect();

    // 1. Remove apps whose bundle path is no longer installed (top-level and inside folders)
    // Keep apps that are on an unmounted volume (/Volumes/<X> where /Volumes/<X> doesn't exist).
    // Skip hidden_bundle_ids (remove if present in layout).
    for page in &mut config.pages {
        page.items.retain_mut(|item| match item {
            LayoutItem::App(app) => {
                if hidden_set.contains(app.bundle_id.as_str()) {
                    report.removed.push(app.bundle_id.clone());
                    false
                } else if installed_paths.contains(app.path.as_path()) {
                    true
                } else if is_on_unmounted_volume(&app.path) {
                    report.retained_unmounted.push(app.bundle_id.clone());
                    true
                } else {
                    report.removed.push(app.bundle_id.clone());
                    false
                }
            }
            LayoutItem::Folder(folder) => {
                folder.apps.retain(|app| {
                    if hidden_set.contains(app.bundle_id.as_str()) {
                        report.removed.push(app.bundle_id.clone());
                        false
                    } else if installed_paths.contains(app.path.as_path()) {
                        true
                    } else if is_on_unmounted_volume(&app.path) {
                        report.retained_unmounted.push(app.bundle_id.clone());
                        true
                    } else {
                        report.removed.push(app.bundle_id.clone());
                        false
                    }
                });

                if folder.apps.is_empty() {
                    report.pruned_folders.push(folder.id.clone());
                    false
                } else {
                    true
                }
            }
        });
    }

    // Prune empty pages
    config.pages.retain(|page| !page.items.is_empty());

    // 2. Identify existing apps by path or bundle_id to determine what is NEW
    let mut existing_paths: HashSet<PathBuf> = HashSet::new();
    let mut existing_bundle_ids: HashSet<String> = HashSet::new();

    for page in &config.pages {
        for item in &page.items {
            match item {
                LayoutItem::App(app) => {
                    existing_paths.insert(app.path.clone());
                    existing_bundle_ids.insert(app.bundle_id.clone());
                }
                LayoutItem::Folder(folder) => {
                    for app in &folder.apps {
                        existing_paths.insert(app.path.clone());
                        existing_bundle_ids.insert(app.bundle_id.clone());
                    }
                }
            }
        }
    }

    // 3. Append NEW apps to first free slot on LAST page, creating new page when capacity is reached
    let page_capacity = (config.grid_config.columns * config.grid_config.rows) as usize;

    for entry in installed {
        if hidden_set.contains(entry.bundle_id.as_str()) {
            continue;
        }
        if existing_paths.contains(&entry.path) || existing_bundle_ids.contains(&entry.bundle_id) {
            continue;
        }

        existing_paths.insert(entry.path.clone());
        existing_bundle_ids.insert(entry.bundle_id.clone());
        report.added.push(entry.bundle_id.clone());

        if config.pages.is_empty() {
            config.pages.push(Page {
                page_index: 0,
                items: Vec::new(),
            });
        }

        let last_idx = config.pages.len() - 1;
        if config.pages[last_idx].items.len() >= page_capacity {
            let new_page_index = config.pages.len();
            config.pages.push(Page {
                page_index: new_page_index,
                items: vec![LayoutItem::App(AppRef::from(entry))],
            });
        } else {
            config.pages[last_idx]
                .items
                .push(LayoutItem::App(AppRef::from(entry)));
        }
    }

    // 4. Renumber page_index sequentially
    for (idx, page) in config.pages.iter_mut().enumerate() {
        page.page_index = idx;
    }

    report
}

pub fn initial_from_apps(apps: &[AppEntry]) -> LayoutConfig {
    let grid_config = GridConfig::default();
    let page_capacity = (grid_config.columns * grid_config.rows) as usize;

    let mut pages = Vec::new();
    if apps.is_empty() {
        pages.push(Page {
            page_index: 0,
            items: Vec::new(),
        });
    } else {
        for (page_idx, chunk) in apps.chunks(page_capacity).enumerate() {
            let items = chunk
                .iter()
                .map(|app| LayoutItem::App(AppRef::from(app)))
                .collect();
            pages.push(Page {
                page_index: page_idx,
                items,
            });
        }
    }

    LayoutConfig {
        version: 1,
        grid_config,
        pages,
        hidden_bundle_ids: Vec::new(),
    }
}

impl LayoutConfig {
    pub fn create_folder(
        &mut self,
        name: &str,
        bundle_ids_or_names: &[String],
    ) -> anyhow::Result<String> {
        if bundle_ids_or_names.is_empty() {
            anyhow::bail!("Cannot create folder with no apps specified");
        }

        let mut matched_apps: Vec<AppRef> = Vec::new();
        let mut first_pos: Option<(usize, usize)> = None;

        for target in bundle_ids_or_names {
            let mut found = false;
            'search: for (p_idx, page) in self.pages.iter_mut().enumerate() {
                for i_idx in 0..page.items.len() {
                    let is_app_match = match &page.items[i_idx] {
                        LayoutItem::App(app) => {
                            app.bundle_id == *target || app.name.eq_ignore_ascii_case(target)
                        }
                        _ => false,
                    };
                    if is_app_match {
                        if first_pos.is_none() {
                            first_pos = Some((p_idx, i_idx));
                        }
                        let item = page.items.remove(i_idx);
                        if let LayoutItem::App(a) = item {
                            matched_apps.push(a);
                        }
                        found = true;
                        break 'search;
                    }

                    if let LayoutItem::Folder(folder) = &mut page.items[i_idx] {
                        if let Some(pos) = folder.apps.iter().position(|a| {
                            a.bundle_id == *target || a.name.eq_ignore_ascii_case(target)
                        }) {
                            if first_pos.is_none() {
                                first_pos = Some((p_idx, i_idx));
                            }
                            let app = folder.apps.remove(pos);
                            matched_apps.push(app);
                            found = true;
                            break 'search;
                        }
                    }
                }
            }

            if !found {
                tracing::warn!("Target app '{}' not found in layout", target);
            }
        }

        if matched_apps.is_empty() {
            anyhow::bail!("None of the specified apps were found in the layout");
        }

        // Prune any folder that became empty
        for page in &mut self.pages {
            page.items.retain(|item| match item {
                LayoutItem::Folder(f) => !f.apps.is_empty(),
                _ => true,
            });
        }

        static FOLDER_COUNTER: AtomicU64 = AtomicU64::new(1);
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let n = FOLDER_COUNTER.fetch_add(1, Ordering::Relaxed);
        let folder_id = format!("folder_{}_{}", ms, n);

        let folder = Folder {
            id: folder_id.clone(),
            name: name.to_string(),
            apps: matched_apps,
        };

        if let Some((p_idx, i_idx)) = first_pos {
            if p_idx < self.pages.len() {
                let insert_idx = i_idx.min(self.pages[p_idx].items.len());
                self.pages[p_idx]
                    .items
                    .insert(insert_idx, LayoutItem::Folder(folder));
            } else if !self.pages.is_empty() {
                let last = self.pages.len() - 1;
                self.pages[last].items.push(LayoutItem::Folder(folder));
            } else {
                self.pages.push(Page {
                    page_index: 0,
                    items: vec![LayoutItem::Folder(folder)],
                });
            }
        } else {
            if self.pages.is_empty() {
                self.pages.push(Page {
                    page_index: 0,
                    items: Vec::new(),
                });
            }
            self.pages[0].items.push(LayoutItem::Folder(folder));
        }

        for (idx, page) in self.pages.iter_mut().enumerate() {
            page.page_index = idx;
        }

        Ok(folder_id)
    }

    pub fn move_item(
        &mut self,
        item_id: &str,
        target_page: usize,
        target_index: usize,
    ) -> anyhow::Result<()> {
        let mut extracted_item: Option<LayoutItem> = None;

        'find: for page in &mut self.pages {
            for i in 0..page.items.len() {
                let matches = match &page.items[i] {
                    LayoutItem::Folder(f) => f.id == item_id,
                    LayoutItem::App(a) => a.bundle_id == item_id,
                };
                if matches {
                    extracted_item = Some(page.items.remove(i));
                    break 'find;
                }

                if let LayoutItem::Folder(f) = &mut page.items[i] {
                    if let Some(pos) = f.apps.iter().position(|a| a.bundle_id == item_id) {
                        let app = f.apps.remove(pos);
                        extracted_item = Some(LayoutItem::App(app));
                        break 'find;
                    }
                }
            }
        }

        let item = match extracted_item {
            Some(it) => it,
            None => anyhow::bail!("Item with id '{}' not found", item_id),
        };

        // Prune any folder that became empty and auto-dissolve 1-app folders
        for page in &mut self.pages {
            page.items.retain(|item| match item {
                LayoutItem::Folder(f) => !f.apps.is_empty(),
                _ => true,
            });
        }
        self.flatten_single_app_folders();

        // Clamp or expand target_page
        if target_page == self.pages.len() {
            self.pages.push(Page {
                page_index: target_page,
                items: Vec::new(),
            });
        } else if target_page > self.pages.len() {
            while self.pages.len() <= target_page {
                let idx = self.pages.len();
                self.pages.push(Page {
                    page_index: idx,
                    items: Vec::new(),
                });
            }
        }

        let page_to_insert = &mut self.pages[target_page];
        let clamped_idx = target_index.min(page_to_insert.items.len());
        page_to_insert.items.insert(clamped_idx, item);

        for (idx, page) in self.pages.iter_mut().enumerate() {
            page.page_index = idx;
        }

        Ok(())
    }

    pub fn find_item(&self, item_id: &str) -> Option<(usize, usize, &LayoutItem)> {
        for (p_idx, page) in self.pages.iter().enumerate() {
            for (i_idx, item) in page.items.iter().enumerate() {
                match item {
                    LayoutItem::App(a) if a.bundle_id == item_id => {
                        return Some((p_idx, i_idx, item));
                    }
                    LayoutItem::Folder(f) if f.id == item_id => {
                        return Some((p_idx, i_idx, item));
                    }
                    LayoutItem::Folder(f) if f.apps.iter().any(|a| a.bundle_id == item_id) => {
                        return Some((p_idx, i_idx, item));
                    }
                    _ => {}
                }
            }
        }
        None
    }

    pub fn flatten_single_app_folders(&mut self) {
        for page in &mut self.pages {
            for item in &mut page.items {
                if let LayoutItem::Folder(folder) = item {
                    if folder.apps.len() == 1 {
                        let single_app = folder.apps.remove(0);
                        *item = LayoutItem::App(single_app);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_app(id: &str, name: &str, path_str: &str) -> AppEntry {
        AppEntry {
            bundle_id: id.to_string(),
            name: name.to_string(),
            path: PathBuf::from(path_str),
            mtime: 1000,
        }
    }

    #[test]
    fn test_json_roundtrip_spec_example() {
        let json_text = r#"{
  "version": 1,
  "grid_config": {
    "columns": 7,
    "rows": 5,
    "icon_size_px": 72,
    "header_offset_pt": 68.0,
    "drop_folder_delay_ms": 250
  },
  "pages": [
    {
      "page_index": 0,
      "items": [
        {
          "type": "app",
          "bundle_id": "com.apple.Safari",
          "name": "Safari",
          "path": "/Applications/Safari.app"
        },
        {
          "type": "folder",
          "id": "folder_dev_01",
          "name": "Development",
          "apps": [
            {
              "bundle_id": "com.mitchellh.ghostty",
              "name": "Ghostty",
              "path": "/Applications/Ghostty.app"
            },
            {
              "bundle_id": "com.microsoft.VSCode",
              "name": "Visual Studio Code",
              "path": "/Applications/Visual Studio Code.app"
            }
          ]
        }
      ]
    }
  ],
  "hidden_bundle_ids": [
    "com.apple.Uninstall"
  ]
}"#;

        let parsed: LayoutConfig = serde_json::from_str(json_text).expect("Deserialization failed");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.grid_config.columns, 7);
        assert_eq!(parsed.grid_config.rows, 5);
        assert_eq!(parsed.grid_config.icon_size_px, 72);
        assert_eq!(parsed.grid_config.header_offset_pt, 68.0);
        assert_eq!(parsed.grid_config.drop_folder_delay_ms, 250);
        assert_eq!(parsed.pages.len(), 1);
        assert_eq!(parsed.pages[0].page_index, 0);
        assert_eq!(parsed.pages[0].items.len(), 2);
        assert_eq!(parsed.hidden_bundle_ids, vec!["com.apple.Uninstall"]);

        let serialized = serde_json::to_string_pretty(&parsed).expect("Serialization failed");
        let roundtripped: LayoutConfig =
            serde_json::from_str(&serialized).expect("Round-trip deserialization failed");
        assert_eq!(parsed, roundtripped);
    }

    #[test]
    fn ut03_reconcile_appends_to_last_page() {
        // UT-03: Page 0 full (35/35), Page 1 has 3 folders, add 2 apps ->
        // they land at end of Page 1, all prior items/indices unchanged.
        let mut config = LayoutConfig::default();
        config.grid_config = GridConfig {
            columns: 7,
            rows: 5,
            icon_size_px: 72,
            header_offset_pt: 68.0,
            drop_folder_delay_ms: 250,
        };

        let mut page0_items = Vec::new();
        let mut installed = Vec::new();

        for i in 0..35 {
            let app = make_app(
                &format!("com.app.p0_{}", i),
                &format!("App 0-{}", i),
                &format!("/Applications/App0_{}.app", i),
            );
            installed.push(app.clone());
            page0_items.push(LayoutItem::App(AppRef::from(&app)));
        }

        let mut page1_items = Vec::new();
        for f in 0..3 {
            let app_f = make_app(
                &format!("com.app.f_{}", f),
                &format!("App F-{}", f),
                &format!("/Applications/AppF_{}.app", f),
            );
            installed.push(app_f.clone());
            page1_items.push(LayoutItem::Folder(Folder {
                id: format!("folder_{}", f),
                name: format!("Folder {}", f),
                apps: vec![AppRef::from(&app_f)],
            }));
        }

        config.pages.push(Page {
            page_index: 0,
            items: page0_items.clone(),
        });
        config.pages.push(Page {
            page_index: 1,
            items: page1_items.clone(),
        });

        // 2 new apps
        let new_app_1 = make_app("com.app.new1", "New App 1", "/Applications/NewApp1.app");
        let new_app_2 = make_app("com.app.new2", "New App 2", "/Applications/NewApp2.app");
        installed.push(new_app_1.clone());
        installed.push(new_app_2.clone());

        let report = reconcile(&mut config, &installed);
        assert_eq!(report.added, vec!["com.app.new1", "com.app.new2"]);
        assert!(report.removed.is_empty());

        assert_eq!(config.pages.len(), 2);
        assert_eq!(config.pages[0].page_index, 0);
        assert_eq!(config.pages[0].items.len(), 35);
        assert_eq!(config.pages[0].items, page0_items);

        assert_eq!(config.pages[1].page_index, 1);
        assert_eq!(config.pages[1].items.len(), 5);
        assert_eq!(config.pages[1].items[0..3], page1_items[..]);
        assert_eq!(
            config.pages[1].items[3],
            LayoutItem::App(AppRef::from(&new_app_1))
        );
        assert_eq!(
            config.pages[1].items[4],
            LayoutItem::App(AppRef::from(&new_app_2))
        );

        // Idempotence test
        let report_again = reconcile(&mut config, &installed);
        assert!(report_again.added.is_empty());
        assert!(report_again.removed.is_empty());
        assert_eq!(config.pages.len(), 2);
        assert_eq!(config.pages[1].items.len(), 5);
    }

    #[test]
    fn ut04_folder_auto_cleanup_on_app_deletion() {
        // UT-04: Folder Auto-Cleanup on App Deletion: Remove the last app inside 'folder_test' via reconciliation.
        // Expected Assertion: 'folder_test' is automatically pruned from layout.json with no orphan references.
        let mut config = LayoutConfig::default();
        let app_surviving = make_app(
            "com.app.surviving",
            "Surviving",
            "/Applications/Surviving.app",
        );
        let app_doomed = make_app("com.app.doomed", "Doomed", "/Applications/Doomed.app");

        config.pages.push(Page {
            page_index: 0,
            items: vec![
                LayoutItem::App(AppRef::from(&app_surviving)),
                LayoutItem::Folder(Folder {
                    id: "folder_test".to_string(),
                    name: "Test Folder".to_string(),
                    apps: vec![AppRef::from(&app_doomed)],
                }),
            ],
        });

        // App doomed is uninstalled
        let installed = vec![app_surviving.clone()];

        let report = reconcile(&mut config, &installed);
        assert_eq!(report.removed, vec!["com.app.doomed"]);
        assert_eq!(report.pruned_folders, vec!["folder_test"]);

        assert_eq!(config.pages.len(), 1);
        assert_eq!(config.pages[0].items.len(), 1);
        assert_eq!(
            config.pages[0].items[0],
            LayoutItem::App(AppRef::from(&app_surviving))
        );

        // Check that folder_test is gone
        assert!(config.find_item("folder_test").is_none());
        assert!(config.find_item("com.app.doomed").is_none());
    }

    #[test]
    fn ut05_atomic_save_and_corrupt_json_recovery() {
        // UT-05: Atomic Save & Corrupt JSON Recovery: Simulate malformed layout.json.
        // Backup layout.json.bak is created and state gracefully recovers without panicking.
        let dir = tempdir().expect("Failed to create tempdir");
        let layout_file = dir.path().join("layout.json");

        // 1. Missing file -> LoadOutcome::Missing
        let store = LayoutStore::new(layout_file.clone());
        let outcome = store.load().expect("Load failed on missing file");
        assert_eq!(outcome, LoadOutcome::Missing);

        // 2. Corrupt file -> LoadOutcome::Recovered(default) and layout.json.bak created
        fs::write(&layout_file, b"{ invalid json content: true,,, }").expect("Write corrupt file");
        let outcome = store.load().expect("Load failed on corrupt file");
        assert_eq!(outcome, LoadOutcome::Recovered(LayoutConfig::default()));

        let bak_file = dir.path().join("layout.json.bak");
        assert!(bak_file.exists(), "layout.json.bak was not created");
        let bak_content = fs::read_to_string(&bak_file).expect("Failed to read bak file");
        assert_eq!(bak_content, "{ invalid json content: true,,, }");

        // 3. Save works atomically and load succeeds
        let mut valid_config = LayoutConfig::default();
        valid_config
            .hidden_bundle_ids
            .push("com.hidden".to_string());
        store.save(&valid_config).expect("Save failed");

        let loaded = store.load().expect("Load failed on valid saved file");
        assert_eq!(loaded, LoadOutcome::Loaded(valid_config));
    }

    #[test]
    fn test_unmounted_volume_retention() {
        let mut config = LayoutConfig::default();
        let regular_app = make_app("com.app.regular", "Regular", "/Applications/Regular.app");
        let unmounted_app = make_app(
            "com.app.ext",
            "Ext App",
            "/Volumes/NonExistentDisk_12345/Apps/Ext.app",
        );

        config.pages.push(Page {
            page_index: 0,
            items: vec![
                LayoutItem::App(AppRef::from(&regular_app)),
                LayoutItem::App(AppRef::from(&unmounted_app)),
            ],
        });

        // Scan only finds regular app
        let installed = vec![regular_app.clone()];

        let report = reconcile(&mut config, &installed);
        assert_eq!(report.retained_unmounted, vec!["com.app.ext"]);
        assert_eq!(config.pages[0].items.len(), 2);
    }

    #[test]
    fn test_create_folder_and_move_item() {
        let mut config = LayoutConfig::default();
        let app1 = make_app("com.app.one", "App One", "/Applications/AppOne.app");
        let app2 = make_app("com.app.two", "App Two", "/Applications/AppTwo.app");
        let app3 = make_app("com.app.three", "App Three", "/Applications/AppThree.app");

        config.pages.push(Page {
            page_index: 0,
            items: vec![
                LayoutItem::App(AppRef::from(&app1)),
                LayoutItem::App(AppRef::from(&app2)),
                LayoutItem::App(AppRef::from(&app3)),
            ],
        });

        let folder_id = config
            .create_folder(
                "My Tools",
                &["com.app.one".to_string(), "App Two".to_string()],
            )
            .expect("create_folder failed");

        assert_eq!(config.pages[0].items.len(), 2);
        match &config.pages[0].items[0] {
            LayoutItem::Folder(f) => {
                assert_eq!(f.id, folder_id);
                assert_eq!(f.name, "My Tools");
                assert_eq!(f.apps.len(), 2);
            }
            _ => panic!("Expected folder at slot 0"),
        }

        // Move App Three to Page 1, slot 0
        config
            .move_item("com.app.three", 1, 0)
            .expect("move_item failed");
        assert_eq!(config.pages.len(), 2);
        assert_eq!(config.pages[0].items.len(), 1);
        assert_eq!(config.pages[1].items.len(), 1);
        assert_eq!(
            config.pages[1].items[0],
            LayoutItem::App(AppRef::from(&app3))
        );
    }
}
