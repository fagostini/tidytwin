//! Action journal, undo, quarantine. Journal = JSON lines in the app data dir.
use crate::scan::{Action, hash_full, hex, mirror_into, move_file, tmp_path};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const QUARANTINE_NAME: &str = ".tidytwin-quarantine";

/// `<topmost writable dir on p's filesystem>/.tidytwin-quarantine/<batch>`, like freedesktop's
/// per-volume `.Trash-$uid`: quarantining is then a rename, never a cross-device copy.
pub fn quarantine_dir_for(p: &Path, batch: u64) -> Option<PathBuf> {
    #[cfg(unix)]
    let dev = |x: &Path| std::fs::metadata(x).ok().map(|m| std::os::unix::fs::MetadataExt::dev(&m));
    #[cfg(not(unix))]
    let dev = |_: &Path| Some(0u64); // ponytail: Windows treats a drive letter as one volume; mounted folders fall back to copy.
    let d0 = dev(p.parent()?)?;
    let same: Vec<&Path> = p.ancestors().skip(1).take_while(|a| !a.as_os_str().is_empty() && dev(a) == Some(d0)).collect();
    same.iter().rev().find_map(|a| {
        let top = a.join(QUARANTINE_NAME);
        private_dir(&top).ok()?;
        let q = top.join(batch.to_string());
        std::fs::create_dir_all(&q).ok().map(|_| q)
    })
}

/// Create `d` (and parents) and make it owner-only. Stashed files sit under mirrored paths whose
/// dirs are created 0755; without this, a file protected only by a 0700 parent becomes readable.
fn private_dir(d: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(d)?;
    #[cfg(unix)]
    std::fs::set_permissions(d, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(())
}

/// Move to trash; returns a stash path when we did the move ourselves (restorable by us).
pub fn trash_file(p: &Path) -> Result<Option<PathBuf>, String> {
    // macOS: the trash crate can't restore, so move into ~/.Trash ourselves when on the same volume.
    #[cfg(target_os = "macos")]
    if let Some(h) = std::env::var_os("HOME") {
        let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let dst = PathBuf::from(h).join(".Trash").join(format!("{name} {}", new_batch()));
        if std::fs::rename(p, &dst).is_ok() {
            return Ok(Some(dst));
        }
    }
    trash::delete(p).map(|_| None).map_err(|e| e.to_string())
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::home_dir().map(|h| h.join(".local/share")))
        // Never a shared temp dir: another user could pre-create it and read/plant quarantined files.
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    base.join("tidytwin")
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub batch: u64,
    pub time: SystemTime,
    pub action: Action,
    pub orig: PathBuf,
    /// Kept duplicate the file can be rebuilt from.
    pub keep: Option<PathBuf>,
    /// BLAKE3 hex of the removed file's content (None if not hashed).
    pub hash: Option<String>,
    pub size: u64,
    pub mtime: SystemTime,
    pub perms: u32, // unix mode, or 1 = readonly on other OSes
    /// Where the bytes went: quarantine, move target, merge backup. None once purged.
    pub stash: Option<PathBuf>,
    pub undone: bool,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub quarantine: bool,
    pub retention_days: u32,
    /// Files under these folders are never selected or removed.
    pub protected: Vec<PathBuf>,
    /// Byte-by-byte compare with the kept copy before removing (slower, no hash trust needed).
    pub verify_bytes: bool,
    /// `group_key`s of duplicate groups hidden from results.
    pub ignored: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { quarantine: true, retention_days: 30, protected: system_protected(), verify_bytes: false, ignored: vec![] }
    }
}

/// OS and installed-software folders, protected by default: "duplicates" there are shared libs,
/// package-owned files, app bundles; removing or relinking one breaks the system. User can unprotect.
pub fn system_protected() -> Vec<PathBuf> {
    let v: &[&str] = if cfg!(windows) {
        &["C:\\Windows", "C:\\Program Files", "C:\\Program Files (x86)", "C:\\ProgramData"]
    } else {
        &["/bin", "/sbin", "/lib", "/lib32", "/lib64", "/usr", "/etc", "/boot", "/opt", "/var/lib", "/nix", "/System", "/Library", "/Applications"]
    };
    v.iter().map(PathBuf::from).collect()
}

