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
    fn identical() {
        let r = compute("x\ny\n", "x\ny\n");
        assert!(r.hunks.is_empty());
        assert_eq!(r.map_line(1, true), 1);
    }
}
