//! Application bundle discovery (spec §5.3, §7 Phase 1).
//!
//! Scans application directories to a maximum depth of 2 (so
//! `/Applications/Vendor/Foo.app` is found) without ever descending into a
//! `.app` bundle, and parses `Contents/Info.plist` (binary or XML).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use notify::{EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppEntry {
    /// CFBundleIdentifier; if missing, fallback `"path:<abs path>"`.
    pub bundle_id: String,
    /// CFBundleDisplayName -> CFBundleName -> dir stem.
    pub name: String,
    /// Absolute path to the .app bundle.
    pub path: PathBuf,
    /// Seconds since UNIX epoch: max(bundle dir mtime, Info.plist mtime).
    pub mtime: u64,
}

/// Maximum directory depth (relative to a scan root) at which bundles are found.
const MAX_DEPTH: usize = 2;

/// /Applications, /Applications/Utilities, /System/Applications, /System/Applications/Utilities, ~/Applications
pub fn default_app_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/Applications/Utilities"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("Applications"));
    }
    dirs
}

/// Scan given dirs, max depth 2, never descend into *.app. Dedup by canonical
/// path. Sorted by name (case-insensitive), then path.
pub fn scan_dirs(dirs: &[PathBuf]) -> Vec<AppEntry> {
    scan_dirs_with_depth(dirs, MAX_DEPTH)
}

fn scan_dirs_with_depth(dirs: &[PathBuf], max_depth: usize) -> Vec<AppEntry> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        collect(dir, 1, max_depth, &mut seen, &mut out);
    }
    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// Scans `default_app_dirs()` up to depth 3 so vendor suites (e.g.
/// `/Applications/Nikon Software/NX Studio/NX Studio.app` and
/// `/Applications/Utilities/Adobe Creative Cloud/ACC/Creative Cloud.app`) are discovered.
pub fn scan_all() -> Vec<AppEntry> {
    scan_dirs_with_depth(&default_app_dirs(), 3)
}

fn collect(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<AppEntry>,
) {
    let Ok(rd) = fs::read_dir(dir) else {
        return; // non-existent / unreadable dirs are skipped silently
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if hidden || !path.is_dir() {
            continue; // is_dir follows symlinks
        }
        if has_app_ext(&path) {
            let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen.contains(&canon) {
                continue;
            }
            if let Some(app) = parse_app_bundle(&path) {
                seen.insert(canon);
                out.push(app);
            }
        } else if depth < max_depth {
            collect(&path, depth + 1, max_depth, seen, out);
        }
    }
}

fn has_app_ext(path: &Path) -> bool {
    path.extension()
        .map(|e| e.eq_ignore_ascii_case("app"))
        .unwrap_or(false)
}

