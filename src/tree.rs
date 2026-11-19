//! Directory aggregation: duplicate folders + disk-usage tree for the treemap.
use crate::scan::{FileEntry, Group, HashCache, Progress, refine, sort_groups};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Folders whose scanned contents (relative paths + bytes) are identical.
/// Only maximal folders are reported: if `x/` and `y/` match, `x/sub` vs `y/sub` is not repeated.
/// Folders are compared on the files the scan saw (excludes / min-size / hidden apply).
// ponytail: indexes every file under every ancestor (files × depth memory); fine for home dirs, a merkle pass if whole-disk folder dedup is slow.
pub fn dup_dirs(files: &[FileEntry], roots: &[PathBuf], prog: &Progress, cache: &HashCache) -> Vec<Group> {
    let mut by_dir: HashMap<&Path, Vec<(PathBuf, usize)>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        let root = &roots[f.root];
        for anc in f.path.ancestors().skip(1) {
            if !anc.starts_with(root) {
                break;
            }
            by_dir.entry(anc).or_default().push((f.path.strip_prefix(anc).unwrap().to_path_buf(), i));
        }
    }
    // Stage 1: same relative names + sizes.
    let mut m: HashMap<Vec<(PathBuf, u64)>, Vec<(&Path, Vec<usize>)>> = HashMap::new();
    for (dir, mut v) in by_dir {
        v.sort();
        let key = v.iter().map(|(r, i)| (r.clone(), files[*i].size)).collect();
        m.entry(key).or_default().push((dir, v.into_iter().map(|x| x.1).collect()));
    }
    let cands: Vec<_> = m.into_values().filter(|g| g.len() > 1).collect();
    // Stage 2: same content (full hashes, cached).
    let groups = refine(cands, prog, |(_, idx): &(&Path, Vec<usize>)| idx.iter().map(|&i| cache.full(&files[i])).collect::<Option<Vec<_>>>());
    let in_dup: HashSet<&Path> = groups.iter().flatten().map(|(d, _)| *d).collect();
    let mut out: Vec<Group> = groups
        .into_iter()
        .filter(|g| !g.iter().all(|(d, _)| d.parent().is_some_and(|p| in_dup.contains(p))))
        .map(|g| {
            g.into_iter()
                .map(|(d, idx)| FileEntry {
                    path: d.to_path_buf(),
                    size: idx.iter().map(|&i| files[i].size).sum(),
                    mtime: idx.iter().map(|&i| files[i].mtime).max().unwrap(),
                    root: files[idx[0]].root,
                })
                .collect()
        })
        .collect();
    sort_groups(&mut out);
    out
}

pub struct Node {
    pub name: OsString,
    pub size: u64,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub dir: bool,
}

/// Arena tree of scanned bytes. Node 0 = virtual root holding each scan root.
pub struct Tree {
    pub nodes: Vec<Node>,
}

impl Tree {
    pub fn build(files: &[FileEntry], roots: &[PathBuf]) -> Self {
        let mut nodes = vec![Node { name: "all roots".into(), size: 0, parent: None, children: vec![], dir: true }];
        let mut idx: HashMap<(usize, OsString), usize> = HashMap::new();
        let mut child = |nodes: &mut Vec<Node>, parent: usize, name: OsString, dir: bool| -> usize {
            *idx.entry((parent, name.clone())).or_insert_with(|| {
                nodes.push(Node { name, size: 0, parent: Some(parent), children: vec![], dir });
                let id = nodes.len() - 1;
                nodes[parent].children.push(id);
                id
            })
        };
        let root_ids: Vec<usize> = roots.iter().map(|r| child(&mut nodes, 0, r.as_os_str().into(), true)).collect();
        for f in files {
            let rel = f.path.strip_prefix(&roots[f.root]).unwrap_or(&f.path);
            let mut cur = root_ids[f.root];
            let comps: Vec<_> = rel.components().collect();
            for (i, c) in comps.iter().enumerate() {
                cur = child(&mut nodes, cur, c.as_os_str().into(), i + 1 < comps.len());
            }
            // Propagate size up.
            let mut n = Some(cur);
            while let Some(i) = n {
                nodes[i].size += f.size;
                n = nodes[i].parent;
            }
        }
        for n in &mut nodes {
            n.children.shrink_to_fit();
        }
        let mut t = Tree { nodes };
        // Children biggest first (treemap layout wants it).
        for i in 0..t.nodes.len() {
            let mut c = std::mem::take(&mut t.nodes[i].children);
            c.sort_by_key(|&k| std::cmp::Reverse(t.nodes[k].size));
            t.nodes[i].children = c;
        }
        t
    }

