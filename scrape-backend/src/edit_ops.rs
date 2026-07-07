//! `string_replace` and similarity-preview helpers, ported verbatim from the
//! slop coding agent (`slop-agent/src/tools/edit_ops.rs`), which itself ported
//! them from `goose/crates/goose/src/agents/platform_extensions/developer/edit.rs`.
//!
//! Pure string logic — no filesystem dependency — so it backs the `edit_file`
//! tool over the SQLite `origin_files` store unchanged.

const NO_MATCH_PREVIEW_LINES: usize = 20;

/// Replace `before` with `after` in `content`. Returns the new content if and
/// only if `before` occurs exactly once. Otherwise returns a human-readable
/// error explaining the ambiguity, with line context for multi-match and a
/// file preview for no-match.
pub fn string_replace(content: &str, before: &str, after: &str) -> Result<String, String> {
    let matches: Vec<_> = content.match_indices(before).collect();
    match matches.len() {
        0 => {
            let mut msg = "No match found for the specified text.".to_string();
            if let Some(hint) = find_similar_context(content, before) {
                msg.push_str(&format!("\n\nDid you mean:\n```\n{hint}\n```"));
            }
            let preview = build_file_preview(content, NO_MATCH_PREVIEW_LINES);
            msg.push_str(&format!("\n\nFile preview:\n```\n{preview}\n```"));
            Err(msg)
        }
        1 => Ok(content.replacen(before, after, 1)),
        n => {
            let mut msg = format!(
                "Found {n} matches. Please provide more context to identify a unique match:\n",
            );
            for (i, (pos, _)) in matches.iter().enumerate().take(2) {
                let line_num = count_lines_before(content, *pos);
                let context = get_line_context(content, line_num, 1);
                msg.push_str(&format!(
                    "\nMatch {} (line {}):\n```\n{}\n```",
                    i + 1,
                    line_num,
                    context
                ));
            }
            if n > 2 {
                msg.push_str(&format!("\n\n...and {} more", n - 2));
            }
            Err(msg)
        }
    }
}

fn count_lines_before(content: &str, byte_pos: usize) -> usize {
    content
        .char_indices()
        .take_while(|(i, _)| *i < byte_pos)
        .filter(|(_, c)| *c == '\n')
        .count()
        + 1
}

fn get_line_context(content: &str, target_line: usize, context: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = target_line.saturating_sub(context + 1);
    let end = (target_line + context).min(lines.len());
    lines[start..end].join("\n")
}

fn find_similar_context(content: &str, search: &str) -> Option<String> {
    let first_line = search.lines().next()?.trim();
    if first_line.is_empty() {
        return None;
    }
    for (i, line) in content.lines().enumerate() {
        if line.contains(first_line) || first_line.contains(line.trim()) {
            return Some(get_line_context(content, i + 1, 2));
        }
    }
    None
}

fn build_file_preview(content: &str, max_lines: usize) -> String {
    if content.is_empty() {
        return "(file is empty)".to_string();
    }
    let lines: Vec<&str> = content.lines().collect();
    let preview_end = lines.len().min(max_lines);
    let mut preview = lines[..preview_end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>4}: {}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > preview_end {
        preview.push_str(&format!("\n... ({} more lines)", lines.len() - preview_end));
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_match_replaces() {
        let out = string_replace("fn foo() {}", "foo", "bar").unwrap();
        assert_eq!(out, "fn bar() {}");
    }

    #[test]
    fn no_match_returns_preview_and_hint() {
        let err = string_replace("hello world", "goodbye", "x").unwrap_err();
        assert!(err.contains("No match found"));
        assert!(err.contains("File preview:"));
        assert!(err.contains("hello world"));
    }

    #[test]
    fn multiple_matches_lists_positions() {
        let err = string_replace("foo\nbar\nfoo\nbaz", "foo", "qux").unwrap_err();
        assert!(err.contains("Found 2 matches"));
        assert!(err.contains("Match 1 (line 1)"));
        assert!(err.contains("Match 2 (line 3)"));
    }

    #[test]
    fn similarity_hint_surfaces_when_close_match_exists() {
        let content = "    println!(\"hello\");";
        let err = string_replace(content, "println!(\"hello\");\nlet y = 2;", "x").unwrap_err();
        assert!(err.contains("Did you mean"), "missing Did you mean in: {err}");
    }

    #[test]
    fn delete_via_empty_after() {
        let out = string_replace("keep\ndelete me\nkeep", "\ndelete me", "").unwrap();
        assert_eq!(out, "keep\nkeep");
    }

    #[test]
    fn many_matches_truncates_to_two_with_count() {
        let err = string_replace("x\nx\nx\nx\nx", "x", "y").unwrap_err();
        assert!(err.contains("Found 5 matches"));
        assert!(err.contains("...and 3 more"));
    }
}
