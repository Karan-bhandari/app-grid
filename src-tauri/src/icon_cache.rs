//! 256x256 app icon cache (spec §7 Phase 1, UT-07).
//!
//! Icons are rasterised from `NSWorkspace -iconForFile:` into an offscreen
//! RGBA bitmap and stored as lossless WebP under
//! `~/Library/Caches/com.personal.app-grid/icons/`.
//!
//! File name: `<sha256(path)[..16]>_<cache_key>.webp` where
//! `cache_key = hex(sha256("{path}|{mtime}"))`. The path-hash prefix lets us
//! delete stale icons for the same app (different mtime) on regeneration
//! without a separate index file that could drift out of sync.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use image::{ExtendedColorType, RgbaImage};
use sha2::{Digest, Sha256};

use crate::app_scanner::AppEntry;

/// Edge length in pixels of every cached icon.
pub const ICON_PX: u32 = 256;

pub struct IconCache {
    root: PathBuf,
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Stable per-app-path prefix (16 hex chars = 64 bits).
fn path_prefix(path: &Path) -> String {
    sha256_hex(path.as_os_str().as_encoded_bytes())[..16].to_owned()
}

impl IconCache {
    /// root = ~/Library/Caches/com.personal.app-grid/icons
    pub fn new() -> Result<Self> {
        let caches = dirs::cache_dir().ok_or_else(|| anyhow!("cannot resolve ~/Library/Caches"))?;
        Self::with_root(caches.join("com.personal.app-grid").join("icons"))
    }

