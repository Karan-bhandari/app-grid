use crate::layout_store::{AppRef, Folder, GridConfig, LayoutConfig, LayoutItem, Page};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub fn legacy_db_path() -> Option<PathBuf> {
    // 1. Try getconf DARWIN_USER_DIR
    if let Ok(output) = std::process::Command::new("getconf")
        .arg("DARWIN_USER_DIR")
        .output()
    {
        if output.status.success() {
            let user_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !user_dir.is_empty() {
                let base = if user_dir.starts_with("/private") {
                    PathBuf::from(&user_dir)
                } else {
                    PathBuf::from("/private").join(user_dir.trim_start_matches('/'))
                };
                let candidate = base.join("com.apple.dock.launchpad/db/db");
                if candidate.exists() {
                    return Some(candidate);
                }
                let candidate_noprivate =
                    PathBuf::from(&user_dir).join("com.apple.dock.launchpad/db/db");
                if candidate_noprivate.exists() {
                    return Some(candidate_noprivate);
                }
            }
        }
    }

    // 2. Fallback to TMPDIR/../0/com.apple.dock.launchpad/db/db
    if let Ok(tmp) = std::env::var("TMPDIR") {
        let candidate = PathBuf::from(tmp).join("../0/com.apple.dock.launchpad/db/db");
        if let Ok(canon) = candidate.canonicalize() {
            if canon.exists() {
                return Some(canon);
            }
        } else if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

pub fn import_from_path(
    path: &Path,
    lookup: &dyn Fn(&str) -> Option<PathBuf>,
) -> anyhow::Result<Option<LayoutConfig>> {
    if !path.exists() {
        return Ok(None);
    }

    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;

    let config = import_from_connection(&conn, lookup)?;

    let total_apps: usize = config
        .pages
        .iter()
        .map(|p| {
            p.items
                .iter()
                .map(|item| match item {
                    LayoutItem::App(_) => 1,
                    LayoutItem::Folder(f) => f.apps.len(),
                })
                .sum::<usize>()
        })
        .sum();

    if total_apps == 0 {
        Ok(None)
    } else {
        Ok(Some(config))
    }
}

pub fn import_from_connection(
    conn: &Connection,
    lookup: &dyn Fn(&str) -> Option<PathBuf>,
) -> anyhow::Result<LayoutConfig> {
    // 1. Identify root item rowid (type 1, preferably uuid = 'ROOTPAGE')
    let root_rowid: i64 = conn
        .query_row(
            "SELECT rowid FROM items WHERE (uuid = 'ROOTPAGE' OR parent_id = 0) AND type = 1 ORDER BY (uuid = 'ROOTPAGE') DESC, rowid ASC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);

    // 2. Fetch top-level pages (type = 3, parent_id = root_rowid), ordered by ordering ASC
    let mut page_stmt = conn.prepare(
        "SELECT rowid FROM items WHERE parent_id = ?1 AND type = 3 ORDER BY ordering ASC",
    )?;

    let page_rowids: Vec<i64> = page_stmt
        .query_map([root_rowid], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut item_stmt =
        conn.prepare("SELECT rowid, type FROM items WHERE parent_id = ?1 ORDER BY ordering ASC")?;

    let mut app_stmt = conn.prepare("SELECT title, bundleid FROM apps WHERE item_id = ?1")?;

    let mut group_stmt = conn.prepare("SELECT title FROM groups WHERE item_id = ?1")?;

    let mut pages = Vec::new();

    for page_rowid in page_rowids {
        let child_rows: Vec<(i64, i32)> = item_stmt
            .query_map([page_rowid], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();

        let mut page_items = Vec::new();

        for (child_rowid, item_type) in child_rows {
            match item_type {
                4 => {
                    // App directly on page
                    if let Ok((title, Some(bid))) = app_stmt.query_row([child_rowid], |row| {
                        let t: Option<String> = row.get(0)?;
                        let b: Option<String> = row.get(1)?;
                        Ok((t, b))
                    }) {
                        let bid = bid.trim().to_string();
                        if !bid.is_empty() {
                            if let Some(path) = lookup(&bid) {
                                let name =
                                    title.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
                                        path.file_stem()
                                            .map(|s| s.to_string_lossy().to_string())
                                            .unwrap_or_else(|| bid.clone())
                                    });
                                page_items.push(LayoutItem::App(AppRef {
                                    bundle_id: bid,
                                    name,
                                    path,
                                }));
                            }
                        }
                    }
                }
                2 => {
                    // Folder on page
                    let folder_title: String = group_stmt
                        .query_row([child_rowid], |row| {
                            let t: Option<String> = row.get(0)?;
                            Ok(t.filter(|s| !s.trim().is_empty())
                                .unwrap_or_else(|| "Folder".to_string()))
                        })
                        .unwrap_or_else(|_| "Folder".to_string());

                    // Folder children: either apps directly (type 4) or via nested folder pages (type 3)
                    let folder_children: Vec<(i64, i32)> = item_stmt
                        .query_map([child_rowid], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .filter_map(|r| r.ok())
                        .collect();

                    let mut folder_apps = Vec::new();

                    for (f_child_rowid, f_child_type) in folder_children {
                        if f_child_type == 4 {
                            // Direct app inside folder
                            if let Ok((title, Some(bid))) =
                                app_stmt.query_row([f_child_rowid], |row| {
                                    let t: Option<String> = row.get(0)?;
                                    let b: Option<String> = row.get(1)?;
                                    Ok((t, b))
                                })
                            {
                                let bid = bid.trim().to_string();
                                if !bid.is_empty() {
                                    if let Some(path) = lookup(&bid) {
                                        let name = title
                                            .filter(|s| !s.trim().is_empty())
                                            .unwrap_or_else(|| {
                                                path.file_stem()
                                                    .map(|s| s.to_string_lossy().to_string())
                                                    .unwrap_or_else(|| bid.clone())
                                            });
                                        folder_apps.push(AppRef {
                                            bundle_id: bid,
                                            name,
                                            path,
                                        });
                                    }
                                }
                            }
                        } else if f_child_type == 3 {
                            // Nested folder page: its children are apps
                            let folder_page_children: Vec<(i64, i32)> = item_stmt
                                .query_map([f_child_rowid], |row| Ok((row.get(0)?, row.get(1)?)))?
                                .filter_map(|r| r.ok())
                                .collect();

                            for (fp_child_rowid, fp_child_type) in folder_page_children {
                                if fp_child_type == 4 {
                                    if let Ok((title, Some(bid))) =
                                        app_stmt.query_row([fp_child_rowid], |row| {
                                            let t: Option<String> = row.get(0)?;
                                            let b: Option<String> = row.get(1)?;
                                            Ok((t, b))
                                        })
                                    {
                                        let bid = bid.trim().to_string();
                                        if !bid.is_empty() {
                                            if let Some(path) = lookup(&bid) {
                                                let name = title
                                                    .filter(|s| !s.trim().is_empty())
                                                    .unwrap_or_else(|| {
                                                        path.file_stem()
                                                            .map(|s| {
                                                                s.to_string_lossy().to_string()
                                                            })
                                                            .unwrap_or_else(|| bid.clone())
                                                    });
                                                folder_apps.push(AppRef {
                                                    bundle_id: bid,
                                                    name,
                                                    path,
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if !folder_apps.is_empty() {
                        let folder_id = format!("folder_legacy_{}", child_rowid);
                        page_items.push(LayoutItem::Folder(Folder {
                            id: folder_id,
                            name: folder_title,
                            apps: folder_apps,
                        }));
                    }
                }
                _ => {}
            }
        }

        if !page_items.is_empty() {
            pages.push(Page {
                page_index: 0,
                items: page_items,
            });
        }
    }

    for (idx, page) in pages.iter_mut().enumerate() {
        page.page_index = idx;
    }

    Ok(LayoutConfig {
        version: 1,
        grid_config: GridConfig::default(),
        pages,
        hidden_bundle_ids: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;

    #[test]
    fn ut06_mock_legacy_sqlite_import() {
        // UT-06: Mock Legacy SQLite Import: Create an in-memory SQLite DB matching
        // Apple's items/apps/groups schema with 2 pages and 2 nested folders.
        // Importer outputs a valid LayoutConfig preserving exact folder names and item ordering.
        let conn = Connection::open_in_memory().expect("Failed to open in-memory sqlite");

        // Create Apple's schema
        conn.execute_batch(
            r#"
            CREATE TABLE items (
                rowid INTEGER PRIMARY KEY,
                uuid VARCHAR,
                flags INTEGER,
                type INTEGER,
                parent_id INTEGER NOT NULL,
                ordering INTEGER
            );
            CREATE TABLE apps (
                item_id INTEGER PRIMARY KEY,
                title VARCHAR,
                bundleid VARCHAR,
                storeid VARCHAR,
                category_id INTEGER,
                moddate REAL,
                bookmark BLOB
            );
            CREATE TABLE groups (
                item_id INTEGER PRIMARY KEY,
                category_id INTEGER,
                title VARCHAR
            );
            "#,
        )
        .expect("Failed to create tables");

        // Insert Root item
        conn.execute(
            "INSERT INTO items (rowid, uuid, type, parent_id, ordering) VALUES (1, 'ROOTPAGE', 1, 0, 0)",
            [],
        )
        .expect("Insert root");

        // Page 1 (rowid 2, ordering 0) and Page 2 (rowid 3, ordering 1)
        conn.execute(
            "INSERT INTO items (rowid, uuid, type, parent_id, ordering) VALUES (2, 'page1', 3, 1, 0)",
            [],
        )
        .expect("Insert page 1");
        conn.execute(
            "INSERT INTO items (rowid, uuid, type, parent_id, ordering) VALUES (3, 'page2', 3, 1, 1)",
            [],
        )
        .expect("Insert page 2");

        // On Page 1:
        // Item 0: App "Safari" (rowid 10, ordering 0)
        conn.execute(
            "INSERT INTO items (rowid, type, parent_id, ordering) VALUES (10, 4, 2, 0)",
            [],
        )
        .expect("Insert Safari item");
        conn.execute(
            "INSERT INTO apps (item_id, title, bundleid) VALUES (10, 'Safari', 'com.apple.Safari')",
            [],
        )
        .expect("Insert Safari app");

        // Item 1: Folder "Development" (rowid 20, ordering 1)
        conn.execute(
            "INSERT INTO items (rowid, type, parent_id, ordering) VALUES (20, 2, 2, 1)",
            [],
        )
        .expect("Insert Development folder item");
        conn.execute(
            "INSERT INTO groups (item_id, title) VALUES (20, 'Development')",
            [],
        )
        .expect("Insert Development group");

        // Folder 1 page (rowid 21, ordering 0)
        conn.execute(
            "INSERT INTO items (rowid, type, parent_id, ordering) VALUES (21, 3, 20, 0)",
            [],
        )
        .expect("Insert Development folder page");

        // App inside Development: Ghostty (rowid 22, ordering 0)
        conn.execute(
            "INSERT INTO items (rowid, type, parent_id, ordering) VALUES (22, 4, 21, 0)",
            [],
        )
        .expect("Insert Ghostty item");
        conn.execute(
            "INSERT INTO apps (item_id, title, bundleid) VALUES (22, 'Ghostty', 'com.mitchellh.ghostty')",
            [],
        )
        .expect("Insert Ghostty app");

        // App inside Development: VSCode (rowid 23, ordering 1)
        conn.execute(
            "INSERT INTO items (rowid, type, parent_id, ordering) VALUES (23, 4, 21, 1)",
            [],
        )
        .expect("Insert VSCode item");
        conn.execute(
            "INSERT INTO apps (item_id, title, bundleid) VALUES (23, 'Visual Studio Code', 'com.microsoft.VSCode')",
            [],
        )
        .expect("Insert VSCode app");

        // On Page 2:
        // Item 0: Folder "Utilities" (rowid 30, ordering 0)
        conn.execute(
            "INSERT INTO items (rowid, type, parent_id, ordering) VALUES (30, 2, 3, 0)",
            [],
        )
        .expect("Insert Utilities folder item");
        conn.execute(
            "INSERT INTO groups (item_id, title) VALUES (30, 'Utilities')",
            [],
        )
        .expect("Insert Utilities group");

        // Folder 2 page (rowid 31, ordering 0)
        conn.execute(
            "INSERT INTO items (rowid, type, parent_id, ordering) VALUES (31, 3, 30, 0)",
            [],
        )
        .expect("Insert Utilities folder page");

        // App inside Utilities: Terminal (rowid 32, ordering 0)
        conn.execute(
            "INSERT INTO items (rowid, type, parent_id, ordering) VALUES (32, 4, 31, 0)",
            [],
        )
        .expect("Insert Terminal item");
        conn.execute(
            "INSERT INTO apps (item_id, title, bundleid) VALUES (32, 'Terminal', 'com.apple.Terminal')",
            [],
        )
        .expect("Insert Terminal app");

        // Item 1: App "Notes" (rowid 40, ordering 1)
        conn.execute(
            "INSERT INTO items (rowid, type, parent_id, ordering) VALUES (40, 4, 3, 1)",
            [],
        )
        .expect("Insert Notes item");
        conn.execute(
            "INSERT INTO apps (item_id, title, bundleid) VALUES (40, 'Notes', 'com.apple.Notes')",
            [],
        )
        .expect("Insert Notes app");

        // Lookup function: all apps installed
        let lookup = |bid: &str| match bid {
            "com.apple.Safari" => Some(PathBuf::from("/Applications/Safari.app")),
            "com.mitchellh.ghostty" => Some(PathBuf::from("/Applications/Ghostty.app")),
            "com.microsoft.VSCode" => Some(PathBuf::from("/Applications/Visual Studio Code.app")),
            "com.apple.Terminal" => {
                Some(PathBuf::from("/System/Applications/Utilities/Terminal.app"))
            }
            "com.apple.Notes" => Some(PathBuf::from("/System/Applications/Notes.app")),
            _ => None,
        };

        let config = import_from_connection(&conn, &lookup).expect("Import failed");

        assert_eq!(config.pages.len(), 2, "Expected exactly 2 pages");
        assert_eq!(config.pages[0].page_index, 0);
        assert_eq!(config.pages[1].page_index, 1);

        // Page 0 assertions
        assert_eq!(config.pages[0].items.len(), 2);
        match &config.pages[0].items[0] {
            LayoutItem::App(app) => {
                assert_eq!(app.bundle_id, "com.apple.Safari");
                assert_eq!(app.name, "Safari");
                assert_eq!(app.path, PathBuf::from("/Applications/Safari.app"));
            }
            _ => panic!("Expected Safari app at page 0, item 0"),
        }

        match &config.pages[0].items[1] {
            LayoutItem::Folder(folder) => {
                assert_eq!(folder.name, "Development");
                assert_eq!(folder.apps.len(), 2);
                assert_eq!(folder.apps[0].bundle_id, "com.mitchellh.ghostty");
                assert_eq!(folder.apps[0].name, "Ghostty");
                assert_eq!(folder.apps[1].bundle_id, "com.microsoft.VSCode");
                assert_eq!(folder.apps[1].name, "Visual Studio Code");
            }
            _ => panic!("Expected Development folder at page 0, item 1"),
        }

        // Page 1 assertions
        assert_eq!(config.pages[1].items.len(), 2);
        match &config.pages[1].items[0] {
            LayoutItem::Folder(folder) => {
                assert_eq!(folder.name, "Utilities");
                assert_eq!(folder.apps.len(), 1);
                assert_eq!(folder.apps[0].bundle_id, "com.apple.Terminal");
                assert_eq!(folder.apps[0].name, "Terminal");
            }
            _ => panic!("Expected Utilities folder at page 1, item 0"),
        }

        match &config.pages[1].items[1] {
            LayoutItem::App(app) => {
                assert_eq!(app.bundle_id, "com.apple.Notes");
                assert_eq!(app.name, "Notes");
                assert_eq!(app.path, PathBuf::from("/System/Applications/Notes.app"));
            }
            _ => panic!("Expected Notes app at page 1, item 1"),
        }
    }
}
