//! Text diff + per-hunk conflict resolution. Two-way (left vs right) or
//! three-way (left and right both derived from a common base, git-style).
use similar::{DiffTag, TextDiff};
use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Pick {
    Left,
    Right,
    LeftThenRight,
    RightThenLeft,
    Base,
    Neither,
}

pub enum Chunk {
    Same(Vec<String>),
    Diff {
        left: Vec<String>,
        right: Vec<String>,
        /// Common ancestor text (three-way only).
        base: Option<Vec<String>>,
        pick: Pick,
        /// False when three-way merge resolved it automatically (only one side changed, or both identically).
        conflict: bool,
    },
}

pub struct Merge {
    pub chunks: Vec<Chunk>,
}

fn lines(s: &str) -> Vec<String> {
    s.split_inclusive('\n').map(String::from).collect()
}

/// Changed regions of `other` relative to `base`: (base line range, replacement lines).
fn hunks(base: &str, other: &str) -> Vec<(Range<usize>, Vec<String>)> {
    let ol = lines(other);
    let mut out: Vec<(Range<usize>, Vec<String>)> = vec![];
    for op in TextDiff::from_lines(base, other).ops() {
        let (tag, br, or) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        let new = ol[or].to_vec();
        match out.last_mut() {
            // Coalesce adjacent delete+insert into one hunk.
            Some((r, l)) if r.end == br.start => {
                r.end = br.end;
                l.extend(new);
            }
            _ => out.push((br, new)),
        }
    }
    out
}

impl Merge {
    pub fn new(left: &str, right: &str) -> Self {
        let bl = lines(left);
        let mut chunks = vec![];
        let mut pos = 0;
        for (r, new) in hunks(left, right) {
            chunks.push(Chunk::Same(bl[pos..r.start].to_vec()));
            chunks.push(Chunk::Diff { left: bl[r.clone()].to_vec(), right: new, base: None, pick: Pick::Left, conflict: true });
            pos = r.end;
        }
        chunks.push(Chunk::Same(bl[pos..].to_vec()));
        chunks.retain(|c| !matches!(c, Chunk::Same(v) if v.is_empty()));
        Self { chunks }
    }

    pub fn new3(base: &str, left: &str, right: &str) -> Self {
        let bl = lines(base);
        let mut all: Vec<(Range<usize>, bool, Vec<String>)> =
            hunks(base, left).into_iter().map(|(r, l)| (r, true, l)).chain(hunks(base, right).into_iter().map(|(r, l)| (r, false, l))).collect();
        all.sort_by_key(|h| (h.0.start, h.0.end));
        let mut chunks = vec![];
        let mut pos = 0;
        let mut i = 0;
        while i < all.len() {
            // Cluster overlapping/touching hunks (conservative, like diff3).
            let (start, mut end) = (all[i].0.start, all[i].0.end);
            let mut j = i + 1;
            while j < all.len() && all[j].0.start <= end {
                end = end.max(all[j].0.end);
                j += 1;
            }
            let group = &all[i..j];
            let side = |is_left: bool| -> Vec<String> {
                let mut out = vec![];
                let mut p = start;
                for (r, _, l) in group.iter().filter(|h| h.1 == is_left) {
                    out.extend_from_slice(&bl[p..r.start]);
                    out.extend(l.iter().cloned());
                    p = r.end;
                }
                out.extend_from_slice(&bl[p..end]);
                out
            };
            let (has_l, has_r) = (group.iter().any(|h| h.1), group.iter().any(|h| !h.1));
            let (l, r) = (side(true), side(false));
            let conflict = has_l && has_r && l != r;
            let pick = if has_l { Pick::Left } else { Pick::Right };
            chunks.push(Chunk::Same(bl[pos..start].to_vec()));
            chunks.push(Chunk::Diff { left: l, right: r, base: Some(bl[start..end].to_vec()), pick, conflict });
            pos = end;
            i = j;
        }
        chunks.push(Chunk::Same(bl[pos..].to_vec()));
        chunks.retain(|c| !matches!(c, Chunk::Same(v) if v.is_empty()));
        Self { chunks }
    }

