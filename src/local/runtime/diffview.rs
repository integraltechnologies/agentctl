//! Verifier diff material that scales with the size of a change rather than
//! with the size of the files it touches. A view is derived deterministically
//! from a hash-bound [`CapturedDiff`]: the capture itself is the binding (both
//! snapshot artifacts, every changed path's before/after content hash and mode,
//! ignored-file metadata, scope violations), and each change adds exact line
//! hunks with a bounded amount of unchanged surrounding context. Nothing is
//! summarized or truncated: a view either shows every changed line or, past
//! [`MAX_DIFF_VIEW_BYTES`], fails closed.
use super::*;
use source::FileChange;

pub const DIFF_VIEW_VERSION: &str = "agentctl-diff-view-1";
/// Unchanged lines shown before and after each change.
pub const CONTEXT_LINES: usize = 3;
/// Ceiling on the expanded verifier diff (packet and integration alike).
pub const MAX_DIFF_VIEW_BYTES: usize = 128 * 1024;
/// Edit-distance bound of the line diff. Past it the changed middle region is
/// shown as one exact delete-then-insert hunk: still exact, only larger.
const MAX_EDIT_DISTANCE: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChangeStatus {
    Added,
    Deleted,
    Modified,
    /// Identical content; only the file mode changed.
    ModeChanged,
    /// An individually ignored file observed by metadata alone.
    IgnoredMetadata,
}

/// Line count and final-newline presence of one side of a text change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextShape {
    pub lines: usize,
    pub final_newline: bool,
}

/// One unified-diff hunk. Starts are one-based; a side with no lines names the
/// line it follows (0 at the start of the file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hunk {
    pub before_start: usize,
    pub before_lines: usize,
    pub after_start: usize,
    pub after_lines: usize,
    /// Each line prefixed by ' ' (context), '-' (removed) or '+' (added),
    /// without its line terminator.
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeView {
    pub path: String,
    pub status: ChangeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<TextShape>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<TextShape>,
    pub hunks: Vec<Hunk>,
    /// Metadata of an individually ignored file on either side; its content is
    /// never captured, so the verifier sees only that it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_before: Option<source::IgnoredFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_after: Option<source::IgnoredFile>,
}

/// The bounded verifier diff of a captured transition and, for the manifest,
/// the changed paths it carries (bound by their after-state, or before-state
/// for deletions).
pub(super) fn view(
    diff: &CapturedDiff,
    reference: &ArtifactRef,
    artifacts: &Artifacts,
) -> Result<(serde_json::Value, Vec<manifest::SuppliedSource>)> {
    let paths = diff
        .changes
        .iter()
        .map(|c| manifest::SuppliedSource {
            path: c.path.clone(),
            kind: manifest::SuppliedKind::Diff,
            start_line: None,
            end_line: None,
            content_hash: c
                .after
                .as_ref()
                .or(c.before.as_ref())
                .map(|f| f.content.hash.clone()),
            truncated: false,
        })
        .collect();
    let mut changes = vec![];
    for change in &diff.changes {
        changes.push(change_view(change, artifacts)?);
    }
    let value = serde_json::json!({
        "version": DIFF_VIEW_VERSION,
        "binding": diff,
        "artifact": reference,
        "context_lines": CONTEXT_LINES,
        "changes": changes,
    });
    require(
        serde_json::to_vec(&value)?.len() <= MAX_DIFF_VIEW_BYTES,
        "exact diff exceeds 128 KiB verifier context; split/replan task",
    )?;
    Ok((value, paths))
}

fn change_view(change: &FileChange, artifacts: &Artifacts) -> Result<ChangeView> {
    let text = |file: &Option<source::FileState>| -> Result<Option<String>> {
        file.as_ref()
            .map(|f| {
                String::from_utf8(artifacts.get(&f.content)?).map_err(|_| {
                    Error::Invalid(
                        "binary diff cannot be verified by this bounded text runtime".into(),
                    )
                })
            })
            .transpose()
    };
    let (before, after) = (text(&change.before)?, text(&change.after)?);
    let status = match (&change.before, &change.after) {
        (None, None) => ChangeStatus::IgnoredMetadata,
        (None, Some(_)) => ChangeStatus::Added,
        (Some(_), None) => ChangeStatus::Deleted,
        (Some(b), Some(a)) if b.content == a.content => ChangeStatus::ModeChanged,
        (Some(_), Some(_)) => ChangeStatus::Modified,
    };
    let (before_lines, before_newline) = split(before.as_deref().unwrap_or(""));
    let (after_lines, after_newline) = split(after.as_deref().unwrap_or(""));
    Ok(ChangeView {
        path: change.path.clone(),
        status,
        before: before.as_ref().map(|_| TextShape {
            lines: before_lines.len(),
            final_newline: before_newline,
        }),
        after: after.as_ref().map(|_| TextShape {
            lines: after_lines.len(),
            final_newline: after_newline,
        }),
        hunks: hunks(&before_lines, &after_lines, CONTEXT_LINES),
        ignored_before: change.before_ignored.clone(),
        ignored_after: change.after_ignored.clone(),
    })
}

