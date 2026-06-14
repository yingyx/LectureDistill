//! Deterministic line diff helper for comparing text files.
//!
//! Computes a unified-diff-style line-by-line comparison using a simple
//! longest-common-subsequence (LCS) algorithm.  No network, no LLM, no
//! external process required -- just deterministic Rust.

use std::cmp::max;

/// A single diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffHunk {
    /// Line number in the base (original) text (1-indexed).
    pub base_start: usize,
    /// Number of lines in the base text that this hunk covers.
    pub base_count: usize,
    /// Line number in the patched (new) text (1-indexed).
    pub patched_start: usize,
    /// Number of lines in the patched text that this hunk covers.
    pub patched_count: usize,
    /// Individual lines with their kind.
    pub lines: Vec<DiffLine>,
}

/// One line inside a diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DiffLine {
    /// Line present in both base and patched (context).
    Context {
        /// 1-indexed line number in the base text.
        base_line: usize,
        /// 1-indexed line number in the patched text.
        patched_line: usize,
        /// The line content.
        content: String,
    },
    /// Line removed from the base text.
    Remove {
        /// 1-indexed line number in the base text.
        base_line: usize,
        /// The line content.
        content: String,
    },
    /// Line added in the patched text.
    Add {
        /// 1-indexed line number in the patched text.
        patched_line: usize,
        /// The line content.
        content: String,
    },
}

/// Compute a line diff between `base` and `patched` text.
///
/// Returns a list of [`DiffHunk`]s.  Adjacent changes within
/// `context_lines` of each other are merged into the same hunk.
pub fn line_diff(base: &str, patched: &str, context_lines: usize) -> Vec<DiffHunk> {
    let base_lines: Vec<&str> = base.lines().collect();
    let patched_lines: Vec<&str> = patched.lines().collect();

    let edits = compute_lcs_edits(&base_lines, &patched_lines);
    edits_to_hunks(&edits, &base_lines, &patched_lines, context_lines)
}

/// A single edit operation from the LCS diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Keep,
    Remove,
    Add,
}

/// Compute the edit script from `a` to `b` using the standard LCS
/// dynamic-programming table, then backtracking.
fn compute_lcs_edits<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Edit> {
    let n = a.len();
    let m = b.len();

    // dp[i][j] = length of LCS of a[..i] and b[..j]
    // Use two rows to save memory.
    let mut prev = vec![0usize; m + 1];
    let mut curr = vec![0usize; m + 1];

    for i in 1..=n {
        for j in 1..=m {
            if a[i - 1] == b[j - 1] {
                curr[j] = prev[j - 1] + 1;
            } else {
                curr[j] = max(prev[j], curr[j - 1]);
            }
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    // Backtrack using the full table. We need to reconstruct or keep the full
    // table.  Let's keep a full table for simplicity -- the typical use is on
    // notes files that are at most a few thousand lines, so O(n*m) memory is
    // fine.
    //
    // Rebuild the full table:
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = max(dp[i - 1][j], dp[i][j - 1]);
            }
        }
    }

    // Backtrack.
    let mut edits: Vec<Edit> = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            edits.push(Edit::Keep);
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            edits.push(Edit::Add);
            j -= 1;
        } else {
            edits.push(Edit::Remove);
            i -= 1;
        }
    }
    edits.reverse();
    edits
}