    /// Use a custom root (tests). Created if missing.
    pub fn with_root(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)
            .with_context(|| format!("creating icon cache {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// hex(sha256("{path}|{mtime}"))
    pub fn cache_key(app: &AppEntry) -> String {
        sha256_hex(format!("{}|{}", app.path.display(), app.mtime).as_bytes())
    }

    pub fn cached_path(&self, app: &AppEntry) -> PathBuf {
        self.root.join(format!(
            "{}_{}.webp",
            path_prefix(&app.path),
            Self::cache_key(app)
        ))
    }

    /// Returns path to the 256x256 icon, generating it via NSWorkspace if
    /// missing and removing stale icons for the same app path.
    pub fn get_or_create(&self, app: &AppEntry) -> Result<PathBuf> {
        self.get_or_create_with(app, render_icon)
    }

    /// Same as [`get_or_create`](Self::get_or_create) with an injectable
    /// renderer (test seam). Images not 256x256 are resized.
    pub fn get_or_create_with<F>(&self, app: &AppEntry, render: F) -> Result<PathBuf>
    where
        F: Fn(&Path) -> Result<RgbaImage>,
    {
        let dest = self.cached_path(app);
        if dest.is_file() {
            return Ok(dest);
        }
        let mut img = render(&app.path)
            .with_context(|| format!("rendering icon for {}", app.path.display()))?;
        if img.dimensions() != (ICON_PX, ICON_PX) {
            img = image::imageops::resize(
                &img,
                ICON_PX,
                ICON_PX,
                image::imageops::FilterType::Lanczos3,
            );
        }
        write_webp_atomic(&img, &dest)?;
        self.remove_stale(&app.path, &dest);
        Ok(dest)
    }

    /// Parallel batch version used by scan-benchmark. Results are in input order.
    pub fn warm(&self, apps: &[AppEntry]) -> Vec<Result<PathBuf>> {
        self.warm_with(apps, render_icon)
    }

    /// [`warm`](Self::warm) with an injectable renderer.
    pub fn warm_with<F>(&self, apps: &[AppEntry], render: F) -> Vec<Result<PathBuf>>
    where
        F: Fn(&Path) -> Result<RgbaImage> + Sync,
    {
        let mut results: Vec<Option<Result<PathBuf>>> = Vec::with_capacity(apps.len());
        let mut misses = Vec::new();
        // Fast path: warm hits are just a stat() each, no threads spawned.
        for (i, app) in apps.iter().enumerate() {
            let p = self.cached_path(app);
            if p.is_file() {
                results.push(Some(Ok(p)));
            } else {
                results.push(None);
                misses.push(i);
            }
        }
        if !misses.is_empty() {
            let workers = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(misses.len());
            let next = AtomicUsize::new(0);
            let done: Vec<(usize, Result<PathBuf>)> = std::thread::scope(|s| {
                let handles: Vec<_> = (0..workers)
                    .map(|_| {
                        s.spawn(|| {
                            let mut out = Vec::new();
                            loop {
                                let j = next.fetch_add(1, Ordering::Relaxed);
                                let Some(&i) = misses.get(j) else { break };
                                out.push((i, self.get_or_create_with(&apps[i], &render)));
                            }
                            out
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            });
            for (i, r) in done {
                results[i] = Some(r);
            }
        }
        results
            .into_iter()
            .map(|r| r.unwrap_or_else(|| Err(anyhow!("icon worker panicked"))))
            .collect()
    }

    /// Delete every `<prefix>_*.webp` for this app path except `keep`.
    fn remove_stale(&self, app_path: &Path, keep: &Path) {
        let prefix = format!("{}_", path_prefix(app_path));
        let Ok(rd) = fs::read_dir(&self.root) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && name.ends_with(".webp") && e.path() != keep {
                if let Err(err) = fs::remove_file(e.path()) {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!("failed to remove stale icon {}: {err}", e.path().display());
                    }
                }
            }
        }
    }
}

/// Write via a unique temp file + rename so readers never see partial files
/// and concurrent writers of the same key cannot corrupt each other.
fn write_webp_atomic(img: &RgbaImage, dest: &Path) -> Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = dest
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", dest.display()))?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let res = (|| -> Result<()> {
        let mut w = BufWriter::new(fs::File::create(&tmp)?);
        image::codecs::webp::WebPEncoder::new_lossless(&mut w).encode(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgba8,
        )?;
        w.flush()?;
        drop(w);
        fs::rename(&tmp, dest)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.with_context(|| format!("writing {}", dest.display()))
}

/// Rasterise the Finder icon of `path` to 256x256 straight (non-premultiplied) RGBA.
#[allow(clippy::chunks_exact_to_as_chunks)]
pub fn render_icon(path: &Path) -> Result<RgbaImage> {
    use objc2::rc::autoreleasepool;
    use objc2::AnyThread;
    use objc2_app_kit::{
        NSBitmapFormat, NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace,
        NSGraphicsContext, NSImageInterpolation, NSWorkspace,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 path {}", path.display()))?;
    let px = ICON_PX as isize;

    autoreleasepool(|_| {
        // SAFETY: NSWorkspace/NSImage/NSBitmapImageRep/NSGraphicsContext are
        // used only from this thread and never escape it. Drawing into an
        // offscreen bitmap-backed NSGraphicsContext is supported off the main
        // thread (the current context is per-thread). Every pointer we read
        // comes from the bitmap rep we allocated, which stays alive (`rep`)
        // for the whole read, and reads are bounded by bytesPerRow * height.
        unsafe {
            let image = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(path_str));
            let size = NSSize::new(ICON_PX as f64, ICON_PX as f64);
            image.setSize(size);

            let rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                px,
                px,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                0,
                32,
            )
            .ok_or_else(|| anyhow!("NSBitmapImageRep allocation failed"))?;

            let data = rep.bitmapData();
            let bpr = rep.bytesPerRow();
            if data.is_null() || bpr < px * 4 || rep.bitsPerPixel() != 32 {
                bail!(
                    "unexpected bitmap layout (bytesPerRow={bpr}, bpp={})",
                    rep.bitsPerPixel()
                );
            }
            let bpr = bpr as usize;
            std::ptr::write_bytes(data, 0, bpr * ICON_PX as usize);

            let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)
                .ok_or_else(|| anyhow!("cannot create bitmap graphics context"))?;
            NSGraphicsContext::saveGraphicsState_class();
            NSGraphicsContext::setCurrentContext(Some(&ctx));
            ctx.setImageInterpolation(NSImageInterpolation::High);
            image.drawInRect_fromRect_operation_fraction(
                NSRect::new(NSPoint::new(0.0, 0.0), size),
                NSRect::ZERO,
                NSCompositingOperation::SourceOver,
                1.0,
            );
            ctx.flushGraphics();
            NSGraphicsContext::restoreGraphicsState_class();

            let format = rep.bitmapFormat();
            let alpha_first = format.contains(NSBitmapFormat::AlphaFirst);
            let premultiplied = !format.contains(NSBitmapFormat::AlphaNonpremultiplied);
            let src = std::slice::from_raw_parts(data as *const u8, bpr * ICON_PX as usize);

            let mut out = RgbaImage::new(ICON_PX, ICON_PX);
            for (y, row) in src.chunks_exact(bpr).enumerate() {
                for (x, p) in row[..ICON_PX as usize * 4].chunks_exact(4).enumerate() {
                    let [r, g, b, a] = if alpha_first {
                        [p[1], p[2], p[3], p[0]]
                    } else {
                        [p[0], p[1], p[2], p[3]]
                    };
                    let rgba = if premultiplied {
                        unpremultiply(r, g, b, a)
                    } else {
                        [r, g, b, a]
                    };
                    out.put_pixel(x as u32, y as u32, image::Rgba(rgba));
                }
            }
            Ok(out)
        }
    })
}

fn unpremultiply(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    if a == 0 {
        return [0, 0, 0, 0];
    }
    let f = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
    [f(r), f(g), f(b), a]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn fake_app(dir: &Path, mtime: u64) -> AppEntry {
        let path = dir.join("Fake.app");
        fs::create_dir_all(path.join("Contents")).unwrap();
        AppEntry {
            bundle_id: "com.test.fake".into(),
            name: "Fake".into(),
            path,
            mtime,
        }
    }

    fn solid(_: &Path) -> Result<RgbaImage> {
        Ok(RgbaImage::from_pixel(
            ICON_PX,
            ICON_PX,
            image::Rgba([10, 20, 30, 255]),
        ))
    }

    fn webp_count(root: &Path) -> usize {
        fs::read_dir(root)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "webp"))
            .count()
    }