/// Lines without terminators, and whether the text ends with a newline.
fn split(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (vec![], false);
    }
    let final_newline = text.ends_with('\n');
    let body = if final_newline {
        &text[..text.len() - 1]
    } else {
        text
    };
    (body.split('\n').collect(), final_newline)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// Shortest edit script between `a` and `b` (Myers' O((N+M)D) algorithm), or
/// `None` when it needs more than `max` edits. Only the band of diagonals each
/// step can reach is retained, so memory is O(D²) for the traceback.
fn myers(a: &[&str], b: &[&str], max: usize) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let limit = (max as isize).min(n + m);
    let offset = limit + 1;
    let mut v = vec![0isize; (2 * limit + 3) as usize];
    let at = |k: isize| (k + offset) as usize;
    let mut trace: Vec<Vec<isize>> = vec![];
    for d in 0..=limit {
        // Diagonals -d-1..=d+1 of the state before this step.
        trace.push(v[at(-d - 1)..=at(d + 1)].to_vec());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]) {
                v[at(k + 1)]
            } else {
                v[at(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Op> {
    let (mut x, mut y) = (n, m);
    let mut ops = vec![];
    for d in (0..trace.len()).rev() {
        let d_i = d as isize;
        // trace[d] holds diagonals -d-1..=d+1.
        let v = |k: isize| trace[d][(k + d_i + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d_i || (k != d_i && v(k - 1) < v(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            ops.push(if x == prev_x { Op::Insert } else { Op::Delete });
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    ops
}

/// The full edit script: common prefix and suffix are matched directly, and
/// only the differing middle goes through the bounded shortest-edit search.
fn script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let middle = myers(a_mid, b_mid, MAX_EDIT_DISTANCE).unwrap_or_else(|| {
        std::iter::repeat_n(Op::Delete, a_mid.len())
            .chain(std::iter::repeat_n(Op::Insert, b_mid.len()))
            .collect()
    });
    let mut ops = vec![Op::Equal; prefix];
    ops.extend(middle);
    ops.extend(std::iter::repeat_n(Op::Equal, suffix));
    ops
}

/// Unified hunks with `context` unchanged lines around each change; changes
/// whose contexts touch are merged into one hunk.
fn hunks(a: &[&str], b: &[&str], context: usize) -> Vec<Hunk> {
    let ops = script(a, b);
    // (op, before lines consumed before it, after lines consumed before it)
    let mut positioned = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0usize, 0usize);
    for op in ops {
        positioned.push((op, i, j));
        match op {
            Op::Equal => {
                i += 1;
                j += 1;
            }
            Op::Delete => i += 1,
            Op::Insert => j += 1,
        }
    }
    let changed: Vec<usize> = positioned
        .iter()
        .enumerate()
        .filter(|(_, (op, ..))| *op != Op::Equal)
        .map(|(p, _)| p)
        .collect();
    let Some(&first) = changed.first() else {
        return vec![];
    };
    let mut groups = vec![];
    let (mut start, mut end) = (first, first);
    for &p in &changed[1..] {
        if p - end - 1 > 2 * context {
            groups.push((start, end));
            start = p;
        }
        end = p;
    }
    groups.push((start, end));
    groups
        .into_iter()
        .map(|(start, end)| {
            let lo = start.saturating_sub(context);
            let hi = (end + context).min(positioned.len() - 1);
            let slice = &positioned[lo..=hi];
            let before_lines = slice.iter().filter(|(op, ..)| *op != Op::Insert).count();
            let after_lines = slice.iter().filter(|(op, ..)| *op != Op::Delete).count();
            let (_, i0, j0) = slice[0];
            Hunk {
                before_start: if before_lines > 0 { i0 + 1 } else { i0 },
                before_lines,
                after_start: if after_lines > 0 { j0 + 1 } else { j0 },
                after_lines,
                lines: slice
                    .iter()
                    .map(|&(op, i, j)| match op {
                        Op::Equal => format!(" {}", a[i]),
                        Op::Delete => format!("-{}", a[i]),
                        Op::Insert => format!("+{}", b[j]),
                    })
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies hunks to `before`: the view is exact iff this reproduces `after`.
    fn apply(before: &[&str], hunks: &[Hunk]) -> Vec<String> {
        let mut out = vec![];
        let mut next = 0usize; // zero-based index of the next unconsumed before line
        for hunk in hunks {
            let first = if hunk.before_lines > 0 {
                hunk.before_start - 1
            } else {
                hunk.before_start
            };
            while next < first {
                out.push(before[next].to_string());
                next += 1;
            }
            for line in &hunk.lines {
                let (tag, text) = line.split_at(1);
                match tag {
                    " " => {
                        assert_eq!(before[next], text, "context line mismatch");
                        out.push(text.to_string());
                        next += 1;
                    }
                    "-" => {
                        assert_eq!(before[next], text, "removed line mismatch");
                        next += 1;
                    }
                    "+" => out.push(text.to_string()),
                    _ => panic!("bad hunk line {line:?}"),
                }
            }
        }
        out.extend(before[next..].iter().map(|s| s.to_string()));
        out
    }

    fn check(before: &str, after: &str) -> Vec<Hunk> {
        let (a, _) = split(before);
        let (b, _) = split(after);
        let hunks = hunks(&a, &b, CONTEXT_LINES);
        assert_eq!(apply(&a, &hunks), b, "{before:?} -> {after:?}");
        for hunk in &hunks {
            let changed = hunk.lines.iter().filter(|l| !l.starts_with(' ')).count();
            assert!(changed > 0, "hunk without a change");
            assert_eq!(
                hunk.before_lines,
                hunk.lines.iter().filter(|l| !l.starts_with('+')).count()
            );
            assert_eq!(
                hunk.after_lines,
                hunk.lines.iter().filter(|l| !l.starts_with('-')).count()
            );
        }
        hunks
    }

    #[test]
    fn hunks_reproduce_the_after_text_exactly() {
        for (before, after) in [
            ("", ""),
            ("", "a\n"),
            ("a\n", ""),
            ("a\nb\nc\n", "a\nb\nc\n"),
            ("a\nb\nc\n", "a\nx\nc\n"),
            ("a\nb\nc\n", "x\na\nb\nc\n"),
            ("a\nb\nc\n", "a\nb\nc\nx\n"),
            ("a\nb\nc", "a\nb\nc\n"),
            ("a\r\nb\r\n", "a\r\nc\r\n"),
            (
                "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n",
                "1\nX\n3\n4\n5\n6\n7\n8\nY\n10\n",
            ),
            ("a\nb\na\nb\na\n", "b\na\nb\na\nb\n"),
        ] {
            check(before, after);
        }
    }

    #[test]
    fn distant_changes_get_separate_hunks_and_near_ones_merge() {
        let before: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let far = before
            .replace("line 5\n", "LINE 5\n")
            .replace("line 30\n", "LINE 30\n");
        assert_eq!(check(&before, &far).len(), 2);
        let near = before
            .replace("line 5\n", "LINE 5\n")
            .replace("line 10\n", "LINE 10\n");
        assert_eq!(check(&before, &near).len(), 1);
    }

    /// Deterministic pseudo-random edits, including ones past the edit-distance
    /// bound, always yield exact hunks.
    #[test]
    fn randomized_edits_are_exact_including_past_the_edit_distance_bound() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..60 {
            let len = 1 + (next() % 300) as usize;
            let before: Vec<String> = (0..len).map(|_| format!("v{}", next() % 12)).collect();
            let mut after = before.clone();
            let edits = if round % 10 == 0 {
                3000
            } else {
                1 + (next() % 20) as usize
            };
            for _ in 0..edits {
                let at = (next() as usize) % (after.len() + 1);
                match next() % 3 {
                    0 if at < after.len() => {
                        after.remove(at);
                    }
                    1 if at < after.len() => after[at] = format!("w{}", next() % 12),
                    _ => after.insert(at, format!("n{}", next() % 12)),
                }
            }
            let (a, b) = (before.join("\n") + "\n", after.join("\n") + "\n");
            check(&a, &b);
        }
    }

    /// A one-line edit in a large file yields one small hunk: context scales
    /// with the change, not the file.
    #[test]
    fn a_one_line_edit_in_a_large_file_is_one_small_hunk() {
        let before: String = (0..4000)
            .map(|i| format!("pub const C{i}: u32 = {i};\n"))
            .collect();
        assert!(before.len() > 100_000);
        let after = before.replace("C2000: u32 = 2000;", "C2000: u32 = 2001;");
        let hunks = check(&before, &after);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].lines.len(), 2 * CONTEXT_LINES + 2);
        assert_eq!(hunks[0].before_start, 2001 - CONTEXT_LINES);
    }
}
