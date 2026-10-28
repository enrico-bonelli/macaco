//! Line diff between two texts, with word-level detail for changed lines.

use similar::{Algorithm, DiffTag, TextDiff};
use std::ops::Range;
use std::time::Duration;

/// Changed lines are compared word by word only for blocks up to this size.
const MAX_WORD_PAIRS: usize = 400;
const MAX_WORD_LINE: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HunkKind {
    /// Lines only in the new (right) text.
    Added,
    /// Lines only in the old (left) text.
    Removed,
    /// Lines that differ on both sides.
    Changed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub kind: HunkKind,
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// Byte range of a changed word within a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Default, Debug)]
pub struct DiffResult {
    pub hunks: Vec<Hunk>,
    pub old_words: Vec<Span>,
    pub new_words: Vec<Span>,
    /// Every operation, equal blocks included: (old range, new range).
    ops: Vec<(Range<usize>, Range<usize>)>,
}

impl DiffResult {
    /// Lines added, removed and changed (old side + new side).
    pub fn stats(&self) -> (usize, usize, usize) {
        let mut s = (0, 0, 0);
        for h in &self.hunks {
            match h.kind {
                HunkKind::Added => s.0 += h.new.len(),
                HunkKind::Removed => s.1 += h.old.len(),
                HunkKind::Changed => s.2 += h.old.len().max(h.new.len()),
            }
        }
        s
    }

    /// Line of the other side corresponding to `line` (for synchronized
    /// scrolling). `from_old` tells which side `line` belongs to.
    pub fn map_line(&self, line: usize, from_old: bool) -> usize {
        for (o, n) in &self.ops {
            let (src, dst) = if from_old { (o, n) } else { (n, o) };
            if src.contains(&line) {
                let off = line - src.start;
                // Proportional inside blocks of different size.
                return dst.start + off * dst.len() / src.len();
            }
        }
        let last = self.ops.last().map_or(0, |(o, n)| if from_old { n.end } else { o.end });
        last.max(1) - 1
    }
}

/// Unified diff (`git diff` style, 3 lines of context) from `old` to `new`;
/// empty if the texts are equal.
pub fn unified(old: &str, new: &str, old_name: &str, new_name: &str) -> String {
    let diff = TextDiff::configure().algorithm(Algorithm::Histogram).timeout(Duration::from_millis(500)).diff_lines(old, new);
    if diff.ratio() == 1.0 {
        return String::new();
    }
    diff.unified_diff().context_radius(3).header(old_name, new_name).to_string()
}

/// Compare two texts line by line.
pub fn compute(old: &str, new: &str) -> DiffResult {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Histogram)
        .timeout(Duration::from_millis(300))
        .diff_slices(&old_lines, &new_lines);
    let mut res = DiffResult::default();
    let mut word_pairs = 0;
    for op in diff.ops() {
        let (tag, o, n) = op.as_tag_tuple();
        res.ops.push((o.clone(), n.clone()));
        let kind = match tag {
            DiffTag::Equal => continue,
            DiffTag::Insert => HunkKind::Added,
            DiffTag::Delete => HunkKind::Removed,
            DiffTag::Replace => HunkKind::Changed,
        };
        if kind == HunkKind::Changed {
            for (a, b) in o.clone().zip(n.clone()) {
                if word_pairs >= MAX_WORD_PAIRS {
                    break;
                }
                word_pairs += 1;
                word_diff(old_lines[a], new_lines[b], a, b, &mut res);
            }
        }
        // Consecutive operations of the same kind are merged.
        match res.hunks.last_mut() {
            Some(h) if h.kind == kind && h.old.end == o.start && h.new.end == n.start => {
                h.old.end = o.end;
                h.new.end = n.end;
            }
            _ => res.hunks.push(Hunk { kind, old: o, new: n }),
        }
    }
    res
}

/// A region of a three-way merge, as line ranges of the three texts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeChunk {
    pub kind: MergeKind,
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeKind {
    /// Unchanged on both sides.
    Same,
    /// Changed only in ours / only in theirs: that version is taken.
    Ours,
    Theirs,
    /// The same change on both sides.
    Both,
    /// Different changes to the same lines: to be resolved.
    Conflict,
}

/// Result of `merge3`: the regions and the merged text, where conflicts
/// keep the common ancestor's lines.
pub struct Merge3 {
    #[cfg_attr(not(test), allow(dead_code))]
    pub chunks: Vec<MergeChunk>,
    pub text: String,
    pub conflicts: usize,
}

