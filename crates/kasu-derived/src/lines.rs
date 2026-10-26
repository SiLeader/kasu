use gix_imara_diff::{Algorithm, Diff, InternedInput};
use std::ops::Range;

pub fn lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split_inclusive(|b| *b == b'\n').collect()
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineEdit {
    pub before: Range<usize>,
    pub after: Range<usize>,
}
pub fn line_edits(before: &[u8], after: &[u8]) -> Vec<LineEdit> {
    let a = lines(before);
    let b = lines(after);
    let mut input = InternedInput::default();
    input.update_before(a.iter().copied());
    input.update_after(b.iter().copied());
    let mut diff = Diff::compute(Algorithm::Myers, &input);
    diff.postprocess_lines(&input);
    diff.hunks()
        .map(|h| LineEdit {
            before: h.before.start as usize..h.before.end as usize,
            after: h.after.start as usize..h.after.end as usize,
        })
        .collect()
}
pub fn matching_lines(before: &[u8], after: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut a = 0;
    let mut b = 0;
    for edit in line_edits(before, after) {
        while a < edit.before.start && b < edit.after.start {
            out.push((a, b));
            a += 1;
            b += 1;
        }
        a = edit.before.end;
        b = edit.after.end;
    }
    let a_len = lines(before).len();
    let b_len = lines(after).len();
    while a < a_len && b < b_len {
        out.push((a, b));
        a += 1;
        b += 1;
    }
    out
}