    #[test]
    fn ut07_cache_key_invalidation_on_app_update() {
        let td = tempfile::tempdir().unwrap();
        let cache = IconCache::with_root(td.path().join("icons")).unwrap();
        let v1 = fake_app(td.path(), 1);
        let v2 = AppEntry {
            mtime: 2,
            ..v1.clone()
        };

        // Real NSWorkspace renderer (returns a generic app icon for a fake bundle).
        let p1 = cache.get_or_create(&v1).unwrap();
        assert!(p1.is_file());
        assert_eq!(p1, cache.cached_path(&v1));

        assert_ne!(IconCache::cache_key(&v1), IconCache::cache_key(&v2));
        let p2_expected = cache.cached_path(&v2);
        assert_ne!(p1, p2_expected);
        assert!(
            !p2_expected.exists(),
            "new key must miss before regeneration"
        );

        let p2 = cache.get_or_create(&v2).unwrap();
        assert_eq!(p2, p2_expected);
        let img = image::open(&p2).unwrap();
        assert_eq!((img.width(), img.height()), (ICON_PX, ICON_PX));
        assert!(
            img.to_rgba8().pixels().any(|p| p[3] > 0),
            "rendered icon is fully transparent"
        );
        assert!(!p1.exists(), "stale icon must be removed");
        assert!(!cache.cached_path(&v1).is_file(), "old key must miss");
        assert_eq!(webp_count(cache.root()), 1);
    }

    #[test]
    fn warm_hit_does_not_rerender() {
        let td = tempfile::tempdir().unwrap();
        let cache = IconCache::with_root(td.path().join("icons")).unwrap();
        let app = fake_app(td.path(), 7);
        let calls = AtomicUsize::new(0);
        let render = |p: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            solid(p)
        };
        let a = cache.get_or_create_with(&app, render).unwrap();
        let b = cache.get_or_create_with(&app, render).unwrap();
        assert_eq!(a, b);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let decoded = image::open(&a).unwrap().to_rgba8();
        assert_eq!(decoded.get_pixel(5, 5), &image::Rgba([10, 20, 30, 255]));
    }

    #[test]
    fn warm_batch_parallel_preserves_order_and_caches() {
        let td = tempfile::tempdir().unwrap();
        let cache = IconCache::with_root(td.path().join("icons")).unwrap();
        let apps: Vec<AppEntry> = (0..40)
            .map(|i| AppEntry {
                bundle_id: format!("com.test.{i}"),
                name: format!("App {i}"),
                path: td.path().join(format!("App{i}.app")),
                mtime: 1,
            })
            .collect();
        let calls = AtomicUsize::new(0);
        let render = |p: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            // Non-256 output is resized.
            let _ = p;
            Ok(RgbaImage::new(64, 64))
        };
        let res = cache.warm_with(&apps, render);
        for (app, r) in apps.iter().zip(&res) {
            assert_eq!(r.as_ref().unwrap(), &cache.cached_path(app));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 40);
        let again = cache.warm_with(&apps, render);
        assert!(again.iter().all(|r| r.is_ok()));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            40,
            "warm cache must not re-render"
        );
        let img = image::open(res[3].as_ref().unwrap()).unwrap();
        assert_eq!((img.width(), img.height()), (ICON_PX, ICON_PX));
    }

    #[test]
    fn real_render_system_app() {
        let calc = Path::new("/System/Applications/Calculator.app");
        if !calc.exists() {
            return;
        }
        let img = render_icon(calc).unwrap();
        assert_eq!(img.dimensions(), (ICON_PX, ICON_PX));
        let opaque = img.pixels().filter(|p| p[3] > 0).count();
        assert!(
            opaque > (ICON_PX * ICON_PX / 4) as usize,
            "only {opaque} visible pixels"
        );
    }

    #[test]
    fn unpremultiply_math() {
        assert_eq!(unpremultiply(0, 0, 0, 0), [0, 0, 0, 0]);
        assert_eq!(unpremultiply(64, 128, 0, 128), [128, 255, 0, 128]);
        assert_eq!(unpremultiply(10, 20, 30, 255), [10, 20, 30, 255]);
    }
}
