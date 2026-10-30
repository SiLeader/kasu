//! Unified and side-by-side diff rows from `ContentEdit`s, with context
//! lines taken from the new file content.
use super::text::{html, strip_newline};
use kasu_proto::v1 as p;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Context,
    Added,
    Removed,
    /// Unchanged lines that are not shown.
    Gap,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub kind: Kind,
    /// 1-based line numbers.
    pub old: Option<u64>,
    pub new: Option<u64>,
    pub text: Vec<u8>,
}
pub fn rows(edits: &[p::ContentEdit], after: &[&[u8]], context: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut shown = 0usize;
    let push_context = |rows: &mut Vec<Row>, from: usize, to: usize, offset: i64| {
        for (index, line) in after.iter().enumerate().take(to).skip(from) {
            rows.push(Row {
                kind: Kind::Context,
                old: Some((index as i64 - offset + 1) as u64),
                new: Some(index as u64 + 1),
                text: line.to_vec(),
            });
        }
    };
    for (i, edit) in edits.iter().enumerate() {
        let start = edit.after_start as usize;
        let offset = edit.after_start as i64 - edit.before_start as i64;
        if i > 0 {
            let end = start.min(shown + context);
            push_context(&mut rows, shown, end, offset);
            shown = end;
        }
        let lead = start.saturating_sub(context).max(shown);
        if lead > shown {
            rows.push(Row {
                kind: Kind::Gap,
                old: None,
                new: None,
                text: Vec::new(),
            });
        }
        push_context(&mut rows, lead, start, offset);
        for (n, line) in edit.before.iter().enumerate() {
            rows.push(Row {
                kind: Kind::Removed,
                old: Some(edit.before_start + n as u64 + 1),
                new: None,
                text: strip_newline(line).to_vec(),
            });
        }
        for (n, line) in edit.after.iter().enumerate() {
            rows.push(Row {
                kind: Kind::Added,
                old: None,
                new: Some(edit.after_start + n as u64 + 1),
                text: strip_newline(line).to_vec(),
            });
        }
        shown = start + edit.after.len();
    }
    if let Some(last) = edits.last() {
        let offset = shown as i64 - (last.before_start as i64 + last.before.len() as i64);
        let end = after.len().min(shown + context);
        push_context(&mut rows, shown, end, offset);
        if end < after.len() {
            rows.push(Row {
                kind: Kind::Gap,
                old: None,
                new: None,
                text: Vec::new(),
            });
        }
    }
    rows
}
/// A line of a diff a review comment can attach to: (side, line).
pub fn anchor(row: &Row) -> Option<(p::AnchorSide, u64)> {
    match row.kind {
        Kind::Removed => row.old.map(|l| (p::AnchorSide::Base, l)),
        Kind::Added | Kind::Context => row.new.map(|l| (p::AnchorSide::Head, l)),
        Kind::Gap => None,
    }
}
fn number(n: Option<u64>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}
/// The rows as HTML table rows; `after` adds content (comment threads,
/// forms) below a row and receives the row index.
pub fn unified(rows: &[Row], id: &str, after: &dyn Fn(usize, &Row) -> String) -> String {
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        let (class, sign) = match row.kind {
            Kind::Context => ("ctx", ' '),
            Kind::Added => ("add", '+'),
            Kind::Removed => ("del", '-'),
            Kind::Gap => {
                out.push_str("<tr class=\"gap\"><td class=\"ln\"></td><td class=\"ln\"></td><td class=\"code\">…</td></tr>");
                continue;
            }
        };
        let target = match row.kind {
            Kind::Removed => format!("{id}-O{}", number(row.old)),
            _ => format!("{id}-N{}", number(row.new)),
        };
        out.push_str(&format!(
            "<tr class=\"{class}\" id=\"{target}\"><td class=\"ln\">{}</td><td class=\"ln\"><a href=\"#{target}\">{}</a></td><td class=\"code\"><span class=\"sign\">{sign}</span>{}</td></tr>",
            number(row.old),
            number(row.new),
            html(&row.text)
        ));
        let extra = after(i, row);
        if !extra.is_empty() {
            out.push_str(&format!(
                "<tr class=\"note\"><td colspan=\"3\">{extra}</td></tr>"
            ));
        }
    }
    out
}
/// Removed and added runs side by side.
pub fn split(rows: &[Row], id: &str, after: &dyn Fn(usize, &Row) -> String) -> String {
    let mut out = String::new();
    let cell = |row: Option<(usize, &Row)>, old: bool| match row {
        None => "<td class=\"ln\"></td><td class=\"code empty\"></td>".to_owned(),
        Some((_, row)) => {
            let class = match row.kind {
                Kind::Added => "add",
                Kind::Removed => "del",
                _ => "ctx",
            };
            let n = if old { row.old } else { row.new };
            let target = format!("{id}-{}{}", if old { "O" } else { "N" }, number(n));
            format!(
                "<td class=\"ln {class}\" id=\"{target}\"><a href=\"#{target}\">{}</a></td><td class=\"code {class}\">{}</td>",
                number(n),
                html(&row.text)
            )
        }
    };
    let mut i = 0;
    while i < rows.len() {
        match rows[i].kind {
            Kind::Gap => {
                out.push_str("<tr class=\"gap\"><td colspan=\"4\">…</td></tr>");
                i += 1;
            }
            Kind::Context => {
                out.push_str(&format!(
                    "<tr>{}{}</tr>",
                    cell(Some((i, &rows[i])), true),
                    cell(Some((i, &rows[i])), false)
                ));
                let extra = after(i, &rows[i]);
                if !extra.is_empty() {
                    out.push_str(&format!(
                        "<tr class=\"note\"><td colspan=\"4\">{extra}</td></tr>"
                    ));
                }
                i += 1;
            }
            _ => {
                let removed: Vec<usize> = (i..rows.len())
                    .take_while(|j| rows[*j].kind == Kind::Removed)
                    .collect();
                let start = i + removed.len();
                let added: Vec<usize> = (start..rows.len())
                    .take_while(|j| rows[*j].kind == Kind::Added)
                    .collect();
                let mut extras = String::new();
                for k in 0..removed.len().max(added.len()) {
                    let left = removed.get(k).map(|j| (*j, &rows[*j]));
                    let right = added.get(k).map(|j| (*j, &rows[*j]));
                    out.push_str(&format!(
                        "<tr>{}{}</tr>",
                        cell(left, true),
                        cell(right, false)
                    ));
                    for (j, row) in left.into_iter().chain(right) {
                        extras.push_str(&after(j, row));
                    }
                }
                if !extras.is_empty() {
                    out.push_str(&format!(
                        "<tr class=\"note\"><td colspan=\"4\">{extras}</td></tr>"
                    ));
                }
                i = start + added.len();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn edit(
        before_start: u64,
        after_start: u64,
        before: &[&str],
        after: &[&str],
    ) -> p::ContentEdit {
        p::ContentEdit {
            before_start,
            after_start,
            before: before
                .iter()
                .map(|l| format!("{l}\n").into_bytes())
                .collect(),
            after: after
                .iter()
                .map(|l| format!("{l}\n").into_bytes())
                .collect(),
        }
    }
    #[test]
    fn context_and_gaps_surround_edits_with_correct_numbers() {
        let new: Vec<String> = (1..=20).map(|n| format!("l{n}")).collect();
        let lines: Vec<&[u8]> = new.iter().map(|l| l.as_bytes()).collect();
        // Line 10 replaced by two lines; old file had 19 lines.
        let rows = rows(&[edit(9, 9, &["old"], &["l10", "l11"])], &lines, 3);
        let kinds: Vec<Kind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds[0], Kind::Gap);
        assert_eq!(rows[1].new, Some(7));
        assert_eq!(rows[1].old, Some(7));
        assert_eq!(rows[4].kind, Kind::Removed);
        assert_eq!(rows[4].old, Some(10));
        assert_eq!(rows[5].new, Some(10));
        // Context after the edit is shifted by one line.
        assert_eq!(rows[7].new, Some(12));
        assert_eq!(rows[7].old, Some(11));
        assert_eq!(rows.last().unwrap().kind, Kind::Gap);
        let html = unified(&rows, "f0", &|_, _| String::new());
        assert!(html.contains("id=\"f0-N12\""));
        assert!(split(&rows, "f0", &|_, _| String::new()).contains("f0-O10"));
    }
}
