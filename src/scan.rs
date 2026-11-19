//! Scanning, duplicate detection, file actions. No GUI deps — testable alone.
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
    /// Index of the scan root this file came from (used by two-dir compare).
    pub root: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Criteria {
    pub name: bool,
    pub size: bool,
    pub mtime: bool,
    pub content: bool,
}

impl Default for Criteria {
    fn default() -> Self {
        Self { name: false, size: true, mtime: false, content: true }
    }
}

#[derive(Default)]
pub struct Progress {
    pub files_seen: AtomicUsize,
    pub hashed: AtomicUsize,
    pub cancel: AtomicBool,
}

#[derive(Clone)]
pub struct ScanOpts {
    pub roots: Vec<PathBuf>,
    pub min_size: u64,
    pub exclude: Vec<String>, // substrings matched against full path
    pub follow_hidden: bool,
}

/// Saved scan output: written by the CLI / scheduled scans, loadable in the GUI.
#[derive(Serialize, Deserialize)]
pub struct Report {
    pub created: SystemTime,
    pub roots: Vec<PathBuf>,
    pub scanned: usize,
    pub secs: f32,
    pub groups: Vec<Group>,
    pub largest: Vec<FileEntry>,
}

/// Default excludes for whole-system scans: pseudo-filesystems that are huge or infinite.
pub const SYSTEM_EXCLUDES: &[&str] = &["/proc/", "/sys/", "/dev/", "/run/", "/snap/", "/var/lib/docker/"];

pub fn collect(opts: &ScanOpts, prog: &Progress) -> Vec<FileEntry> {
    let mut out = Vec::new();
    // Skip hardlinks to the same inode: they are not wasted space and
    // "deleting a duplicate" would delete nothing.
    let mut seen_inodes = HashSet::new();
    let data = crate::undo::data_dir();
    for (ri, root) in opts.roots.iter().enumerate() {
        let walker = walkdir::WalkDir::new(root).follow_links(false);
        let it = walker.into_iter().filter_entry(|e| {
            let p = e.path().to_string_lossy();
            let hidden = e.depth() > 0 && e.file_name().to_string_lossy().starts_with('.');
            // Never scan our own quarantine/backups: they'd show up as duplicates.
            e.file_name() != crate::undo::QUARANTINE_NAME
                && !e.path().starts_with(&data)
                && (opts.follow_hidden || !hidden)
                && !opts.exclude.iter().any(|x| !x.is_empty() && p.contains(x.as_str()))
        });
        for e in it.flatten() {
            if prog.cancel.load(Ordering::Relaxed) {
                return out;
            }
            if !e.file_type().is_file() {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            if md.len() < opts.min_size {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if !seen_inodes.insert((md.dev(), md.ino())) {
                    continue;
                }
            }
            #[cfg(not(unix))]
            let _ = &mut seen_inodes;
            prog.files_seen.fetch_add(1, Ordering::Relaxed);
            out.push(FileEntry {
                path: e.into_path(),
                size: md.len(),
                mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                root: ri,
            });
        }
    }
    out
}

pub fn largest(files: &[FileEntry], n: usize) -> Vec<FileEntry> {
    let mut v = files.to_vec();
    v.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    v.truncate(n);
    v
}

const PARTIAL: usize = 16 * 1024;

fn hash_partial(p: &Path, size: u64) -> std::io::Result<[u8; 32]> {
    let mut f = std::fs::File::open(p)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; PARTIAL];
    let n = f.read(&mut buf)?;
    h.update(&buf[..n]);
    // Head + tail catches files with shared headers (media containers, zips).
    if size > (2 * PARTIAL) as u64 {
        f.seek(SeekFrom::End(-(PARTIAL as i64)))?;
        let n = f.read(&mut buf)?;
        h.update(&buf[..n]);
    }
    Ok(*h.finalize().as_bytes())
}

pub fn hash_full(p: &Path) -> std::io::Result<[u8; 32]> {
    let mut h = blake3::Hasher::new();
    h.update_reader(std::fs::File::open(p)?)?;
    Ok(*h.finalize().as_bytes())
}

pub fn hex(h: [u8; 32]) -> String {
    blake3::Hash::from(h).to_hex().to_string()
}

