/// Shared diff computation for CLI and TUI.
/// Produces structured diff lines with line numbers and change kinds.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Added,
    Removed,
    Omitted,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    pub marker: char,
    pub text: String,
}

pub struct ComputedDiff {
    pub lines: Vec<DiffLine>,
    pub changed_blocks: usize,
}

pub fn compute_diff(old_text: &str, new_text: &str) -> ComputedDiff {
    let old_lines = split_lines(old_text);
    let new_lines = split_lines(new_text);
    let ops = lcs_diff(&old_lines, &new_lines);
    let (rendered, changed_blocks) = compress(ops, 2, 64);
    ComputedDiff {
        lines: rendered,
        changed_blocks,
    }
}

pub fn compute_write_diff(new_text: &str) -> ComputedDiff {
    let new_lines = split_lines(new_text);
    let lines: Vec<DiffLine> = new_lines
        .iter()
        .enumerate()
        .take(30)
        .map(|(idx, line)| DiffLine {
            kind: DiffKind::Added,
            old_lineno: None,
            new_lineno: Some(idx + 1),
            marker: '+',
            text: line.clone(),
        })
        .collect();
    let mut result = lines;
    if new_lines.len() > 30 {
        result.push(DiffLine {
            kind: DiffKind::Omitted,
            old_lineno: None,
            new_lineno: None,
            marker: '…',
            text: format!(
                "{} more lines (total {})",
                new_lines.len() - 30,
                new_lines.len()
            ),
        });
    }
    ComputedDiff {
        lines: result,
        changed_blocks: 1,
    }
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.lines().map(|l| l.to_string()).collect()
    }
}

fn lcs_diff(old_lines: &[String], new_lines: &[String]) -> Vec<DiffLine> {
    let n = old_lines.len();
    let m = new_lines.len();

    if n.saturating_mul(m) > 40_000 {
        let mut out = Vec::new();
        for (idx, line) in old_lines.iter().enumerate() {
            out.push(DiffLine {
                kind: DiffKind::Removed,
                old_lineno: Some(idx + 1),
                new_lineno: None,
                marker: '-',
                text: line.clone(),
            });
        }
        for (idx, line) in new_lines.iter().enumerate() {
            out.push(DiffLine {
                kind: DiffKind::Added,
                old_lineno: None,
                new_lineno: Some(idx + 1),
                marker: '+',
                text: line.clone(),
            });
        }
        return out;
    }

    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_lines[i] == new_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut i = 0usize;
    let mut j = 0usize;
    let mut out = Vec::new();
    while i < n || j < m {
        if i < n && j < m && old_lines[i] == new_lines[j] {
            out.push(DiffLine {
                kind: DiffKind::Context,
                old_lineno: Some(i + 1),
                new_lineno: Some(j + 1),
                marker: ' ',
                text: old_lines[i].clone(),
            });
            i += 1;
            j += 1;
        } else if j < m && (i == n || dp[i][j + 1] >= dp[i + 1][j]) {
            out.push(DiffLine {
                kind: DiffKind::Added,
                old_lineno: None,
                new_lineno: Some(j + 1),
                marker: '+',
                text: new_lines[j].clone(),
            });
            j += 1;
        } else if i < n {
            out.push(DiffLine {
                kind: DiffKind::Removed,
                old_lineno: Some(i + 1),
                new_lineno: None,
                marker: '-',
                text: old_lines[i].clone(),
            });
            i += 1;
        }
    }
    out
}

fn compress(ops: Vec<DiffLine>, context: usize, max_lines: usize) -> (Vec<DiffLine>, usize) {
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| (line.kind != DiffKind::Context).then_some(idx))
        .collect();

    if changed.is_empty() {
        return (
            vec![DiffLine {
                kind: DiffKind::Context,
                old_lineno: None,
                new_lineno: None,
                marker: ' ',
                text: "(no textual changes)".to_string(),
            }],
            0,
        );
    }

    let mut ranges = Vec::new();
    let mut start = changed[0].saturating_sub(context);
    let mut end = (changed[0] + context + 1).min(ops.len());
    for &idx in changed.iter().skip(1) {
        let next_start = idx.saturating_sub(context);
        let next_end = (idx + context + 1).min(ops.len());
        if next_start <= end {
            end = end.max(next_end);
        } else {
            ranges.push((start, end));
            start = next_start;
            end = next_end;
        }
    }
    ranges.push((start, end));

    let changed_blocks = ranges.len();
    let mut out = Vec::new();
    let mut previous_end = 0usize;
    for (range_index, (start, end)) in ranges.into_iter().enumerate() {
        if range_index > 0 {
            let skipped = start.saturating_sub(previous_end);
            out.push(DiffLine {
                kind: DiffKind::Omitted,
                old_lineno: None,
                new_lineno: None,
                marker: '…',
                text: format!("{} unchanged lines hidden", skipped.max(1)),
            });
        }
        out.extend(ops[start..end].iter().cloned());
        previous_end = end;
        if out.len() >= max_lines {
            out.truncate(max_lines.saturating_sub(1));
            out.push(DiffLine {
                kind: DiffKind::Omitted,
                old_lineno: None,
                new_lineno: None,
                marker: '…',
                text: "diff truncated".to_string(),
            });
            break;
        }
    }
    (out, changed_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_edit_produces_diff() {
        let old = "fn main() {\n    println!(\"hello\");\n}\n";
        let new = "fn main() {\n    println!(\"world\");\n}\n";
        let diff = compute_diff(old, new);
        assert!(diff.lines.iter().any(|l| l.kind == DiffKind::Removed));
        assert!(diff.lines.iter().any(|l| l.kind == DiffKind::Added));
        assert!(diff.changed_blocks >= 1);
    }

    #[test]
    fn write_diff_caps_at_30_lines() {
        let text = (1..=50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = compute_write_diff(&text);
        let added_count = diff
            .lines
            .iter()
            .filter(|l| l.kind == DiffKind::Added)
            .count();
        assert_eq!(added_count, 30);
        assert!(diff.lines.iter().any(|l| l.kind == DiffKind::Omitted));
    }
}
