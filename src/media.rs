//! Similar (not byte-identical) images and music.
use crate::scan::{FileEntry, Group, Progress};
use rayon::prelude::*;
use std::sync::atomic::Ordering;

const IMG_EXT: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff"];
const AUDIO_EXT: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "aac", "wav", "wv", "ape", "mpc", "aiff"];

fn has_ext(f: &FileEntry, exts: &[&str]) -> bool {
    f.path.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
}

/// Union-find: groups of indices connected by `pairs`.
fn cluster(n: usize, pairs: impl IntoIterator<Item = (usize, usize)>) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..n).collect();
    fn root(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for (i, j) in pairs {
        let (a, b) = (root(&mut parent, i), root(&mut parent, j));
        parent[a] = b;
    }
    let mut m: std::collections::HashMap<usize, Vec<usize>> = Default::default();
    for i in 0..n {
        let r = root(&mut parent, i);
        m.entry(r).or_default().push(i);
    }
    m.into_values().filter(|g| g.len() > 1).collect()
}

/// BK-tree over a metric: finds all items within distance `t` without comparing every pair.
struct BkTree<'a, T> {
    items: &'a [T],
    dist: fn(&T, &T) -> u32,
    nodes: Vec<(usize, Vec<(u32, usize)>)>, // (item index, children by edge distance)
}

impl<'a, T> BkTree<'a, T> {
    fn new(items: &'a [T], dist: fn(&T, &T) -> u32) -> Self {
        let mut t = Self { items, dist, nodes: Vec::with_capacity(items.len()) };
        for i in 0..items.len() {
            t.insert(i);
        }
        t
    }

    fn insert(&mut self, i: usize) {
        if self.nodes.is_empty() {
            self.nodes.push((i, vec![]));
            return;
        }
        let mut cur = 0;
        loop {
            let d = (self.dist)(&self.items[self.nodes[cur].0], &self.items[i]);
            match self.nodes[cur].1.iter().find(|c| c.0 == d) {
                Some(&(_, next)) => cur = next,
                None => {
                    self.nodes.push((i, vec![]));
                    let id = self.nodes.len() - 1;
                    self.nodes[cur].1.push((d, id));
                    return;
                }
            }
        }
    }

    fn within(&self, q: &T, t: u32) -> Vec<usize> {
        let mut out = vec![];
        let mut stack = if self.nodes.is_empty() { vec![] } else { vec![0] };
        while let Some(n) = stack.pop() {
            let (item, ref kids) = self.nodes[n];
            let d = (self.dist)(&self.items[item], q);
            if d <= t {
                out.push(item);
            }
            stack.extend(kids.iter().filter(|c| c.0 + t >= d && c.0 <= d + t).map(|c| c.1));
        }
        out
    }
}

/// Group visually similar images. `max_dist` = Hamming distance on a 64-bit gradient hash
/// (0 = near-identical, ~10 = resized/recompressed/lightly edited).
pub fn similar_images(files: &[FileEntry], max_dist: u32, prog: &Progress) -> Vec<Group> {
    prog.begin(files.iter().filter(|f| has_ext(f, IMG_EXT)).count());
    let hasher = image_hasher::HasherConfig::new().hash_size(8, 8).hash_alg(image_hasher::HashAlg::Gradient).to_hasher();
    let hashed: Vec<(FileEntry, image_hasher::ImageHash)> = files
        .par_iter()
        .filter(|f| has_ext(f, IMG_EXT))
        .filter_map(|f| {
            if prog.cancel.load(Ordering::Relaxed) {
                return None;
            }
            prog.hashed.fetch_add(1, Ordering::Relaxed);
            let img = image::open(&f.path).ok()?;
            Some((f.clone(), hasher.hash_image(&img)))
        })
        .collect();
    let hashes: Vec<image_hasher::ImageHash> = hashed.iter().map(|h| h.1.clone()).collect();
    let bk = BkTree::new(&hashes, |a, b| a.dist(b));
    let pairs: Vec<(usize, usize)> = (0..hashes.len()).into_par_iter().flat_map_iter(|i| bk.within(&hashes[i], max_dist).into_iter().filter(move |&j| j > i).map(move |j| (i, j))).collect();
    cluster(hashes.len(), pairs)
        .into_iter()
        .map(|g| g.into_iter().map(|i| hashed[i].0.clone()).collect())
        .collect()
}