    pub fn diffs(&self) -> usize {
        self.chunks.iter().filter(|c| matches!(c, Chunk::Diff { .. })).count()
    }

    pub fn conflicts(&self) -> usize {
        self.chunks.iter().filter(|c| matches!(c, Chunk::Diff { conflict: true, .. })).count()
    }

    /// Set pick on every true conflict (auto-resolved hunks keep their pick).
    pub fn pick_all(&mut self, p: Pick) {
        for c in &mut self.chunks {
            if let Chunk::Diff { pick, conflict: true, .. } = c {
                *pick = p;
            }
        }
    }

    pub fn result(&self) -> String {
        let mut s = String::new();
        let mut push = |v: &[String]| {
            for x in v {
                s += x;
                // Keep line boundary when a non-final last line lacked '\n'.
                if !x.ends_with('\n') {
                    s.push('\n');
                }
            }
        };
        for c in &self.chunks {
            match c {
                Chunk::Same(l) => push(l),
                Chunk::Diff { left, right, base, pick, .. } => match pick {
                    Pick::Left => push(left),
                    Pick::Right => push(right),
                    Pick::LeftThenRight => {
                        push(left);
                        push(right)
                    }
                    Pick::RightThenLeft => {
                        push(right);
                        push(left)
                    }
                    Pick::Base => push(base.as_deref().unwrap_or(&[])),
                    Pick::Neither => {}
                },
            }
        }
        // Don't invent a trailing newline the chosen source didn't have.
        let last_line_unterminated = match self.chunks.last() {
            Some(Chunk::Same(v)) => v.last().is_some_and(|x| !x.ends_with('\n')),
            Some(Chunk::Diff { left, right, base, pick, .. }) => {
                let v = match pick {
                    Pick::Left | Pick::RightThenLeft => left,
                    Pick::Right | Pick::LeftThenRight => right,
                    Pick::Base => base.as_ref().unwrap_or(left),
                    Pick::Neither => return s,
                };
                v.last().is_some_and(|x| !x.ends_with('\n'))
            }
            None => false,
        };
        if last_line_unterminated {
            s.pop();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_way_picks() {
        let l = "a\nb\nc\nd\n";
        let r = "a\nB\nc\nd\ne\n";
        let mut m = Merge::new(l, r);
        assert_eq!(m.conflicts(), 2);
        assert_eq!(m.result(), l);
        m.pick_all(Pick::Right);
        assert_eq!(m.result(), r);
        m.pick_all(Pick::LeftThenRight);
        assert_eq!(m.result(), "a\nb\nB\nc\nd\ne\n");
        m.pick_all(Pick::Neither);
        assert_eq!(m.result(), "a\nc\nd\n");
        assert_eq!(Merge::new(l, l).conflicts(), 0);
        let mut m = Merge::new("x\ny", "x\nz");
        assert_eq!(m.result(), "x\ny");
        m.pick_all(Pick::Right);
        assert_eq!(m.result(), "x\nz");
    }

    #[test]
    fn three_way_auto_and_conflict() {
        let base = "1\n2\n3\n4\n5\n6\n7\n";
        let left = "1\nTWO\n3\n4\n5\n6\n7\n"; // changes line 2
        let right = "1\n2\n3\n4\n5\nSIX\n7\n"; // changes line 6
        let m = Merge::new3(base, left, right);
        assert_eq!((m.diffs(), m.conflicts()), (2, 0));
        assert_eq!(m.result(), "1\nTWO\n3\n4\n5\nSIX\n7\n");

        let right2 = "1\nDOS\n3\n4\n5\n6\n7\n"; // same line as left → conflict
        let mut m = Merge::new3(base, left, right2);
        assert_eq!(m.conflicts(), 1);
        m.pick_all(Pick::Right);
        assert_eq!(m.result(), right2);
        m.pick_all(Pick::Base);
        assert_eq!(m.result(), base);
        // Identical change on both sides isn't a conflict.
        assert_eq!(Merge::new3(base, left, left).conflicts(), 0);
    }
}