/// Byte-by-byte equality, for users who don't want to trust a hash alone.
pub fn files_equal(a: &Path, b: &Path) -> std::io::Result<bool> {
    let (fa, fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ra, mut rb) = (std::io::BufReader::with_capacity(1 << 20, fa), std::io::BufReader::with_capacity(1 << 20, fb));
    loop {
        let (ba, bb) = (ra.fill_buf()?, rb.fill_buf()?);
        if ba.is_empty() || bb.is_empty() {
            return Ok(ba.is_empty() && bb.is_empty());
        }
        let n = ba.len().min(bb.len());
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
        ra.consume(n);
        rb.consume(n);
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct CacheVal {
    size: u64,
    mtime: Option<SystemTime>,
    partial: Option<[u8; 32]>,
    full: Option<[u8; 32]>,
}

/// Persistent hash cache keyed by path; entries are valid while size+mtime match.
#[derive(Default)]
pub struct HashCache {
    map: Mutex<HashMap<PathBuf, CacheVal>>,
    path: Option<PathBuf>,
}

impl HashCache {
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::File::open(&path)
            .map(|f| {
                std::io::BufReader::new(f)
                    .lines()
                    .map_while(Result::ok)
                    .filter_map(|l| serde_json::from_str::<(PathBuf, CacheVal)>(&l).ok())
                    .collect()
            })
            .unwrap_or_default();
        Self { map: Mutex::new(map), path: Some(path) }
    }

    // ponytail: JSON lines, whole-file rewrite, stale paths never pruned; switch to sqlite/redb past a few million files.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(p) = &self.path else { return Ok(()) };
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("tmp");
        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        for kv in self.map.lock().unwrap().iter() {
            serde_json::to_writer(&mut w, &kv)?;
            w.write_all(b"\n")?;
        }
        w.flush()?;
        drop(w);
        std::fs::rename(tmp, p)
    }

    /// Forget files under `roots` that this scan didn't see (deleted, moved, now excluded).
    pub fn prune(&self, roots: &[PathBuf], seen: &[FileEntry]) {
        let seen: HashSet<&Path> = seen.iter().map(|f| f.path.as_path()).collect();
        self.map.lock().unwrap().retain(|p, _| seen.contains(p.as_path()) || !roots.iter().any(|r| p.starts_with(r)));
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    fn get(&self, f: &FileEntry, full: bool, compute: impl FnOnce() -> std::io::Result<[u8; 32]>) -> Option<[u8; 32]> {
        {
            let m = self.map.lock().unwrap();
            if let Some(v) = m.get(&f.path).filter(|v| v.size == f.size && v.mtime == Some(f.mtime)) {
                let h = if full { v.full } else { v.partial };
                if h.is_some() {
                    return h;
                }
            }
        }
        let h = compute().ok()?;
        let mut m = self.map.lock().unwrap();
        let v = m.entry(f.path.clone()).or_default();
        if v.size != f.size || v.mtime != Some(f.mtime) {
            *v = CacheVal { size: f.size, mtime: Some(f.mtime), ..Default::default() };
        }
        if full { v.full = Some(h) } else { v.partial = Some(h) }
        Some(h)
    }

    pub fn partial(&self, f: &FileEntry) -> Option<[u8; 32]> {
        self.get(f, false, || hash_partial(&f.path, f.size))
    }

    pub fn full(&self, f: &FileEntry) -> Option<[u8; 32]> {
        self.get(f, true, || hash_full(&f.path))
    }
}

/// A group of files (or directories) considered duplicates of each other.
pub type Group = Vec<FileEntry>;

/// Regroup each group by a fallible key computed in parallel; drop singletons & unreadable files.
pub fn refine<T, K, F>(groups: Vec<Vec<T>>, prog: &Progress, key: F) -> Vec<Vec<T>>
where
    T: Send,
    K: std::hash::Hash + Eq + Send,
    F: Fn(&T) -> Option<K> + Sync,
{
    groups
        .into_par_iter()
        .flat_map(|g| {
            if prog.cancel.load(Ordering::Relaxed) {
                return vec![];
            }
            let keyed: Vec<(K, T)> = g
                .into_par_iter()
                .filter_map(|f| {
                    let k = key(&f);
                    prog.hashed.fetch_add(1, Ordering::Relaxed);
                    k.map(|k| (k, f))
                })
                .collect();
            let mut m: HashMap<K, Vec<T>> = HashMap::new();
            for (k, f) in keyed {
                m.entry(k).or_default().push(f);
            }
            m.into_values().filter(|g| g.len() > 1).collect::<Vec<_>>()
        })
        .collect()
}

/// Find duplicate groups. If `cross_roots`, keep only groups spanning ≥2 roots
/// (two-directory comparison mode).
pub fn find_dups(files: Vec<FileEntry>, c: Criteria, cross_roots: bool, prog: &Progress, cache: &HashCache) -> Vec<Group> {
    // Content equality implies size equality; use size as cheap pre-filter.
    let use_size = c.size || c.content;
    let mut m: HashMap<(Option<std::ffi::OsString>, Option<u64>, Option<SystemTime>), Group> = HashMap::new();
    for f in files {
        let k = (
            c.name.then(|| f.path.file_name().unwrap_or_default().to_os_string()),
            use_size.then_some(f.size),
            c.mtime.then_some(f.mtime),
        );
        m.entry(k).or_default().push(f);
    }
    let mut groups: Vec<Group> = m.into_values().filter(|g| g.len() > 1).collect();
    if cross_roots {
        groups.retain(|g| g.iter().any(|f| f.root != g[0].root));
    }
    if c.content {
        groups = refine(groups, prog, |f| cache.partial(f));
        // Small files: partial hash (first PARTIAL bytes) already covered the whole file.
        let (small, big): (Vec<_>, Vec<_>) = groups.into_iter().partition(|g| g[0].size <= PARTIAL as u64);
        groups = small;
        groups.extend(refine(big, prog, |f| cache.full(f)));
    }
    if cross_roots {
        groups.retain(|g| g.iter().any(|f| f.root != g[0].root));
    }
    sort_groups(&mut groups);
    groups
}

pub fn sort_groups(groups: &mut [Group]) {
    for g in groups.iter_mut() {
        g.sort_by(|a, b| a.path.cmp(&b.path));
    }
    // Most wasted space first.
    groups.sort_by_key(|g| std::cmp::Reverse(g[0].size * (g.len() as u64 - 1)));
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum KeepRule {
    Oldest,
    Newest,
    ShortestPath,
    FirstRoot,
}

pub fn is_protected(p: &Path, protected: &[PathBuf]) -> bool {
    protected.iter().any(|d| p.starts_with(d))
}

/// Mark everything for removal except one file per group chosen by `rule`.
/// Files under `protected` dirs are never selected; a group containing one keeps it.
pub fn auto_select(groups: &[Group], rule: KeepRule, protected: &[PathBuf]) -> HashSet<PathBuf> {
    let mut sel = HashSet::new();
    for g in groups {
        let keep = if g.iter().any(|f| is_protected(&f.path, protected)) {
            None
        } else {
            match rule {
                KeepRule::Oldest => g.iter().min_by_key(|f| f.mtime),
                KeepRule::Newest => g.iter().max_by_key(|f| f.mtime),
                KeepRule::ShortestPath => g.iter().min_by_key(|f| f.path.as_os_str().len()),
                KeepRule::FirstRoot => g.iter().min_by_key(|f| f.root),
            }
        };
        let keep = keep.map(|f| &f.path);
        sel.extend(g.iter().filter(|f| Some(&f.path) != keep && !is_protected(&f.path, protected)).map(|f| f.path.clone()));
    }
    sel
}

/// True if applying `sel` would remove every copy in some group (data loss).
pub fn removes_all_copies(groups: &[Group], sel: &HashSet<PathBuf>) -> bool {
    let doomed = doomed(groups, sel);
    groups.iter().any(|g| g.iter().all(|f| removed(&f.path, &doomed)))
}

/// Selected paths that `apply` on these groups will actually remove.
fn doomed<'a>(groups: &'a [Group], sel: &HashSet<PathBuf>) -> Vec<&'a Path> {
    groups.iter().flatten().filter(|f| sel.contains(&f.path)).map(|f| f.path.as_path()).collect()
}