#[derive(Debug)]
struct Track {
    artist: String,
    title: String,
    secs: u64,
}

fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Group songs with the same normalized artist+title and duration within `tol_secs`.
// ponytail: tag-based like dupeGuru's music mode; acoustic fingerprinting (rusty-chromaprint + symphonia) when untagged libraries matter.
pub fn similar_audio(files: &[FileEntry], tol_secs: u64, prog: &Progress) -> Vec<Group> {
    use lofty::prelude::*;
    prog.begin(files.iter().filter(|f| has_ext(f, AUDIO_EXT)).count());
    let tracks: Vec<(FileEntry, Track)> = files
        .par_iter()
        .filter(|f| has_ext(f, AUDIO_EXT))
        .filter_map(|f| {
            if prog.cancel.load(Ordering::Relaxed) {
                return None;
            }
            prog.hashed.fetch_add(1, Ordering::Relaxed);
            let tf = lofty::read_from_path(&f.path).ok()?;
            let tag = tf.primary_tag().or_else(|| tf.first_tag())?;
            let t = Track {
                artist: norm(&tag.artist()?),
                title: norm(&tag.title()?),
                secs: tf.properties().duration().as_secs(),
            };
            (!t.title.is_empty()).then(|| (f.clone(), t))
        })
        .collect();
    // Bucket by artist+title, then chain tracks whose durations are within tolerance: O(n log n).
    let mut buckets: std::collections::HashMap<(&str, &str), Vec<usize>> = Default::default();
    for (i, (_, t)) in tracks.iter().enumerate() {
        buckets.entry((&t.artist, &t.title)).or_default().push(i);
    }
    let mut pairs = vec![];
    for mut b in buckets.into_values() {
        b.sort_by_key(|&i| tracks[i].1.secs);
        pairs.extend(b.windows(2).filter(|w| tracks[w[1]].1.secs - tracks[w[0]].1.secs <= tol_secs).map(|w| (w[0], w[1])));
    }
    cluster(tracks.len(), pairs)
        .into_iter()
        .map(|g| g.into_iter().map(|i| tracks[i].0.clone()).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bktree_matches_bruteforce() {
        let items: Vec<u64> = (0..500u64).map(|i| i.wrapping_mul(0x9E3779B97F4A7C15) >> 40).collect();
        let bk = BkTree::new(&items, |a, b| (a ^ b).count_ones());
        for q in items.iter().take(50) {
            let mut got = bk.within(q, 6);
            got.sort();
            let want: Vec<usize> = (0..items.len()).filter(|&j| (items[j] ^ q).count_ones() <= 6).collect();
            assert_eq!(got, want);
        }
    }

    #[test]
    fn similar_images_found() {
        let d = std::env::temp_dir().join(format!("tidytwin-img-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        // Same gradient at two sizes (similar); inverted gradient (different).
        let grad = |w: u32, inv: bool| {
            image::RgbImage::from_fn(w, w, |x, y| {
                let v = ((x + y) * 255 / (2 * w)) as u8;
                let v = if inv { 255 - v } else { v };
                image::Rgb([v, v / 2, 255 - v / 3])
            })
        };
        grad(256, false).save(d.join("a.png")).unwrap();
        grad(100, false).save(d.join("b.jpg")).unwrap();
        grad(256, true).save(d.join("c.png")).unwrap();
        let files: Vec<FileEntry> = ["a.png", "b.jpg", "c.png"]
            .iter()
            .map(|n| FileEntry { path: d.join(n), size: 1, mtime: std::time::SystemTime::UNIX_EPOCH, root: 0 })
            .collect();
        let g = similar_images(&files, 6, &Progress::default());
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(g[0].len(), 2);
        assert!(g[0].iter().all(|f| !f.path.ends_with("c.png")));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