/// Changed regions of `side` with respect to `base`: (base lines, side lines).
fn changes(base: &[&str], side: &[&str]) -> Vec<(Range<usize>, Range<usize>)> {
    TextDiff::configure()
        .algorithm(Algorithm::Histogram)
        .timeout(Duration::from_millis(500))
        .diff_slices(base, side)
        .ops()
        .iter()
        .map(|op| op.as_tag_tuple())
        .filter(|(tag, _, _)| *tag != DiffTag::Equal)
        .map(|(_, o, n)| (o, n))
        .collect()
}

/// Three-way merge of the changes made to `base` in `ours` and in
/// `theirs` (like `git merge`): a change made on one side only is taken,
/// overlapping or adjacent different changes are conflicts.
pub fn merge3(base: &str, ours: &str, theirs: &str) -> Merge3 {
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let o: Vec<&str> = ours.split_inclusive('\n').collect();
    let t: Vec<&str> = theirs.split_inclusive('\n').collect();
    // (base range, side range, is ours)
    let mut all: Vec<(Range<usize>, Range<usize>, bool)> = changes(&b, &o).into_iter().map(|(x, y)| (x, y, true)).collect();
    all.extend(changes(&b, &t).into_iter().map(|(x, y)| (x, y, false)));
    all.sort_by_key(|(x, _, ours)| (x.start, x.end, !ours));

    let mut chunks = Vec::new();
    let (mut bp, mut op, mut tp) = (0, 0, 0);
    let mut i = 0;
    while i < all.len() {
        // A cluster: changes that overlap or touch in the base.
        let (bs, mut be) = (all[i].0.start, all[i].0.end);
        let mut j = i + 1;
        while j < all.len() && all[j].0.start <= be {
            be = be.max(all[j].0.end);
            j += 1;
        }
        let cluster = &all[i..j];
        if bs > bp {
            let n = bs - bp;
            chunks.push(MergeChunk { kind: MergeKind::Same, base: bp..bs, ours: op..op + n, theirs: tp..tp + n });
            op += n;
            tp += n;
        }
        // The range of a side covering the base span bs..be.
        let span = |mine: bool, pos: usize| -> Option<Range<usize>> {
            let h: Vec<_> = cluster.iter().filter(|c| c.2 == mine).collect();
            let (first, last) = (h.first()?, h.last()?);
            let start = first.1.start - (first.0.start - bs);
            let end = last.1.end + (be - last.0.end);
            debug_assert_eq!(start, pos);
            Some(start..end)
        };
        let ours_r = span(true, op);
        let theirs_r = span(false, tp);
        let kind = match (&ours_r, &theirs_r) {
            (Some(_), None) => MergeKind::Ours,
            (None, Some(_)) => MergeKind::Theirs,
            (Some(x), Some(y)) if o[x.clone()] == t[y.clone()] => MergeKind::Both,
            _ => MergeKind::Conflict,
        };
        let ours_r = ours_r.unwrap_or(op..op + (be - bs));
        let theirs_r = theirs_r.unwrap_or(tp..tp + (be - bs));
        op = ours_r.end;
        tp = theirs_r.end;
        chunks.push(MergeChunk { kind, base: bs..be, ours: ours_r, theirs: theirs_r });
        bp = be;
        i = j;
    }
    if bp < b.len() {
        let n = b.len() - bp;
        chunks.push(MergeChunk { kind: MergeKind::Same, base: bp..b.len(), ours: op..op + n, theirs: tp..tp + n });
    }
    let mut text = String::with_capacity(base.len().max(ours.len()));
    let mut conflicts = 0;
    for c in &chunks {
        let lines = match c.kind {
            MergeKind::Same | MergeKind::Conflict => &b[c.base.clone()],
            MergeKind::Ours | MergeKind::Both => &o[c.ours.clone()],
            MergeKind::Theirs => &t[c.theirs.clone()],
        };
        conflicts += (c.kind == MergeKind::Conflict) as usize;
        for l in lines {
            text.push_str(l);
        }
    }
    Merge3 { chunks, text, conflicts }
}

