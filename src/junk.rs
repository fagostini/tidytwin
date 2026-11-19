//! Clutter: empty folders/files, temporary files, broken symlinks, rebuildable build/cache
//! folders, corrupt or misnamed images/music, names that break on other systems.
use crate::scan::{FileEntry, Group, Progress, ScanOpts, wanted};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Kind {
    EmptyDir,
    EmptyFile,
    Temp,
    BrokenLink,
    Rebuildable,
    Corrupt,
    WrongExt,
    BadName,
}

impl Kind {
    pub const ALL: [Kind; 8] = [Kind::EmptyDir, Kind::EmptyFile, Kind::Temp, Kind::BrokenLink, Kind::Rebuildable, Kind::Corrupt, Kind::WrongExt, Kind::BadName];
    pub fn label(self) -> &'static str {
        match self {
            Kind::EmptyDir => "Empty folders",
            Kind::EmptyFile => "Empty files",
            Kind::Temp => "Temporary files",
            Kind::BrokenLink => "Broken symlinks",
            Kind::Rebuildable => "Rebuildable build & cache folders",
            Kind::Corrupt => "Broken images & music",
            Kind::WrongExt => "Wrong file extension",
            Kind::BadName => "Problem file names",
        }
    }
    /// Plain junk, safe to bulk-select. The rest need a look first (or are fine files with a fixable issue).
    pub fn bulk(self) -> bool {
        matches!(self, Kind::EmptyDir | Kind::EmptyFile | Kind::Temp | Kind::BrokenLink)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Item {
    pub file: FileEntry,
    pub kind: Kind,
    /// Why it was flagged: link target, decoder error, real format, which tool rebuilds it.
    pub why: String,
}

const IMG_EXT: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff"];
const AUDIO_EXT: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "aac", "wav", "wv", "ape", "mpc", "aiff"];

fn ext_in(p: &Path, exts: &[&str]) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
}

/// Leftovers of editors, downloads, OS file browsers and our own interrupted writes.
pub fn is_temp(name: &str) -> bool {
    const EXT: &[&str] = &["tmp", "temp", "swp", "swo", "part", "partial", "crdownload"];
    const NAMES: &[&str] = &[".DS_Store", "Thumbs.db", "ehthumbs.db"];
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    NAMES.contains(&name)
        || EXT.contains(&ext.as_str())
        || (name.len() > 1 && name.ends_with('~'))
        || name.starts_with(".~lock.")
        || (name.len() > 2 && name.starts_with('#') && name.ends_with('#'))
        || name.contains(".tidytwin-tmp-")
}

/// Empty files that mean something by existing (markers), so not clutter.
fn is_marker(name: &str) -> bool {
    name.starts_with('.') || ["__init__.py", "py.typed"].contains(&name)
}

/// Folders a tool regenerates on demand. `why` names the tool. Checked before hidden/exclude
/// filters on purpose: `.venv`, `.cache` and `node_modules` are hidden or excluded by default.
pub fn rebuildable(dir: &Path, home: Option<&Path>) -> Option<&'static str> {
    let name = dir.file_name()?.to_str()?;
    let parent = dir.parent()?;
    let sib = |f: &str| parent.join(f).exists();
    let has = |f: &str| dir.join(f).exists();
    Some(match name {
        "node_modules" => "npm / yarn / pnpm install",
        "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".tox" => "Python regenerates it",
        ".gradle" => "Gradle",
        ".next" | ".nuxt" | ".parcel-cache" | ".turbo" | ".svelte-kit" => "JS build tool",
        "target" if sib("Cargo.toml") => "cargo build",
        "build" if sib("CMakeLists.txt") || sib("build.gradle") || sib("build.gradle.kts") || sib("package.json") || sib("pyproject.toml") => "project build output",
        ".venv" | "venv" | "env" if has("pyvenv.cfg") => "Python virtualenv (recreate from requirements)",
        ".cache" if home == Some(parent) => "user cache (apps refill it)",
        // Cache Directory Tagging spec: the folder declares itself disposable.
        _ if has("CACHEDIR.TAG") => "tagged as a cache (CACHEDIR.TAG)",
        _ => return None,
    })
}