/// Convert a flat edit script into unified-diff-style hunks.
fn edits_to_hunks(
    edits: &[Edit],
    base_lines: &[&str],
    patched_lines: &[&str],
    context_lines: usize,
) -> Vec<DiffHunk> {
    if edits.is_empty() {
        return Vec::new();
    }

    // Step 1: Build a list of DiffLine items from the edit script.
    let mut lines: Vec<DiffLine> = Vec::new();
    let mut base_idx: usize = 1; // 1-indexed
    let mut patched_idx: usize = 1;

    for edit in edits {
        match edit {
            Edit::Keep => {
                lines.push(DiffLine::Context {
                    base_line: base_idx,
                    patched_line: patched_idx,
                    content: base_lines[base_idx - 1].to_string(),
                });
                base_idx += 1;
                patched_idx += 1;
            }
            Edit::Remove => {
                lines.push(DiffLine::Remove {
                    base_line: base_idx,
                    content: base_lines[base_idx - 1].to_string(),
                });
                base_idx += 1;
            }
            Edit::Add => {
                lines.push(DiffLine::Add {
                    patched_line: patched_idx,
                    content: patched_lines[patched_idx - 1].to_string(),
                });
                patched_idx += 1;
            }
        }
    }

    // Step 2: Group into hunks around change regions.
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let n = lines.len();

    // Find ranges that contain at least one change (Remove or Add).
    let mut change_ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if !matches!(lines[i], DiffLine::Context { .. }) {
            let start = i;
            while i < n && !matches!(lines[i], DiffLine::Context { .. }) {
                i += 1;
            }
            change_ranges.push((start, i));
        } else {
            i += 1;
        }
    }

    if change_ranges.is_empty() {
        return Vec::new();
    }

    // Merge overlapping ranges after adding context.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in &change_ranges {
        let ctx_start = start.saturating_sub(context_lines);
        let ctx_end = (end + context_lines).min(n);

        if let Some(last) = merged.last_mut() {
            if ctx_start <= last.1 {
                // Overlap -- extend.
                last.1 = last.1.max(ctx_end);
                continue;
            }
        }
        merged.push((ctx_start, ctx_end));
    }

    for (start, end) in merged {
        let hunk_lines: Vec<DiffLine> = lines[start..end].to_vec();

        let base_start = hunk_lines
            .iter()
            .filter_map(|l| match l {
                DiffLine::Context { base_line, .. } => Some(*base_line),
                DiffLine::Remove { base_line, .. } => Some(*base_line),
                _ => None,
            })
            .min()
            .unwrap_or(1);

        let base_count = hunk_lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Context { .. } | DiffLine::Remove { .. }))
            .count();

        let patched_start = hunk_lines
            .iter()
            .filter_map(|l| match l {
                DiffLine::Context { patched_line, .. } => Some(*patched_line),
                DiffLine::Add { patched_line, .. } => Some(*patched_line),
                _ => None,
            })
            .min()
            .unwrap_or(1);

        let patched_count = hunk_lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Context { .. } | DiffLine::Add { .. }))
            .count();

        hunks.push(DiffHunk {
            base_start,
            base_count,
            patched_start,
            patched_count,
            lines: hunk_lines,
        });
    }

    hunks
}

/// Convenience: compute a diff and serialize it to a JSON string.
pub fn diff_json(base: &str, patched: &str, context_lines: usize) -> serde_json::Value {
    let hunks = line_diff(base, patched, context_lines);
    serde_json::to_value(&hunks).unwrap_or_default()
}