fn mtime_secs(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn load_plist_dict(plist_path: &Path) -> Option<plist::Dictionary> {
    if let Ok(val) = plist::Value::from_file(plist_path) {
        return val.into_dictionary();
    }
    // Fallback for XML Info.plist files with trailing NUL bytes (e.g. Steam app shortcuts)
    let raw = fs::read(plist_path).ok()?;
    let trimmed = raw.into_iter().take_while(|&b| b != 0).collect::<Vec<u8>>();
    plist::Value::from_reader(std::io::Cursor::new(trimmed))
        .ok()?
        .into_dictionary()
}

/// Parse a single bundle; None if not a valid .app (wrong extension, not a
/// directory, or no parseable Info.plist dictionary).
pub fn parse_app_bundle(path: &Path) -> Option<AppEntry> {
    if !has_app_ext(path) || !path.is_dir() {
        return None;
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    // Regular macOS bundles use Contents/Info.plist; iOS apps installed on
    // Apple Silicon use a `WrappedBundle` symlink to the inner bundle.
    let plist_path = ["Contents/Info.plist", "WrappedBundle/Info.plist"]
        .iter()
        .map(|p| abs.join(p))
        .find(|p| p.is_file())?;
    let dict = load_plist_dict(&plist_path)?;
    let get = |k: &str| {
        dict.get(k)
            .and_then(|v| v.as_string())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let stem = abs.file_stem()?.to_string_lossy().into_owned();
    let name = get("CFBundleDisplayName")
        .or_else(|| get("CFBundleName"))
        .unwrap_or(stem);
    let bundle_id = get("CFBundleIdentifier").unwrap_or_else(|| format!("path:{}", abs.display()));
    let mtime = mtime_secs(&abs)
        .unwrap_or(0)
        .max(mtime_secs(&plist_path).unwrap_or(0));
    Some(AppEntry {
        bundle_id,
        name,
        path: abs,
        mtime,
    })
}

// ---------------------------------------------------------------------------
// Filesystem watcher
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A bundle appeared or changed (e.g. app update → new mtime).
    Installed(AppEntry),
    /// A previously known bundle disappeared.
    Removed(PathBuf),
}

/// Quiet period per bundle before its final state is evaluated.
const DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Target {
    /// A `.app` bundle at depth 1 or 2.
    Bundle(PathBuf),
    /// A non-.app directory at depth 1 (vendor folder) created/removed/renamed.
    Container(PathBuf),
    /// FSEvents dropped events: reconcile everything.
    Rescan,
}

/// A watched root: the path as given (used for reporting) and its canonical
/// form (FSEvents reports canonical paths, e.g. /private/var/...).
struct Root {
    given: PathBuf,
    canon: PathBuf,
}

/// Map an event path to the `.app` bundle (or depth-1 container) it belongs to.
fn classify(roots: &[Root], path: &Path) -> Option<Target> {
    // Longest matching root wins (/System/Applications/Utilities vs /System/Applications).
    let (root, rel) = roots
        .iter()
        .flat_map(|r| {
            [&r.canon, &r.given]
                .into_iter()
                .filter_map(move |base| path.strip_prefix(base).ok().map(|rel| (r, base, rel)))
        })
        .max_by_key(|(_, base, _)| base.components().count())
        .map(|(r, _, rel)| (r, rel.to_path_buf()))?;
    let comps: Vec<_> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_owned()),
            _ => None,
        })
        .collect();
    if comps.is_empty() {
        return None;
    }
    let mut acc = root.given.clone();
    for (i, c) in comps.iter().take(MAX_DEPTH).enumerate() {
        acc.push(c);
        if has_app_ext(Path::new(c)) {
            return Some(Target::Bundle(acc));
        }
        if c.to_string_lossy().starts_with('.') {
            return None;
        }
        if i == 0 && comps.len() == 1 {
            return Some(Target::Container(acc));
        }
    }
    None
}

struct WatchState<F> {
    dirs: Vec<PathBuf>,
    known: HashMap<PathBuf, AppEntry>,
    cb: F,
}

impl<F: Fn(WatchEvent)> WatchState<F> {
    fn reconcile_bundle(&mut self, path: &Path) {
        match parse_app_bundle(path) {
            Some(app) => {
                if self.known.get(path) != Some(&app) {
                    self.known.insert(path.to_path_buf(), app.clone());
                    (self.cb)(WatchEvent::Installed(app));
                }
            }
            None => {
                // Missing, or mid-copy without Info.plist yet: only report
                // removal once the directory is actually gone.
                if !path.exists() && self.known.remove(path).is_some() {
                    (self.cb)(WatchEvent::Removed(path.to_path_buf()));
                }
            }
        }
    }

    fn reconcile_container(&mut self, dir: &Path) {
        let mut candidates: HashSet<PathBuf> = self
            .known
            .keys()
            .filter(|k| k.starts_with(dir))
            .cloned()
            .collect();
        if dir.is_dir() {
            if has_app_ext(dir) {
                candidates.insert(dir.to_path_buf());
            } else if let Ok(rd) = fs::read_dir(dir) {
                candidates.extend(rd.flatten().map(|e| e.path()).filter(|p| has_app_ext(p)));
            }
        }
        for c in candidates {
            self.reconcile_bundle(&c);
        }
    }

    fn rescan(&mut self) {
        let current: HashMap<PathBuf, AppEntry> = scan_dirs(&self.dirs)
            .into_iter()
            .map(|a| (a.path.clone(), a))
            .collect();
        let gone: Vec<PathBuf> = self
            .known
            .keys()
            .filter(|k| !current.contains_key(*k))
            .cloned()
            .collect();
        for p in gone {
            self.known.remove(&p);
            (self.cb)(WatchEvent::Removed(p));
        }
        for (p, app) in current {
            if self.known.get(&p) != Some(&app) {
                self.known.insert(p, app.clone());
                (self.cb)(WatchEvent::Installed(app));
            }
        }
    }

    fn flush(&mut self, target: &Target) {
        match target {
            Target::Bundle(p) => self.reconcile_bundle(p),
            Target::Container(p) => self.reconcile_container(p),
            Target::Rescan => self.rescan(),
        }
    }
}