    pub fn path(&self, mut i: usize) -> PathBuf {
        let mut parts = vec![];
        while let Some(p) = self.nodes[i].parent {
            parts.push(self.nodes[i].name.clone());
            i = p;
        }
        parts.iter().rev().collect()
    }
}

/// Squarified treemap (Bruls et al.). `vals` sorted descending. Returns [x, y, w, h] per value.
pub fn squarify(vals: &[f64], x: f64, y: f64, w: f64, h: f64) -> Vec<[f64; 4]> {
    let total: f64 = vals.iter().sum();
    if total <= 0.0 || w <= 0.0 || h <= 0.0 {
        return vec![[x, y, 0.0, 0.0]; vals.len()];
    }
    let scale = w * h / total;
    let areas: Vec<f64> = vals.iter().map(|v| v * scale).collect();
    let (mut x, mut y, mut w, mut h) = (x, y, w, h);
    let mut out = Vec::with_capacity(vals.len());
    let mut i = 0;
    let worst = |row: &[f64], side: f64| {
        let s: f64 = row.iter().sum();
        let (mx, mn) = row.iter().fold((0f64, f64::MAX), |(a, b), &v| (a.max(v), b.min(v)));
        (side * side * mx / (s * s)).max(s * s / (side * side * mn))
    };
    while i < areas.len() {
        let side = w.min(h);
        let mut j = i + 1;
        while j < areas.len() && worst(&areas[i..=j], side) <= worst(&areas[i..j], side) {
            j += 1;
        }
        let row = &areas[i..j];
        let s: f64 = row.iter().sum();
        if w >= h {
            let cw = s / h;
            let mut yy = y;
            for a in row {
                out.push([x, yy, cw, a / cw]);
                yy += a / cw;
            }
            x += cw;
            w -= cw;
        } else {
            let rh = s / w;
            let mut xx = x;
            for a in row {
                out.push([xx, y, a / rh, rh]);
                xx += a / rh;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::*;

    #[test]
    fn squarify_fills_area() {
        let r = squarify(&[6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0], 0.0, 0.0, 6.0, 4.0);
        let area: f64 = r.iter().map(|q| q[2] * q[3]).sum();
        assert!((area - 24.0).abs() < 1e-9);
        assert!(r.iter().all(|q| q[0] >= -1e-9 && q[1] >= -1e-9 && q[0] + q[2] <= 6.0 + 1e-9 && q[1] + q[3] <= 4.0 + 1e-9));
    }

    #[test]
    fn finds_dup_folders_and_tree() {
        let d = tmpdir("dirs");
        for top in ["a/proj", "b/copy"] {
            std::fs::create_dir_all(d.join(top).join("sub")).unwrap();
            std::fs::write(d.join(top).join("x.txt"), "x").unwrap();
            std::fs::write(d.join(top).join("sub/y.txt"), "yy").unwrap();
        }
        std::fs::create_dir_all(d.join("a/other/sub")).unwrap();
        std::fs::write(d.join("a/other/sub/y.txt"), "yy").unwrap(); // only sub/ matches → not maximal elsewhere
        let roots = vec![d.join("a"), d.join("b")];
        let files = collect(&ScanOpts { roots: roots.clone(), min_size: 0, exclude: vec![], follow_hidden: true }, &Progress::default());
        let g = dup_dirs(&files, &roots, &Progress::default(), &HashCache::default());
        let mut paths: Vec<Vec<PathBuf>> = g.iter().map(|g| g.iter().map(|f| f.path.clone()).collect()).collect();
        paths.sort();
        assert_eq!(paths, vec![
            vec![d.join("a/other/sub"), d.join("a/proj/sub"), d.join("b/copy/sub")],
            vec![d.join("a/proj"), d.join("b/copy")],
        ]);
        // proj/sub group is reported because a/other/sub's parent isn't a dup.
        let t = Tree::build(&files, &roots);
        assert_eq!(t.nodes[0].size, 3 + 3 + 2);
        let a = t.nodes[0].children.iter().copied().find(|&i| t.path(i) == d.join("a")).unwrap();
        assert_eq!(t.nodes[a].size, 5);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