pub struct Journal {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub settings: Settings,
}

pub fn new_batch() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64
}

pub fn perms_of(md: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    return std::os::unix::fs::PermissionsExt::mode(&md.permissions());
    #[cfg(not(unix))]
    return md.permissions().readonly() as u32;
}

fn set_perms(p: &Path, v: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    let perm = std::os::unix::fs::PermissionsExt::from_mode(v);
    #[cfg(not(unix))]
    let perm = {
        let mut x = std::fs::metadata(p)?.permissions();
        x.set_readonly(v == 1);
        x
    };
    std::fs::set_permissions(p, perm)
}

impl Journal {
    pub fn open(dir: PathBuf) -> Self {
        let entries = load_entries(&dir);
        let settings = std::fs::read_to_string(dir.join("settings.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        // Holds quarantine + backups: create it private before anything lands in it.
        let _ = private_dir(&dir);
        Self { dir, entries, settings }
    }

    pub fn quarantine_dir(&self, batch: u64) -> PathBuf {
        self.dir.join("quarantine").join(batch.to_string())
    }

    /// Save settings only. The journal is written solely under `locked`.
    pub fn save(&self) -> std::io::Result<()> {
        private_dir(&self.dir)?;
        std::fs::write(self.dir.join("settings.json"), serde_json::to_string(&self.settings).unwrap())
    }

    /// Run `f` on the journal as it is on disk, holding an exclusive lock, then write it back.
    /// GUI and CLI (`undo-last`, scheduled scans) can run at once: without the reload, one
    /// process's stale copy would overwrite the other's entries / undone flags, and an entry
    /// could be undone twice.
    // ponytail: rewrites the whole file on each change; fine to ~100k entries, append-only log if it grows past that.
    fn locked<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> std::io::Result<T> {
        private_dir(&self.dir)?;
        let lock = std::fs::File::options().create(true).truncate(false).write(true).open(self.dir.join("journal.lock"))?;
        lock.lock()?; // released when `lock` drops
        self.entries = load_entries(&self.dir);
        let r = f(self);
        let mut s = String::new();
        for e in &self.entries {
            s += &serde_json::to_string(e).unwrap();
            s.push('\n');
        }
        let tmp = self.dir.join("journal.jsonl.tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(tmp, self.dir.join("journal.jsonl"))?;
        Ok(r)
    }

    pub fn record(&mut self, mut es: Vec<Entry>) -> std::io::Result<()> {
        self.locked(|j| j.entries.append(&mut es))
    }

    /// Back up `p` (if it exists) before it gets overwritten. Returns the journal entry to record.
    fn backup(&self, p: &Path, batch: u64) -> Result<Option<Entry>, String> {
        let Ok(md) = std::fs::metadata(p) else { return Ok(None) };
        let bak = mirror_into(p, &self.dir.join("backups").join(batch.to_string()));
        std::fs::create_dir_all(bak.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::copy(p, &bak).map_err(|e| e.to_string())?;
        Ok(Some(Entry {
            batch,
            time: SystemTime::now(),
            action: Action::Overwrite,
            orig: p.to_path_buf(),
            keep: None,
            hash: None,
            size: md.len(),
            mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            perms: perms_of(&md),
            stash: Some(bak),
            undone: false,
        }))
    }

    /// Write text to `p`, backing up the old content first so it can be undone.
    pub fn save_text(&mut self, p: &Path, txt: &str) -> Result<(), String> {
        let mut e = self.backup(p, new_batch())?;
        std::fs::write(p, txt).map_err(|e| e.to_string())?;
        if let Some(e) = &mut e {
            e.hash = Some(blake3::hash(txt.as_bytes()).to_hex().to_string());
        }
        self.record(e.into_iter().collect()).map_err(|e| e.to_string())
    }

    /// Copy `src` to `dst` (sync). Overwrites are backed up; new files are journaled so undo can remove them.
    pub fn copy_over(&self, src: &Path, dst: &Path, batch: u64) -> Result<Entry, String> {
        let mut e = match self.backup(dst, batch)? {
            Some(e) => e,
            None => {
                let md = std::fs::metadata(src).map_err(|e| e.to_string())?;
                Entry {
                    batch,
                    time: SystemTime::now(),
                    action: Action::Created,
                    orig: dst.to_path_buf(),
                    keep: None,
                    hash: None, // set below, from what was actually written
                    size: md.len(),
                    mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    perms: perms_of(&md),
                    stash: None,
                    undone: false,
                }
            }
        };
        std::fs::create_dir_all(dst.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::copy(src, dst).map_err(|e| e.to_string())?;
        // Keep source mtime so the two sides compare equal afterwards.
        let mt = std::fs::metadata(src).and_then(|m| m.modified()).map_err(|e| e.to_string())?;
        std::fs::File::options().write(true).open(dst).and_then(|f| f.set_modified(mt)).map_err(|e| e.to_string())?;
        // Undo checks this: it won't delete (Created) or roll back (Overwrite) a file edited since.
        e.hash = Some(hex(hash_full(dst).map_err(|e| e.to_string())?));
        Ok(e)
    }

    /// Undo every not-yet-undone entry of a batch, newest first. Returns errors.
    pub fn undo_batch(&mut self, batch: u64) -> Vec<String> {
        let r = self.locked(|j| {
            let mut errs = vec![];
            for e in j.entries.iter_mut().rev().filter(|e| e.batch == batch && !e.undone) {
                match undo_entry(e) {
                    Ok(()) => e.undone = true,
                    Err(m) => errs.push(format!("{}: {m}", e.orig.display())),
                }
            }
            errs
        });
        r.unwrap_or_else(|e| vec![format!("journal: {e}")])
    }

    /// Permanently delete quarantined files and merge backups older than `days` (0 = all).
    pub fn purge(&mut self, days: u32) -> std::io::Result<usize> {
        self.locked(|j| j.purge_unlocked(days))
    }

    fn purge_unlocked(&mut self, days: u32) -> usize {
        let cutoff = SystemTime::now() - Duration::from_secs(days as u64 * 86400);
        let mut n = 0;
        for e in &mut self.entries {
            let stash_owned = matches!(e.action, Action::Quarantine | Action::Overwrite) && !e.undone;
            if stash_owned && e.time <= cutoff && let Some(s) = e.stash.take() {
                let _ = if s.is_dir() { std::fs::remove_dir_all(&s) } else { std::fs::remove_file(&s) };
                remove_empty_parents(&s, e.batch);
                n += 1;
            }
        }
        // Drop now-empty batch dirs.
        for sub in ["quarantine", "backups"] {
            if let Ok(rd) = std::fs::read_dir(self.dir.join(sub)) {
                for d in rd.flatten() {
                    let id: u64 = d.file_name().to_string_lossy().parse().unwrap_or(0);
                    if !self.entries.iter().any(|e| e.batch == id && e.stash.is_some() && !e.undone) {
                        let _ = std::fs::remove_dir_all(d.path());
                    }
                }
            }
        }
        n
    }

    pub fn quarantine_bytes(&self) -> u64 {
        self.entries.iter().filter(|e| e.action == Action::Quarantine && !e.undone && e.stash.is_some()).map(|e| e.size).sum()
    }
}

fn load_entries(dir: &Path) -> Vec<Entry> {
    std::fs::read_to_string(dir.join("journal.jsonl")).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Remove now-empty dirs above a purged stash, up to its batch dir and the quarantine root.
fn remove_empty_parents(stash: &Path, batch: u64) {
    let b = batch.to_string();
    for d in stash.ancestors().skip(1) {
        if std::fs::remove_dir(d).is_err() {
            return;
        }
        if d.file_name().is_some_and(|n| n == b.as_str()) {
            if let Some(q) = d.parent().filter(|q| q.ends_with(QUARANTINE_NAME)) {
                let _ = std::fs::remove_dir(q);
            }
            return;
        }
    }
}

#[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
fn trash_restore(p: &Path) -> Result<(), String> {
    let item = trash::os_limited::list()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|i| i.original_path() == p)
        .max_by_key(|i| i.time_deleted)
        .ok_or("not found in trash")?;
    trash::os_limited::restore_all([item]).map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))))]
fn trash_restore(_: &Path) -> Result<(), String> {
    Err("trash restore unsupported on this OS".into())
}

fn undo_entry(e: &Entry) -> Result<(), String> {
    // symlink_metadata, not exists(): a stashed broken symlink is still there.
    let stash = e.stash.as_ref().filter(|s| std::fs::symlink_metadata(s).is_ok());
    let first = match (e.action, stash) {
        (Action::Overwrite, Some(s)) => {
            // Hash = what we wrote; a mismatch means the user edited it since. (Old entries: None, unchecked.)
            if let (Some(want), Ok(h)) = (&e.hash, hash_full(&e.orig))
                && &hex(h) != want
            {
                return Err("file changed since it was written; not rolling back".into());
            }
            return std::fs::copy(s, &e.orig).map(|_| ()).map_err(|x| x.to_string());
        }
        (Action::Overwrite, None) => return Err("backup purged".into()),
        (Action::Created, _) => {
            let h = hash_full(&e.orig).map_err(|x| x.to_string())?;
            if Some(hex(h)) != e.hash {
                return Err("file changed since it was copied; not deleting".into());
            }
            return std::fs::remove_file(&e.orig).map_err(|x| x.to_string());
        }
        (Action::Trash, None) => trash_restore(&e.orig),
        (Action::Quarantine | Action::MoveTo | Action::Trash, Some(s)) => {
            if std::fs::symlink_metadata(&e.orig).is_ok() {
                return Err("original path is occupied".into());
            }
            std::fs::create_dir_all(e.orig.parent().unwrap()).map_err(|x| x.to_string())?;
            move_file(s, &e.orig).map_err(|x| x.to_string())?;
            // Only our quarantine tree is ours to tidy; a MoveTo/Trash stash sits in user dirs.
            if e.action == Action::Quarantine {
                remove_empty_parents(s, e.batch);
            }
            Ok(())
        }
        _ => Err("no stash".into()),
    };
    first.or_else(|why| rebuild_from_keep(e).map_err(|w| format!("{why}; rebuild from kept copy failed: {w}")))
}

/// Recreate the removed file by copying the kept duplicate, verifying its hash.
fn rebuild_from_keep(e: &Entry) -> Result<(), String> {
    let keep = e.keep.as_ref().ok_or("no kept copy recorded")?;
    let want = e.hash.as_ref().ok_or("no hash recorded")?;
    // A hardlinked dup legitimately still exists at orig; anything else must not be clobbered.
    if !matches!(e.action, Action::Hardlink | Action::Reflink) && e.orig.exists() {
        return Err("original path is occupied".into());
    }
    let got = hash_full(keep).map_err(|x| format!("kept copy unreadable: {x}"))?;
    if &hex(got) != want {
        return Err("kept copy content differs (changed, or was never identical)".into());
    }
    std::fs::create_dir_all(e.orig.parent().unwrap()).map_err(|x| x.to_string())?;
    let tmp = tmp_path(&e.orig);
    let r = (|| {
        std::fs::copy(keep, &tmp)?;
        set_perms(&tmp, e.perms)?;
        std::fs::File::options().write(true).open(&tmp)?.set_modified(e.mtime)?;
        std::fs::rename(&tmp, &e.orig)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.map_err(|x| x.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::*;
    use std::collections::HashSet;

    #[test]
    fn quarantine_delete_merge_undo_purge() {
        let d = std::env::temp_dir().join(format!("tidytwin-undo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for s in ["a", "b", "c"] {
            std::fs::create_dir_all(d.join(s)).unwrap();
        }
        for s in ["a", "b", "c"] {
            std::fs::write(d.join(s).join("f.txt"), "same content\n").unwrap();
        }
        let mut j = Journal::open(d.join("data"));
        let prog = Progress::default();
        let opts = ScanOpts { roots: vec![d.join("a"), d.join("b"), d.join("c")], min_size: 1, exclude: vec![], follow_hidden: true };
        let groups = find_dups(collect(&opts, &prog), Criteria::default(), false, &prog, &HashCache::default());
        assert_eq!(groups[0].len(), 3);

        // Quarantine b, delete c.
        let qb = new_batch();
        let sel: HashSet<_> = [d.join("b/f.txt")].into();
        let (es, errs) = apply(&groups, &sel, &ApplyOpts { action: Some(Action::Quarantine), dest: Some(j.quarantine_dir(qb)), batch: qb, ..Default::default() });
        assert!(errs.is_empty(), "{errs:?}");
        j.record(es).unwrap();
        let db = qb + 1;
        let sel: HashSet<_> = [d.join("c/f.txt")].into();
        let (es, errs) = apply(&groups, &sel, &ApplyOpts { action: Some(Action::Delete), dest: None, batch: db, ..Default::default() });
        assert!(errs.is_empty(), "{errs:?}");
        j.record(es).unwrap();
        assert!(!d.join("b/f.txt").exists() && !d.join("c/f.txt").exists());
        assert_eq!(j.quarantine_bytes(), 13);

        // Journal survives reopen; undo both.
        let mut j = Journal::open(d.join("data"));
        let qst = j.entries.iter().find(|e| e.batch == qb).unwrap().stash.clone().unwrap();
        assert!(j.undo_batch(qb).is_empty());
        assert!(qst.ancestors().find(|a| a.ends_with(qb.to_string())).is_some_and(|b| !b.exists()), "undo cleans quarantine dirs");
        assert!(j.undo_batch(db).is_empty());
        assert_eq!(std::fs::read_to_string(d.join("c/f.txt")).unwrap(), "same content\n");
        assert!(d.join("b/f.txt").exists());

        // Merge save undo restores old content.
        j.save_text(&d.join("a/f.txt"), "merged\n").unwrap();
        let mb = j.entries.last().unwrap().batch;
        assert!(j.undo_batch(mb).is_empty());
        assert_eq!(std::fs::read_to_string(d.join("a/f.txt")).unwrap(), "same content\n");

        // Quarantine again, purge → undo falls back to rebuilding from kept copy.
        let q2 = new_batch();
        let sel: HashSet<_> = [d.join("b/f.txt")].into();
        let (es, _) = apply(&groups, &sel, &ApplyOpts { action: Some(Action::Quarantine), dest: Some(j.quarantine_dir(q2)), batch: q2, ..Default::default() });
        j.record(es).unwrap();
        let st = j.entries.last().unwrap().stash.clone().unwrap();
        assert!(st.to_string_lossy().contains(QUARANTINE_NAME), "same-filesystem quarantine used: {st:?}");
        assert_eq!(j.purge(0).unwrap(), 1);
        assert!(!st.exists());
        assert!(st.ancestors().find(|a| a.ends_with(q2.to_string())).is_some_and(|b| !b.exists()), "batch dir cleaned");
        assert!(j.undo_batch(q2).is_empty());
        assert!(d.join("b/f.txt").exists());

        // Sync copy: new file + overwrite, both undoable.
        let nb = new_batch();
        let e1 = j.copy_over(&d.join("a/f.txt"), &d.join("c/new.txt"), nb).unwrap();
        std::fs::write(d.join("b/f.txt"), "b-version\n").unwrap();
        let e2 = j.copy_over(&d.join("a/f.txt"), &d.join("b/f.txt"), nb).unwrap();
        j.record(vec![e1, e2]).unwrap();
        assert!(j.undo_batch(nb).is_empty());
        assert!(!d.join("c/new.txt").exists());
        assert_eq!(std::fs::read_to_string(d.join("b/f.txt")).unwrap(), "b-version\n");
        std::fs::write(d.join("b/f.txt"), "same content\n").unwrap();

        // Kept copy changed → refuse rebuild.
        let sel: HashSet<_> = [d.join("c/f.txt")].into();
        let (es, _) = apply(&groups, &sel, &ApplyOpts { action: Some(Action::Delete), dest: None, batch: 7, ..Default::default() });
        j.record(es).unwrap();
        std::fs::write(d.join("a/f.txt"), "edited").unwrap();
        assert_eq!(j.undo_batch(7).len(), 1);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn two_journal_handles_dont_clobber() {
        let d = std::env::temp_dir().join(format!("tidytwin-jlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.txt"), "v1").unwrap();
        let (mut gui, mut cli) = (Journal::open(d.join("data")), Journal::open(d.join("data")));
        gui.save_text(&d.join("f.txt"), "v2").unwrap();
        let b = gui.entries[0].batch;
        // CLI opened before the save, still undoes it (reloads under lock)...
        assert!(cli.undo_batch(b).is_empty());
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "v1");
        // ...and the GUI's stale copy neither re-undoes it nor erases the undone flag.
        assert!(gui.undo_batch(b).is_empty());
        gui.record(vec![]).unwrap();
        assert!(Journal::open(d.join("data")).entries[0].undone);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn moveto_and_overwrite_undo_safety() {
        let d = std::env::temp_dir().join(format!("tidytwin-undo2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for s in ["a", "b", "dest/keepme"] {
            std::fs::create_dir_all(d.join(s)).unwrap();
        }
        std::fs::write(d.join("a/f.txt"), "x").unwrap();
        std::fs::write(d.join("b/f.txt"), "x").unwrap();
        let mut j = Journal::open(d.join("data"));
        #[cfg(unix)]
        assert!(is_protected(Path::new("/usr/lib/libc.so.6"), &Settings::default().protected) && !is_protected(&d, &Settings::default().protected));
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(d.join("data")).unwrap().permissions()) & 0o777, 0o700);
        let prog = Progress::default();
        let opts = ScanOpts { roots: vec![d.join("a"), d.join("b")], min_size: 1, exclude: vec![], follow_hidden: true };
        let groups = find_dups(collect(&opts, &prog), Criteria::default(), false, &prog, &HashCache::default());
        let sel: HashSet<_> = [d.join("b/f.txt")].into();
        let mv = |batch| ApplyOpts { action: Some(Action::MoveTo), dest: Some(d.join("dest/keepme")), batch, ..Default::default() };

        // MoveTo, undo: user's (now empty again) dest folder must survive.
        let (es, errs) = apply(&groups, &sel, &mv(1));
        assert!(errs.is_empty(), "{errs:?}");
        j.record(es).unwrap();
        assert!(j.undo_batch(1).is_empty());
        assert!(d.join("b/f.txt").exists() && d.join("dest/keepme").is_dir());

        // MoveTo onto an occupied target: refused, nothing overwritten.
        let t = mirror_into(&d.join("b/f.txt"), &d.join("dest/keepme"));
        std::fs::create_dir_all(t.parent().unwrap()).unwrap();
        std::fs::write(&t, "precious").unwrap();
        let (es, errs) = apply(&groups, &sel, &mv(2));
        assert_eq!((es.len(), errs.len()), (0, 1));
        assert_eq!(std::fs::read_to_string(&t).unwrap(), "precious");

        // Overwrite undo refuses to clobber edits made after the save.
        j.save_text(&d.join("a/f.txt"), "merged").unwrap();
        let b = j.entries.last().unwrap().batch;
        std::fs::write(d.join("a/f.txt"), "edited later").unwrap();
        assert_eq!(j.undo_batch(b).len(), 1);
        assert_eq!(std::fs::read_to_string(d.join("a/f.txt")).unwrap(), "edited later");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