fn word_diff(a: &str, b: &str, la: usize, lb: usize, res: &mut DiffResult) {
    if a.len() > MAX_WORD_LINE || b.len() > MAX_WORD_LINE {
        return;
    }
    let d = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .timeout(Duration::from_millis(5))
        .diff_words(a, b);
    let offsets = |slices: Vec<&str>| {
        let mut acc = 0;
        let mut v = Vec::with_capacity(slices.len() + 1);
        v.push(0);
        for s in slices {
            acc += s.len();
            v.push(acc);
        }
        v
    };
    let oa = offsets(d.iter_old_slices().collect());
    let ob = offsets(d.iter_new_slices().collect());
    for op in d.ops() {
        let (tag, o, n) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        if !o.is_empty() && !a[oa[o.start]..oa[o.end]].trim().is_empty() {
            res.old_words.push(Span { line: la, start: oa[o.start], end: oa[o.end] });
        }
        if !n.is_empty() && !b[ob[n.start]..ob[n.end]].trim().is_empty() {
            res.new_words.push(Span { line: lb, start: ob[n.start], end: ob[n.end] });
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unified_text() {
        assert_eq!(super::unified("a\nb\n", "a\nb\n", "x", "y"), "");
        let u = super::unified("a\nb\nc\n", "a\nB\nc\n", "vecchio", "attuale");
        assert!(u.starts_with("--- vecchio\n+++ attuale\n@@ -1,3 +1,3 @@"), "{u}");
        assert!(u.contains("-b\n+B\n"));
    }

    use super::*;

    #[test]
    fn hunks_and_words() {
        let old = "a\nb\nc\nd\ne";
        let new = "a\nB x\nc\ne\nf";
        let r = compute(old, new);
        assert_eq!(
            r.hunks,
            vec![
                Hunk { kind: HunkKind::Changed, old: 1..2, new: 1..2 },
                Hunk { kind: HunkKind::Removed, old: 3..4, new: 3..3 },
                Hunk { kind: HunkKind::Added, old: 5..5, new: 4..5 },
            ]
        );
        assert_eq!(r.stats(), (1, 1, 1));
        assert_eq!(r.old_words, vec![Span { line: 1, start: 0, end: 1 }]);
        assert!(r.new_words.iter().all(|s| s.line == 1));
        // Line mapping across the removed line.
        assert_eq!(r.map_line(4, true), 3);
        assert_eq!(r.map_line(3, false), 4);
        assert_eq!(r.map_line(2, true), 2);
    }

    #[test]
    fn three_way() {
        let base = "a\nb\nc\nd\ne\nf\ng\n";
        // Separate changes are both taken.
        let m = merge3(base, "a\nB\nc\nd\ne\nf\ng\n", "a\nb\nc\nd\ne\nF\ng\n");
        assert_eq!((m.text.as_str(), m.conflicts), ("a\nB\nc\nd\ne\nF\ng\n", 0));
        // The same change twice is not a conflict.
        let m = merge3(base, "a\nX\nc\nd\ne\nf\ng\n", "a\nX\nc\nd\ne\nf\ng\n");
        assert_eq!(m.conflicts, 0);
        assert!(m.chunks.iter().any(|c| c.kind == MergeKind::Both));
        // Different changes to the same line: the base line is kept.
        let m = merge3(base, "a\nb\nOURS\nd\ne\nf\ng\n", "a\nb\nTHEIRS\nTHEIRS2\nd\ne\nf\ng\n");
        assert_eq!((m.text.as_str(), m.conflicts), (base, 1));
        let c = m.chunks.iter().find(|c| c.kind == MergeKind::Conflict).unwrap();
        assert_eq!((c.base.clone(), c.ours.clone(), c.theirs.clone()), (2..3, 2..3, 2..4));
        // Lines added at the end on both sides, differently.
        let m = merge3("x\n", "x\nuno\n", "x\ndue\n");
        assert_eq!((m.text.as_str(), m.conflicts), ("x\n", 1));
        // Ranges of every region line up on the three texts.
        let m = merge3(base, "a\nb\nc\nnuova\nd\ne\ng\n", "z\na\nb\nc\nd\ne\nf\ng\n");
        assert_eq!((m.text.as_str(), m.conflicts), ("z\na\nb\nc\nnuova\nd\ne\ng\n", 0));
        let last = m.chunks.last().unwrap();
        assert_eq!((last.base.end, last.ours.end, last.theirs.end), (7, 7, 8));
    }

    #[test]
    fn identical() {
        let r = compute("x\ny\n", "x\ny\n");
        assert!(r.hunks.is_empty());
        assert_eq!(r.map_line(1, true), 1);
    }
}