/// Names that break on Windows/macOS filesystems, USB sticks or cloud sync.
pub fn bad_name(name: &OsStr) -> Option<String> {
    let Some(n) = name.to_str() else { return Some("not valid UTF-8".into()) };
    if let Some(c) = n.chars().find(|c| c.is_control()) {
        return Some(format!("contains control character U+{:04X}", c as u32));
    }
    if let Some(c) = n.chars().find(|c| r#"<>:"\|?*"#.contains(*c)) {
        return Some(format!("contains '{c}' (not allowed on Windows)"));
    }
    if n.starts_with(' ') || n.ends_with(' ') {
        return Some("starts or ends with a space".into());
    }
    if n.ends_with('.') && n != "." && n != ".." {
        return Some("ends with a dot (dropped on Windows)".into());
    }
    let stem = n.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0');
    reserved.then(|| format!("{stem} is a reserved name on Windows"))
}

/// Holds nothing but (possibly nested) folders. Walks unfiltered so hidden or excluded files
/// count, and an unreadable entry counts as content: never call a folder empty on a guess.
fn only_dirs(p: &Path) -> bool {
    walkdir::WalkDir::new(p).follow_links(false).into_iter().all(|e| e.is_ok_and(|e| e.file_type().is_dir()))
}

/// Bytes under `p`, not following links.
fn du(p: &Path) -> u64 {
    walkdir::WalkDir::new(p).follow_links(false).into_iter().flatten().filter_map(|e| e.metadata().ok()).filter(|m| !m.is_dir()).map(|m| m.len()).sum()
}

/// Image whose bytes are a different format than its extension says: Some(real extension).
fn real_ext(p: &Path) -> Option<&'static str> {
    let got = image::ImageReader::open(p).ok()?.with_guessed_format().ok()?.format()?;
    let want = image::ImageFormat::from_path(p).ok();
    (Some(got) != want).then(|| got.extensions_str().first().copied()).flatten()
}

fn entry(path: PathBuf, size: u64, mtime: SystemTime, root: usize) -> FileEntry {
    FileEntry { path, size, mtime, root }
}

fn mtime_of(md: Option<std::fs::Metadata>) -> SystemTime {
    md.and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH)
}

