//! Two-directory comparison by relative path (A vs B).
use crate::scan::{FileEntry, HashCache};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    OnlyA,
    OnlyB,
    Same,
    NewerA,
    NewerB,
    /// Different content, same mtime.
    Differ,
}

pub struct Row {
    pub rel: PathBuf,
    pub a: Option<FileEntry>,
    pub b: Option<FileEntry>,
    pub state: State,
}

/// `files` from a scan of exactly two roots. `content`: equal size alone isn't "same", hash too.
pub fn compare(files: &[FileEntry], roots: &[PathBuf], content: bool, cache: &HashCache) -> Vec<Row> {
    let mut m: BTreeMap<PathBuf, (Option<FileEntry>, Option<FileEntry>)> = BTreeMap::new();
    for f in files.iter().filter(|f| f.root < 2) {
        let rel = f.path.strip_prefix(&roots[f.root]).unwrap_or(&f.path).to_path_buf();
        let e = m.entry(rel).or_default();
        if f.root == 0 { e.0 = Some(f.clone()) } else { e.1 = Some(f.clone()) }
    }
    m.into_iter()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(rel, (a, b))| {
            let state = match (&a, &b) {
                (Some(_), None) => State::OnlyA,
                (None, Some(_)) => State::OnlyB,
                (Some(x), Some(y)) => {
                    let same = x.size == y.size && if content { cache.full(x).is_some() && cache.full(x) == cache.full(y) } else { x.mtime == y.mtime };
                    if same {
                        State::Same
                    } else if x.mtime > y.mtime {
                        State::NewerA
                    } else if y.mtime > x.mtime {
                        State::NewerB
                    } else {
                        State::Differ
                    }
                }
                (None, None) => unreachable!(),
            };
            Row { rel, a, b, state }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::*;

    #[test]
    fn sync_states() {
        let d = tmpdir("sync");
        std::fs::write(d.join("a/same"), "1").unwrap();
        std::fs::write(d.join("b/same"), "1").unwrap();
        std::fs::write(d.join("a/onlya"), "1").unwrap();
        std::fs::write(d.join("b/onlyb"), "1").unwrap();
        std::fs::write(d.join("a/chg"), "old").unwrap();
        std::fs::write(d.join("b/chg"), "new!").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        std::fs::File::options().write(true).open(d.join("a/chg")).unwrap().set_modified(old).unwrap();
        let roots = vec![d.join("a"), d.join("b")];
        let files = collect(&ScanOpts { roots: roots.clone(), min_size: 0, exclude: vec![], follow_hidden: true }, &Progress::default());
        let rows = compare(&files, &roots, true, &HashCache::default());
        let st: Vec<_> = rows.iter().map(|r| (r.rel.to_string_lossy().into_owned(), r.state)).collect();
        assert_eq!(st, vec![
            ("chg".into(), State::NewerB),
            ("onlya".into(), State::OnlyA),
            ("onlyb".into(), State::OnlyB),
            ("same".into(), State::Same),
        ]);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