/// Convenience: compute a unified diff string (like `diff -u`).
pub fn unified_diff(base: &str, patched: &str, context_lines: usize) -> String {
    let hunks = line_diff(base, patched, context_lines);
    let mut out = String::new();

    for hunk in &hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.base_start, hunk.base_count, hunk.patched_start, hunk.patched_count
        ));
        for line in &hunk.lines {
            match line {
                DiffLine::Context { content, .. } => {
                    out.push_str(&format!(" {}\n", content));
                }
                DiffLine::Remove { content, .. } => {
                    out.push_str(&format!("-{}\n", content));
                }
                DiffLine::Add { content, .. } => {
                    out.push_str(&format!("+{}\n", content));
                }
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_strings() {
        let hunks = line_diff("", "", 3);
        assert!(hunks.is_empty());
    }

    #[test]
    fn test_identical_text() {
        let text = "line one\nline two\nline three\n";
        let hunks = line_diff(text, text, 3);
        assert!(hunks.is_empty());
    }

    #[test]
    fn test_single_line_change() {
        let base = "line one\nline two\nline three\n";
        let patched = "line one\nline two modified\nline three\n";

        let hunks = line_diff(base, patched, 3);
        assert_eq!(hunks.len(), 1);

        let hunk = &hunks[0];
        // Should have context around the change.
        let has_remove = hunk
            .lines
            .iter()
            .any(|l| matches!(l, DiffLine::Remove { .. }));
        let has_add = hunk.lines.iter().any(|l| matches!(l, DiffLine::Add { .. }));
        assert!(has_remove, "expected a Remove line");
        assert!(has_add, "expected an Add line");
    }

    #[test]
    fn test_add_lines() {
        let base = "line one\nline two\n";
        let patched = "line one\nline one point five\nline two\n";

        let hunks = line_diff(base, patched, 0);
        assert_eq!(hunks.len(), 1);

        let add_count = hunks[0]
            .lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Add { .. }))
            .count();
        assert_eq!(add_count, 1);
    }

    #[test]
    fn test_remove_lines() {
        let base = "line one\nremove me\nline two\n";
        let patched = "line one\nline two\n";

        let hunks = line_diff(base, patched, 0);
        assert_eq!(hunks.len(), 1);

        let remove_count = hunks[0]
            .lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Remove { .. }))
            .count();
        assert_eq!(remove_count, 1);
    }

    #[test]
    fn test_multiple_hunks() {
        let base = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let patched = "a\nB\nc\nd\nE\nf\nG\nh\n";

        let hunks = line_diff(base, patched, 0);
        // Three separate changes, each in its own hunk with context=0.
        assert_eq!(hunks.len(), 3);
    }

    #[test]
    fn test_context_merges_adjacent_changes() {
        let base = "line0\nline1\nline2\nline3\nline4\nline5\nline6\n";
        let patched = "line0\nCHANGE1\nline2\nCHANGE2\nline4\nline5\nline6\n";

        // With context 0, these are two separate hunks.
        let hunks_no_ctx = line_diff(base, patched, 0);
        assert_eq!(hunks_no_ctx.len(), 2);

        // With context 1, they merge because line2 (context) sits between them.
        let hunks_ctx = line_diff(base, patched, 1);
        assert_eq!(hunks_ctx.len(), 1);
    }

    #[test]
    fn test_diff_json_returns_array() {
        let base = "hello\nworld\n";
        let patched = "hello\nWORLD\n";
        let json = diff_json(base, patched, 3);
        assert!(json.is_array());
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn test_unified_diff_output() {
        let base = "line one\nline two\nline three\n";
        let patched = "line one\nline TWO\nline three\n";
        let out = unified_diff(base, patched, 3);
        assert!(out.contains("@@"));
        assert!(out.contains("-line two"));
        assert!(out.contains("+line TWO"));
    }

    #[test]
    fn test_line_numbers_are_1_indexed() {
        let base = "first\nsecond\nthird\n";
        let patched = "first\nSECOND\nthird\n";
        let hunks = line_diff(base, patched, 3);

        let hunk = &hunks[0];
        assert!(hunk.base_start >= 1, "base_start must be 1-indexed");
        assert!(hunk.patched_start >= 1, "patched_start must be 1-indexed");

        for line in &hunk.lines {
            match line {
                DiffLine::Context {
                    base_line,
                    patched_line,
                    ..
                } => {
                    assert!(*base_line >= 1, "base_line in context must be 1-indexed");
                    assert!(
                        *patched_line >= 1,
                        "patched_line in context must be 1-indexed"
                    );
                }
                DiffLine::Remove { base_line, .. } => {
                    assert!(*base_line >= 1, "base_line in remove must be 1-indexed");
                }
                DiffLine::Add { patched_line, .. } => {
                    assert!(*patched_line >= 1, "patched_line in add must be 1-indexed");
                }
            }
        }
    }

    #[test]
    fn test_large_text() {
        let mut base = String::new();
        let mut patched = String::new();
        for i in 0..200 {
            base.push_str(&format!("line {}\n", i));
            if i == 100 {
                patched.push_str("INSERTED LINE\n");
            }
            patched.push_str(&format!("line {}\n", i));
        }

        let hunks = line_diff(&base, &patched, 3);
        assert_eq!(hunks.len(), 1);
        let add_count = hunks[0]
            .lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Add { .. }))
            .count();
        assert_eq!(add_count, 1);
    }
}