// ponytail: second walk of the tree after `collect`; fold into it if scans of huge trees feel slow.
pub fn find(opts: &ScanOpts, prog: &Progress, corrupt: bool) -> Vec<Item> {
    let data = crate::undo::data_dir();
    let home = std::env::home_dir();
    let mut out = Vec::new();
    let mut media = Vec::new();
    let mut rebuild: Vec<Item> = Vec::new();
    for (ri, root) in opts.roots.iter().enumerate() {
        // Shared with the filter closure: a skipped rebuildable folder still counts as content.
        let has_content: RefCell<HashMap<PathBuf, bool>> = RefCell::new(HashMap::new());
        let found: RefCell<Vec<Item>> = RefCell::new(Vec::new());
        // Children before parents, so each folder knows whether anything was inside.
        let it = walkdir::WalkDir::new(root).follow_links(false).contents_first(true).into_iter().filter_entry(|e| {
            if e.depth() > 0 && e.file_type().is_dir() && !e.path().starts_with(&data) && e.file_name() != crate::undo::QUARANTINE_NAME {
                if let Some(why) = rebuildable(e.path(), home.as_deref()) {
                    let fe = entry(e.path().to_path_buf(), 0, mtime_of(e.metadata().ok()), ri);
                    found.borrow_mut().push(Item { file: fe, kind: Kind::Rebuildable, why: why.into() });
                    if let Some(p) = e.path().parent() {
                        has_content.borrow_mut().insert(p.to_path_buf(), true);
                    }
                    return false; // never descend: its contents are not clutter of their own
                }
            }
            wanted(e, opts, &data)
        });
        let mut empty: Vec<FileEntry> = Vec::new();
        for e in it {
            if prog.cancel.load(Ordering::Relaxed) {
                return out;
            }
            let Ok(e) = e else { continue };
            let ft = e.file_type();
            let path = e.path();
            if e.depth() > 0 && let Some(why) = bad_name(e.file_name()) {
                let fe = entry(path.to_path_buf(), 0, mtime_of(e.metadata().ok()), ri);
                out.push(Item { file: fe, kind: Kind::BadName, why });
            }
            let content = if ft.is_dir() {
                let c = has_content.borrow_mut().remove(path).unwrap_or(false);
                if !c && e.depth() > 0 {
                    empty.push(entry(path.to_path_buf(), 0, mtime_of(e.metadata().ok()), ri));
                }
                c
            } else {
                true
            };
            if content && let Some(parent) = path.parent() {
                has_content.borrow_mut().insert(parent.to_path_buf(), true);
            }
            if ft.is_dir() {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            let fe = entry(path.to_path_buf(), md.len(), md.modified().unwrap_or(SystemTime::UNIX_EPOCH), ri);
            let name = e.file_name().to_string_lossy();
            if ft.is_symlink() {
                if std::fs::metadata(path).is_err() {
                    let target = std::fs::read_link(path).map(|t| format!("→ {}", t.display())).unwrap_or_default();
                    out.push(Item { file: fe, kind: Kind::BrokenLink, why: target });
                }
            } else if is_temp(&name) {
                out.push(Item { file: fe, kind: Kind::Temp, why: String::new() });
            } else if md.len() == 0 {
                if !is_marker(&name) {
                    out.push(Item { file: fe, kind: Kind::EmptyFile, why: String::new() });
                }
            } else if ext_in(path, IMG_EXT) || (corrupt && ext_in(path, AUDIO_EXT)) {
                media.push(fe);
            }
        }
        // Report only the topmost empty folder of a nest: removing it removes the rest.
        let set: HashSet<&Path> = empty.iter().map(|f| f.path.as_path()).collect();
        let top: Vec<FileEntry> = empty.iter().filter(|f| !f.path.parent().is_some_and(|p| set.contains(p))).cloned().collect();
        out.extend(top.into_iter().filter(|f| only_dirs(&f.path)).map(|file| Item { file, kind: Kind::EmptyDir, why: String::new() }));
        rebuild.extend(found.into_inner());
    }
    prog.begin(media.len() + rebuild.len());
    let step = || prog.hashed.fetch_add(1, Ordering::Relaxed);
    out.par_extend(rebuild.into_par_iter().map(|mut i| {
        i.file.size = du(&i.file.path);
        step();
        i
    }));
    out.par_extend(media.into_par_iter().filter_map(|f| {
        if prog.cancel.load(Ordering::Relaxed) {
            return None;
        }
        let r = if ext_in(&f.path, IMG_EXT) {
            // Header sniff is cheap, so misnamed images are always checked; full decode only on request.
            match real_ext(&f.path) {
                Some(ext) => Some((Kind::WrongExt, format!("actually .{ext}"))),
                None if corrupt => image::open(&f.path).err().map(|e| (Kind::Corrupt, e.to_string())),
                None => None,
            }
        } else {
            lofty::read_from_path(&f.path).err().map(|e| (Kind::Corrupt, e.to_string()))
        };
        step();
        r.map(|(kind, why)| Item { file: f, kind, why })
    }));
    out
}

/// Split items into one group per kind (in `Kind::ALL` order) plus the per-path reasons.
pub fn group(items: Vec<Item>) -> (Vec<Group>, Vec<Kind>, HashMap<(Kind, PathBuf), String>) {
    let (mut groups, mut kinds) = (Vec::new(), Vec::new());
    for k in Kind::ALL {
        let g: Group = items.iter().filter(|i| i.kind == k).map(|i| i.file.clone()).collect();
        if !g.is_empty() {
            groups.push(g);
            kinds.push(k);
        }
    }
    let why = items.into_iter().filter(|i| !i.why.is_empty()).map(|i| ((i.kind, i.file.path), i.why)).collect();
    (groups, kinds, why)
}

pub fn to_csv(items: &[Item]) -> String {
    let mut s = String::from("kind,size,mtime_unix,path,why\n");
    let q = |x: &str| format!("\"{}\"", x.replace('"', "\"\""));
    for i in items {
        let t = i.file.mtime.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        s += &format!("{:?},{},{t},{},{}\n", i.kind, i.file.size, q(&i.file.path.display().to_string()), q(&i.why));
    }
    s
}

/// Rename a misnamed image to its real extension. Journaled as a move, so undo renames it back.
pub fn fix_ext(p: &Path, batch: u64) -> Result<crate::undo::Entry, String> {
    let ext = real_ext(p).ok_or("extension already matches the content")?;
    let to = p.with_extension(ext);
    if std::fs::symlink_metadata(&to).is_ok() {
        return Err(format!("{} already exists", to.display()));
    }
    let md = std::fs::symlink_metadata(p).map_err(|e| e.to_string())?;
    std::fs::rename(p, &to).map_err(|e| e.to_string())?;
    Ok(crate::undo::Entry {
        batch,
        time: SystemTime::now(),
        action: crate::scan::Action::MoveTo,
        orig: p.to_path_buf(),
        keep: None,
        hash: None,
        size: md.len(),
        mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        perms: crate::undo::perms_of(&md),
        stash: Some(to),
        undone: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_clutter() {
        let d = std::env::temp_dir().join(format!("tidytwin-junk-{}", crate::undo::new_batch()));
        let mk = |p: &str| std::fs::create_dir_all(d.join(p)).unwrap();
        let put = |p: &str, s: &str| std::fs::write(d.join(p), s).unwrap();
        mk("empty/nested/deeper");
        mk("hidden_only");
        put("hidden_only/.keep", "x");
        mk("proj/node_modules/pkg"); // only content of proj: proj must not count as empty
        put("proj/node_modules/pkg/i.js", "ab");
        mk("full");
        put("full/a.txt", "x");
        put("full/a.txt~", "x");
        put("full/a:b.txt", "x");
        put("full/bad.png", "not a png");
        put("full/empty.txt", "");
        put("full/__init__.py", "");
        image::RgbImage::new(1, 1).save_with_format(d.join("full/x.jpg"), image::ImageFormat::Png).unwrap();
        std::os::unix::fs::symlink(d.join("nowhere"), d.join("full/dangling")).unwrap();
        let opts = ScanOpts { roots: vec![d.clone()], min_size: 0, exclude: vec![], follow_hidden: false };
        let items = find(&opts, &Progress::default(), true);
        let mut got: Vec<(Kind, PathBuf)> = items.iter().map(|i| (i.kind, i.file.path.clone())).collect();
        got.sort_by(|a, b| a.1.cmp(&b.1));
        let nm = items.iter().find(|i| i.kind == Kind::Rebuildable).map(|i| i.file.size);
        let wrong = items.iter().find(|i| i.kind == Kind::WrongExt).map(|i| i.why.clone());
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(
            got,
            vec![
                (Kind::EmptyDir, d.join("empty")), // topmost only; hidden_only has a hidden file
                (Kind::Temp, d.join("full/a.txt~")),
                (Kind::BadName, d.join("full/a:b.txt")),
                (Kind::Corrupt, d.join("full/bad.png")),
                (Kind::BrokenLink, d.join("full/dangling")),
                (Kind::EmptyFile, d.join("full/empty.txt")), // __init__.py is a marker
                (Kind::WrongExt, d.join("full/x.jpg")),
                (Kind::Rebuildable, d.join("proj/node_modules")),
            ]
        );
        assert_eq!(nm, Some(2));
        assert_eq!(wrong.as_deref(), Some("actually .png"));
    }

    #[test]
    fn cleanup_and_undo() {
        use crate::scan::{Action, ApplyOpts, apply};
        let d = std::env::temp_dir().join(format!("tidytwin-junk-act-{}", crate::undo::new_batch()));
        std::fs::create_dir_all(d.join("e/sub")).unwrap();
        std::fs::create_dir_all(d.join("w")).unwrap();
        std::os::unix::fs::symlink(d.join("nowhere"), d.join("w/link")).unwrap();
        let mut j = crate::undo::Journal::open(d.join("data"));
        let fe = |p: PathBuf| entry(p, 0, SystemTime::UNIX_EPOCH, 0);
        let groups = vec![vec![fe(d.join("e"))], vec![fe(d.join("w/link"))]];
        let sel: HashSet<PathBuf> = [d.join("e"), d.join("w/link")].into();

        // Quarantine both (no kept copy), then undo restores the dangling link itself.
        let b = crate::undo::new_batch();
        let o = ApplyOpts { action: Some(Action::Quarantine), dest: Some(j.quarantine_dir(b)), batch: b, no_keep: true, ..Default::default() };
        let (es, errs) = apply(&groups, &sel, &o);
        assert!(errs.is_empty(), "{errs:?}");
        j.record(es).unwrap();
        assert!(std::fs::symlink_metadata(d.join("w/link")).is_err() && !d.join("e").exists());
        assert!(j.undo_batch(b).is_empty());
        assert!(std::fs::symlink_metadata(d.join("w/link")).unwrap().file_type().is_symlink());
        assert!(d.join("e/sub").is_dir());

        // Permanent delete of an empty nest works, but refuses once a file appears inside.
        std::fs::write(d.join("e/sub/new.txt"), "x").unwrap();
        let o = ApplyOpts { action: Some(Action::Delete), batch: b + 1, no_keep: true, ..Default::default() };
        let (_, errs) = apply(&groups[..1], &sel, &o);
        assert_eq!(errs.len(), 1, "non-empty folder must be refused");
        assert!(d.join("e/sub/new.txt").exists());
        std::fs::remove_file(d.join("e/sub/new.txt")).unwrap();
        let (es, errs) = apply(&groups, &sel, &o);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(es.len(), 2);
        assert!(!d.join("e").exists() && std::fs::symlink_metadata(d.join("w/link")).is_err());

        // Fixing a misnamed image renames it; undo renames it back.
        let img = d.join("pic.jpg");
        image::RgbImage::new(1, 1).save_with_format(&img, image::ImageFormat::Png).unwrap();
        let fb = crate::undo::new_batch();
        j.record(vec![fix_ext(&img, fb).unwrap()]).unwrap();
        assert!(d.join("pic.png").exists() && !img.exists());
        assert!(j.undo_batch(fb).is_empty());
        assert!(img.exists() && !d.join("pic.png").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn ignore_key() {
        use crate::scan::{drop_ignored, group_key};
        let fe = |p: &str| entry(p.into(), 1, SystemTime::UNIX_EPOCH, 0);
        let (ab, ba, abc) = (vec![fe("/a"), fe("/b")], vec![fe("/b"), fe("/a")], vec![fe("/a"), fe("/b"), fe("/c")]);
        assert_eq!(group_key(&ab), group_key(&ba)); // order doesn't matter
        let mut gs = vec![ba, abc];
        drop_ignored(&mut gs, &[group_key(&ab)]);
        assert_eq!(gs.len(), 1); // a new copy (/c) makes it a different group: still shown
        assert_eq!(gs[0].len(), 3);
    }

    #[test]
    fn temp_names() {
        for n in ["x.tmp", "f.PART", "notes.txt~", ".~lock.a.odt#", "#draft#", ".DS_Store", "a.tidytwin-tmp-123"] {
            assert!(is_temp(n), "{n}");
        }
        for n in ["tmp", "~", "readme.md", "#", "template.txt"] {
            assert!(!is_temp(n), "{n}");
        }
    }

    #[test]
    fn bad_names() {
        for n in ["a:b", "what?", "trail ", " lead", "dot.", "CON", "nul.txt", "com1.log", "tab\there"] {
            assert!(bad_name(OsStr::new(n)).is_some(), "{n}");
        }
        for n in ["normal.txt", ".hidden", "CONFIG", "com0", "COM10.txt", "a b.c"] {
            assert_eq!(bad_name(OsStr::new(n)), None, "{n}");
        }
        use std::os::unix::ffi::OsStrExt;
        assert!(bad_name(OsStr::from_bytes(b"\xff.txt")).is_some());
    }

    #[test]
    fn rebuildable_dirs() {
        let d = std::env::temp_dir().join(format!("tidytwin-rb-{}", crate::undo::new_batch()));
        std::fs::create_dir_all(d.join("rs/target")).unwrap();
        std::fs::create_dir_all(d.join("docs/target")).unwrap();
        std::fs::create_dir_all(d.join("home/.cache")).unwrap();
        std::fs::write(d.join("rs/Cargo.toml"), "").unwrap();
        let r = |p: &str| rebuildable(&d.join(p), Some(&d.join("home")));
        let got = (r("rs/target").is_some(), r("docs/target").is_some(), r("home/.cache").is_some(), r("rs/.cache").is_some());
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(got, (true, false, true, false)); // `target` only next to Cargo.toml; `.cache` only in home
    }
}