/// `p` goes away: selected itself, or inside a selected folder (nested folder groups).
// ponytail: O(files × selected); a sorted prefix lookup if selections reach the 100k range.
fn removed(p: &Path, doomed: &[&Path]) -> bool {
    doomed.iter().any(|d| p.starts_with(d))
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum Action {
    Trash,
    Delete,
    /// Move into the app's quarantine dir; purged after retention period.
    Quarantine,
    /// Replace duplicate with a hardlink to the kept copy (same filesystem only).
    Hardlink,
    /// Replace duplicate with a copy-on-write clone (Btrfs, XFS, APFS, ReFS): separate file, shared blocks.
    Reflink,
    MoveTo,
    /// Journal-only: an existing file overwritten (merge save, sync copy). Stash = backup.
    #[serde(alias = "MergeSave")]
    Overwrite,
    /// Journal-only: a new file created by sync copy. Undo deletes it.
    Created,
}

#[derive(Default)]
pub struct ApplyOpts {
    pub action: Option<Action>,
    /// Target dir for MoveTo; fallback for Quarantine when the file's filesystem has no writable spot.
    pub dest: Option<PathBuf>,
    pub batch: u64,
    pub protected: Vec<PathBuf>,
    /// Compare bytes with the kept copy instead of trusting BLAKE3 alone.
    pub verify_bytes: bool,
}

/// Apply action to selected files. Kept copy per group = first unselected file.
/// Every removed file is checked against the kept copy first: a file that changed since
/// the scan (or was never identical, e.g. name-only / similar-image match) is refused for
/// Hardlink/Reflink/Delete and recorded as non-rebuildable for the recoverable actions.
/// Directories (folder-duplicate groups) support only Trash / Quarantine / MoveTo.
pub fn apply(groups: &[Group], sel: &HashSet<PathBuf>, o: &ApplyOpts) -> (Vec<crate::undo::Entry>, Vec<String>) {
    let action = o.action.expect("action");
    let mut done = Vec::new();
    let mut errs = Vec::new();
    let doomed = doomed(groups, sel);
    for g in groups {
        // Kept copy must survive the whole run, not sit inside a folder removed by another group.
        let Some(keep) = g.iter().find(|f| !removed(&f.path, &doomed)) else {
            errs.push(format!("skipped group (all copies selected): {}", g[0].path.display()));
            continue;
        };
        let keep_hash = OnceCell::new();
        for f in g.iter().filter(|f| sel.contains(&f.path)) {
            let r = (|| -> Result<crate::undo::Entry, String> {
                if is_protected(&f.path, &o.protected) {
                    return Err("in a protected folder".into());
                }
                let md = std::fs::symlink_metadata(&f.path).map_err(|e| e.to_string())?;
                let (hash, same) = if md.is_dir() {
                    if !matches!(action, Action::Trash | Action::Quarantine | Action::MoveTo) {
                        return Err(format!("{action:?} not supported for folders"));
                    }
                    (None, false)
                } else {
                    let h = hash_full(&f.path).map_err(|e| e.to_string())?;
                    let kh = keep_hash.get_or_init(|| hash_full(&keep.path).ok());
                    let same = *kh == Some(h) && (!o.verify_bytes || files_equal(&f.path, &keep.path).unwrap_or(false));
                    (Some(hex(h)), same)
                };
                if matches!(action, Action::Hardlink | Action::Reflink) && !same {
                    return Err("content differs from kept copy; refusing to link".into());
                }
                if action == Action::Delete && !md.is_dir() && !same {
                    return Err("content differs from kept copy; refusing permanent delete (use Trash or Quarantine)".into());
                }
                let stash = match action {
                    Action::Trash => crate::undo::trash_file(&f.path)?,
                    Action::Delete => std::fs::remove_file(&f.path).map(|_| None).map_err(|e| e.to_string())?,
                    Action::Hardlink => replace_with(&f.path, |tmp| std::fs::hard_link(&keep.path, tmp)).map(|_| None).map_err(|e| e.to_string())?,
                    Action::Reflink => replace_with(&f.path, |tmp| {
                        reflink_copy::reflink(&keep.path, tmp)?;
                        std::fs::set_permissions(tmp, md.permissions())?;
                        std::fs::File::options().write(true).open(tmp)?.set_modified(md.modified()?)
                    })
                    .map(|_| None)
                    .map_err(|e| format!("reflink (needs CoW filesystem): {e}"))?,
                    Action::MoveTo | Action::Quarantine => {
                        // Quarantine on the file's own filesystem → instant rename; `dest` is the fallback.
                        let qd = (action == Action::Quarantine).then(|| crate::undo::quarantine_dir_for(&f.path, o.batch)).flatten();
                        let t = mirror_into(&f.path, qd.as_deref().or(o.dest.as_deref()).expect("dest"));
                        // rename() silently replaces an existing target.
                        if std::fs::symlink_metadata(&t).is_ok() {
                            return Err(format!("{} already exists; not overwriting", t.display()));
                        }
                        std::fs::create_dir_all(t.parent().unwrap()).map_err(|e| e.to_string())?;
                        move_file(&f.path, &t).map_err(|e| e.to_string())?;
                        Some(t)
                    }
                    Action::Overwrite | Action::Created => unreachable!(),
                };
                Ok(crate::undo::Entry {
                    batch: o.batch,
                    time: SystemTime::now(),
                    action,
                    orig: f.path.clone(),
                    keep: same.then(|| keep.path.clone()),
                    hash,
                    size: f.size,
                    mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    perms: crate::undo::perms_of(&md),
                    stash,
                    undone: false,
                })
            })();
            match r {
                Ok(e) => done.push(e),
                Err(e) => errs.push(format!("{}: {e}", f.path.display())),
            }
        }
    }
    (done, errs)
}

use std::cell::OnceCell;

/// Build replacement at a temp name, then atomic rename: `dup` is never missing.
fn replace_with(dup: &Path, make: impl FnOnce(&Path) -> std::io::Result<()>) -> std::io::Result<()> {
    let tmp = tmp_path(dup);
    make(&tmp).and_then(|_| std::fs::rename(&tmp, dup)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Unique sibling temp name (`f.txt` → `f.txt.tidytwin-tmp-<nanos>`): never an existing user file,
/// so cleaning it up on error can't delete anything we didn't create.
pub fn tmp_path(p: &Path) -> PathBuf {
    let mut n = p.file_name().unwrap_or_default().to_os_string();
    n.push(format!(".tidytwin-tmp-{}", crate::undo::new_batch()));
    p.with_file_name(n)
}

/// `/a/b/c.txt` under `dir` → `dir/a/b/c.txt` (avoids name collisions).
pub fn mirror_into(src: &Path, dir: &Path) -> PathBuf {
    let rel: PathBuf = src.components().filter(|c| matches!(c, std::path::Component::Normal(_))).collect();
    dir.join(rel)
}

/// Rename, falling back to copy+delete across filesystems (files only).
pub fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Err(e) = std::fs::rename(src, dst) {
        if src.is_dir() {
            return Err(std::io::Error::other(format!("cannot move folder across filesystems: {e}")));
        }
        std::fs::copy(src, dst)?;
        let _ = std::fs::File::options().write(true).open(dst).and_then(|f| f.set_modified(std::fs::metadata(src)?.modified()?));
        std::fs::remove_file(src)?;
    }
    Ok(())
}

pub fn to_csv(groups: &[Group]) -> String {
    let mut s = String::from("group,size,mtime_unix,path\n");
    for (i, g) in groups.iter().enumerate() {
        for f in g {
            let t = f.mtime.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let p = f.path.display().to_string().replace('"', "\"\"");
            s += &format!("{i},{},{t},\"{p}\"\n", f.size);
        }
    }
    s
}

/// Heuristic: no NUL bytes in first 8 KiB and valid UTF-8 there.
pub fn is_text(p: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let mut buf = [0u8; 8192];
    let n = f.read(&mut buf).unwrap_or(0);
    let b = &buf[..n];
    !b.contains(&0) && (std::str::from_utf8(b).is_ok() || std::str::from_utf8(&b[..n.saturating_sub(3)]).is_ok())
}

pub fn human(n: u64) -> String {
    let u = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < u.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{f:.1} {}", u[i]) }
}

#[cfg(test)]
pub fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tidytwin-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("a")).unwrap();
    std::fs::create_dir_all(d.join("b")).unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_content_dups_and_cross_root() {
        let d = tmpdir("dups");
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut big2 = big.clone();
        big2[50_000] ^= 1; // same size, same head/tail, differs in middle
        std::fs::write(d.join("a/x.bin"), &big).unwrap();
        std::fs::write(d.join("b/y.bin"), &big).unwrap();
        std::fs::write(d.join("a/z.bin"), &big2).unwrap();
        std::fs::write(d.join("a/s1.txt"), "hello").unwrap();
        std::fs::write(d.join("a/s2.txt"), "hello").unwrap();
        let prog = Progress::default();
        let cache = HashCache::load(d.join("cache.jsonl"));
        let opts = ScanOpts { roots: vec![d.join("a"), d.join("b")], min_size: 1, exclude: vec![], follow_hidden: true };
        let files = collect(&opts, &prog);
        assert_eq!(files.len(), 5);

        let g = find_dups(files.clone(), Criteria::default(), false, &prog, &cache);
        assert_eq!(g.len(), 2, "{g:?}");
        assert_eq!(g[0].len(), 2); // big pair first (more wasted bytes); z.bin excluded by full hash
        assert!(g[0].iter().all(|f| f.path.extension().unwrap() == "bin"));
        assert!(!files_equal(&d.join("a/x.bin"), &d.join("a/z.bin")).unwrap());
        assert!(files_equal(&d.join("a/x.bin"), &d.join("b/y.bin")).unwrap());

        // Cache persists and is reused (second run hashes nothing new).
        cache.save().unwrap();
        let cache = HashCache::load(d.join("cache.jsonl"));
        assert!(cache.len() >= 3);
        let before = cache.len();
        let g = find_dups(files.clone(), Criteria::default(), true, &prog, &cache);
        assert_eq!(cache.len(), before);
        assert_eq!(g.len(), 1, "only x/y span both roots");

        // Name-only: nothing shares a name.
        let c = Criteria { name: true, size: false, mtime: false, content: false };
        assert!(find_dups(files, c, false, &prog, &cache).is_empty());

        // Protected folder: never selected, and apply refuses it.
        let g = find_dups(collect(&opts, &prog), Criteria::default(), true, &prog, &cache);
        let prot = vec![d.join("b")];
        let sel = auto_select(&g, KeepRule::FirstRoot, &prot);
        assert_eq!(sel, [d.join("a/x.bin")].into(), "protected b/y kept, a/x selected");
        let forced: HashSet<_> = [d.join("b/y.bin")].into();
        let (ok, errs) = apply(&g, &forced, &ApplyOpts { action: Some(Action::Delete), protected: prot, ..Default::default() });
        assert_eq!((ok.len(), errs.len()), (0, 1));

        // Hardlink with byte verify.
        let sel = auto_select(&g, KeepRule::FirstRoot, &[]);
        assert!(removes_all_copies(&g, &g[0].iter().map(|f| f.path.clone()).collect()));
        let (ok, errs) = apply(&g, &sel, &ApplyOpts { action: Some(Action::Hardlink), verify_bytes: true, ..Default::default() });
        assert_eq!((ok.len(), errs.len()), (1, 0), "{errs:?}");
        assert_eq!(std::fs::read(d.join("b/y.bin")).unwrap(), big);
        // Now hardlinked → rescan dedupes by inode.
        assert_eq!(collect(&opts, &prog).len(), 4);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn nested_selection_keeps_a_copy() {
        let f = |p: &str| FileEntry { path: PathBuf::from(p), size: 1, mtime: SystemTime::UNIX_EPOCH, root: 0 };
        // Folder group: /x/p vs /y/p. File group: /x/p/f vs /z/f.
        let groups = vec![vec![f("/x/p"), f("/y/p")], vec![f("/x/p/f"), f("/z/f")]];
        // Removing /x/p (folder) and /z/f leaves no copy of f: /x/p/f goes with its folder.
        let sel: HashSet<_> = [PathBuf::from("/x/p"), PathBuf::from("/z/f")].into();
        assert!(removes_all_copies(&groups, &sel));
        assert!(!removes_all_copies(&groups, &[PathBuf::from("/x/p")].into()));
    }

    #[test]
    fn never_permanently_deletes_differing_files() {
        let d = tmpdir("nodelete");
        // 16–32 KiB, differs only past the first PARTIAL bytes: used to be grouped as identical.
        let mut v = vec![7u8; 20_000];
        std::fs::write(d.join("a/mid"), &v).unwrap();
        v[19_999] = 8;
        std::fs::write(d.join("b/mid"), &v).unwrap();
        // Same name+size, different content: a legit name/size-only group.
        std::fs::write(d.join("a/n.txt"), "aaaa").unwrap();
        std::fs::write(d.join("b/n.txt"), "bbbb").unwrap();
        let prog = Progress::default();
        let opts = ScanOpts { roots: vec![d.join("a"), d.join("b")], min_size: 1, exclude: vec![], follow_hidden: true };
        let files = collect(&opts, &prog);
        assert!(find_dups(files.clone(), Criteria::default(), false, &prog, &HashCache::default()).is_empty());

        let c = Criteria { name: true, size: true, mtime: false, content: false };
        let g = find_dups(files, c, false, &prog, &HashCache::default());
        assert_eq!(g.len(), 2);
        let sel: HashSet<_> = [d.join("b/mid"), d.join("b/n.txt")].into();
        let (ok, errs) = apply(&g, &sel, &ApplyOpts { action: Some(Action::Delete), batch: 1, ..Default::default() });
        assert_eq!((ok.len(), errs.len()), (0, 2), "{errs:?}");
        assert!(d.join("b/mid").exists() && d.join("b/n.txt").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