/// Watch `dirs` for `.app` installs/removals. Create, remove and rename
/// events are mapped to their containing bundle and debounced (~500 ms of
/// quiet per bundle) before `cb` is invoked on a dedicated worker thread.
/// The initial state is taken from `scan_dirs(dirs)`, so only changes after
/// the call are reported. Dropping the returned watcher stops the worker.
pub fn watch<F: Fn(WatchEvent) + Send + 'static>(
    dirs: &[PathBuf],
    cb: F,
) -> anyhow::Result<notify::RecommendedWatcher> {
    let roots: Vec<Root> = dirs
        .iter()
        .filter(|d| d.is_dir())
        .map(|d| Root {
            given: d.clone(),
            canon: fs::canonicalize(d).unwrap_or_else(|_| d.clone()),
        })
        .collect();

    let (tx, rx) = mpsc::channel::<Target>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if ev.need_rescan() {
            let _ = tx.send(Target::Rescan);
            return;
        }
        let relevant = matches!(
            ev.kind,
            EventKind::Create(_)
                | EventKind::Remove(_)
                | EventKind::Modify(notify::event::ModifyKind::Name(_))
        );
        if !relevant {
            return;
        }
        for p in &ev.paths {
            if let Some(t) = classify(&roots, p) {
                let _ = tx.send(t);
            }
        }
    })?;
    for d in dirs.iter().filter(|d| d.is_dir()) {
        watcher.watch(d, RecursiveMode::Recursive)?;
    }

    let mut state = WatchState {
        dirs: dirs.to_vec(),
        known: scan_dirs(dirs)
            .into_iter()
            .map(|a| (a.path.clone(), a))
            .collect(),
        cb,
    };
    std::thread::Builder::new()
        .name("app-watcher".into())
        .spawn(move || {
            let mut pending: HashMap<Target, Instant> = HashMap::new();
            loop {
                let msg = match pending.values().min() {
                    None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                    Some(oldest) => rx.recv_timeout(
                        (*oldest + DEBOUNCE).saturating_duration_since(Instant::now()),
                    ),
                };
                let disconnected = match msg {
                    Ok(t) => {
                        pending.insert(t, Instant::now());
                        while let Ok(t) = rx.try_recv() {
                            pending.insert(t, Instant::now());
                        }
                        false
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => false,
                    Err(mpsc::RecvTimeoutError::Disconnected) => true,
                };
                let now = Instant::now();
                let ready: Vec<Target> = pending
                    .iter()
                    .filter(|(_, t)| disconnected || now.duration_since(**t) >= DEBOUNCE)
                    .map(|(k, _)| k.clone())
                    .collect();
                for t in ready {
                    pending.remove(&t);
                    state.flush(&t);
                }
                if disconnected {
                    break;
                }
            }
        })?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn make_app(dir: &Path, name: &str, keys: &[(&str, &str)]) -> PathBuf {
        let app = dir.join(format!("{name}.app"));
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        let body: String = keys
            .iter()
            .map(|(k, v)| format!("<key>{k}</key><string>{v}</string>"))
            .collect();
        fs::write(
            app.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>{body}</dict></plist>"#
            ),
        )
        .unwrap();
        app
    }

    #[test]
    fn parses_xml_plist_and_name_fallbacks() {
        let td = tempfile::tempdir().unwrap();
        let a = make_app(
            td.path(),
            "Alpha",
            &[
                ("CFBundleIdentifier", "com.x.alpha"),
                ("CFBundleDisplayName", "Alpha Display"),
                ("CFBundleName", "AlphaName"),
            ],
        );
        let b = make_app(
            td.path(),
            "Beta",
            &[
                ("CFBundleIdentifier", "com.x.beta"),
                ("CFBundleName", "BetaName"),
            ],
        );
        let c = make_app(td.path(), "Gamma", &[("CFBundleDisplayName", "  ")]);

        let ea = parse_app_bundle(&a).unwrap();
        assert_eq!(
            (ea.bundle_id.as_str(), ea.name.as_str()),
            ("com.x.alpha", "Alpha Display")
        );
        assert!(ea.mtime > 0);
        assert_eq!(parse_app_bundle(&b).unwrap().name, "BetaName");
        let ec = parse_app_bundle(&c).unwrap();
        assert_eq!(ec.name, "Gamma");
        assert_eq!(ec.bundle_id, format!("path:{}", c.display()));
    }

    #[test]
    fn invalid_bundles_rejected() {
        let td = tempfile::tempdir().unwrap();
        let no_plist = td.path().join("Empty.app");
        fs::create_dir_all(&no_plist).unwrap();
        assert!(parse_app_bundle(&no_plist).is_none());
        let not_app = td.path().join("Folder");
        fs::create_dir_all(&not_app).unwrap();
        assert!(parse_app_bundle(&not_app).is_none());
        assert!(parse_app_bundle(&td.path().join("Missing.app")).is_none());
    }

    #[test]
    fn scan_depth_two_vendor_and_no_descent_into_app() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let outer = make_app(root, "Outer", &[("CFBundleIdentifier", "com.x.outer")]);
        // Nested helper app inside Contents must NOT be found.
        make_app(
            &outer.join("Contents/Helpers"),
            "Helper",
            &[("CFBundleIdentifier", "com.x.helper")],
        );
        make_app(&outer, "Direct", &[("CFBundleIdentifier", "com.x.direct")]);
        // Depth 2 vendor dir is found; depth 3 is not.
        make_app(
            &root.join("Vendor"),
            "VendorApp",
            &[("CFBundleIdentifier", "com.x.vendor")],
        );
        make_app(
            &root.join("Vendor/Deep"),
            "TooDeep",
            &[("CFBundleIdentifier", "com.x.deep")],
        );
        // Hidden entries skipped.
        make_app(
            &root.join(".hidden"),
            "Hidden",
            &[("CFBundleIdentifier", "com.x.hidden")],
        );

        let ids: Vec<String> = scan_dirs(&[root.to_path_buf()])
            .into_iter()
            .map(|a| a.bundle_id)
            .collect();
        assert_eq!(ids, vec!["com.x.outer", "com.x.vendor"]);
    }

    #[test]
    fn scan_dedups_sorts_and_skips_missing_dirs() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        make_app(&root, "zeta", &[("CFBundleIdentifier", "z")]);
        make_app(&root, "Alpha", &[("CFBundleIdentifier", "a")]);
        make_app(&root.join("Sub"), "mid", &[("CFBundleIdentifier", "m")]);
        // Root listed twice + its subdir (like /System/Applications/Utilities).
        let dirs = vec![
            root.clone(),
            root.join("Sub"),
            root.clone(),
            root.join("does-not-exist"),
        ];
        let names: Vec<String> = scan_dirs(&dirs).into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["Alpha", "mid", "zeta"]);
    }

    #[test]
    fn classify_maps_paths_to_bundles() {
        let roots = vec![
            Root {
                given: "/A".into(),
                canon: "/private/A".into(),
            },
            Root {
                given: "/A/Utilities".into(),
                canon: "/A/Utilities".into(),
            },
        ];
        let b = |p: &str| Some(Target::Bundle(PathBuf::from(p)));
        assert_eq!(
            classify(&roots, Path::new("/A/Foo.app/Contents/Info.plist")),
            b("/A/Foo.app")
        );
        assert_eq!(
            classify(&roots, Path::new("/private/A/Foo.app")),
            b("/A/Foo.app")
        );
        assert_eq!(
            classify(&roots, Path::new("/A/V/Bar.app/Contents/x.app")),
            b("/A/V/Bar.app")
        );
        assert_eq!(
            classify(&roots, Path::new("/A/Utilities/T.app/Contents")),
            b("/A/Utilities/T.app")
        );
        assert_eq!(
            classify(&roots, Path::new("/A/V")),
            Some(Target::Container("/A/V".into()))
        );
        assert_eq!(classify(&roots, Path::new("/A/V/a/b.app")), None);
        assert_eq!(classify(&roots, Path::new("/A/V/readme.txt")), None);
        assert_eq!(classify(&roots, Path::new("/Elsewhere/X.app")), None);
    }

    fn wait_for(events: &Arc<Mutex<Vec<WatchEvent>>>, pred: impl Fn(&WatchEvent) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if events.lock().unwrap().iter().any(&pred) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn watcher_reports_install_and_removal_once() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let _w = watch(&[root.clone()], move |e| sink.lock().unwrap().push(e)).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let app = make_app(&root, "Fresh", &[("CFBundleIdentifier", "com.x.fresh")]);
        assert!(wait_for(
            &events,
            |e| matches!(e, WatchEvent::Installed(a) if a.bundle_id == "com.x.fresh")
        ));
        std::thread::sleep(Duration::from_millis(800));
        let installs = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, WatchEvent::Installed(_)))
            .count();
        assert_eq!(
            installs,
            1,
            "duplicate events must be debounced: {:?}",
            events.lock().unwrap()
        );

        fs::remove_dir_all(&app).unwrap();
        assert!(wait_for(&events, |e| *e == WatchEvent::Removed(app.clone())));
    }
}
