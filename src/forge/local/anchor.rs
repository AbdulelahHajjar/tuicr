use std::path::Path;

use crate::forge::remote_comments::RemoteCommentSide;
use crate::model::{DiffFile, LineOrigin};

use super::store::LocalThread;

pub(crate) fn line_text(
    files: &[DiffFile],
    path: &Path,
    side: RemoteCommentSide,
    line_number: u32,
) -> Option<String> {
    matching_lines(files, path, side)
        .find(|(line, _)| *line == line_number)
        .map(|(_, content)| content.to_string())
}

pub(crate) fn reanchor(
    thread: &LocalThread,
    current_head: &str,
    files: &[DiffFile],
) -> (Option<u32>, bool) {
    let side = RemoteCommentSide::parse(&thread.side);
    if thread.original_commit == current_head
        && (thread.line_text.is_empty()
            || line_text(files, Path::new(&thread.path), side, thread.original_line).as_deref()
                == Some(thread.line_text.as_str()))
    {
        return (Some(thread.original_line), false);
    }
    if thread.line_text.is_empty() {
        return (None, true);
    }
    let nearest = matching_lines(files, Path::new(&thread.path), side)
        .filter(|(_, content)| *content == thread.line_text)
        .map(|(line, _)| line)
        .min_by_key(|line| line.abs_diff(thread.original_line));
    (nearest, nearest.is_none())
}

fn matching_lines<'a>(
    files: &'a [DiffFile],
    path: &Path,
    side: RemoteCommentSide,
) -> impl Iterator<Item = (u32, &'a str)> {
    files
        .iter()
        .filter(move |file| file.display_path() == path)
        .flat_map(|file| &file.hunks)
        .flat_map(|hunk| &hunk.lines)
        .filter_map(move |line| {
            let number = match side {
                RemoteCommentSide::Right => match line.origin {
                    LineOrigin::Addition | LineOrigin::Context => line.new_lineno,
                    LineOrigin::Deletion => None,
                },
                RemoteCommentSide::Left => match line.origin {
                    LineOrigin::Deletion | LineOrigin::Context => line.old_lineno,
                    LineOrigin::Addition => None,
                },
            }?;
            Some((number, line.content.as_str()))
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::model::{DiffHunk, DiffLine, FileStatus};

    use super::*;

    fn file(lines: &[(u32, u32, &str)]) -> DiffFile {
        DiffFile {
            old_path: Some(PathBuf::from("src/lib.rs")),
            new_path: Some(PathBuf::from("src/lib.rs")),
            status: FileStatus::Modified,
            hunks: vec![DiffHunk {
                header: "@@".to_string(),
                lines: lines
                    .iter()
                    .map(|(old, new, content)| DiffLine {
                        origin: LineOrigin::Context,
                        content: (*content).to_string(),
                        old_lineno: Some(*old),
                        new_lineno: Some(*new),
                        highlighted_spans: None,
                    })
                    .collect(),
                old_start: 1,
                old_count: lines.len() as u32,
                new_start: 1,
                new_count: lines.len() as u32,
            }],
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash: 0,
        }
    }

    fn thread(original_commit: &str, original_line: u32, text: &str) -> LocalThread {
        LocalThread {
            id: "thread".to_string(),
            path: "src/lib.rs".to_string(),
            side: "RIGHT".to_string(),
            original_line,
            original_commit: original_commit.to_string(),
            base_commit: "base".to_string(),
            line_text: text.to_string(),
            created_at: chrono::Utc::now(),
            is_resolved: false,
            resolved_at: None,
            review_id: Some(1),
            comments: Vec::new(),
        }
    }

    #[test]
    fn should_keep_original_line_at_same_head() {
        let files = vec![file(&[(17, 17, "same")])];
        assert_eq!(
            reanchor(&thread("head", 17, "same"), "head", &files),
            (Some(17), false)
        );
    }

    #[test]
    fn should_pick_nearest_duplicate_when_line_moves() {
        let files = vec![file(&[(3, 3, "same"), (20, 20, "same")])];
        assert_eq!(
            reanchor(&thread("old", 17, "same"), "new", &files),
            (Some(20), false)
        );
    }

    #[test]
    fn should_mark_deleted_line_outdated() {
        let files = vec![file(&[(3, 3, "other")])];
        assert_eq!(
            reanchor(&thread("old", 17, "gone"), "new", &files),
            (None, true)
        );
    }

    #[test]
    fn should_reanchor_left_line_by_content_at_same_head() {
        let files = vec![file(&[(2, 3, "b")])];
        let mut comment = thread("head", 3, "b");
        comment.side = "LEFT".to_string();

        assert_eq!(reanchor(&comment, "head", &files), (Some(2), false));
    }

    #[test]
    fn should_keep_empty_text_fast_path_at_same_head() {
        assert_eq!(
            reanchor(&thread("head", 17, ""), "head", &[]),
            (Some(17), false)
        );
    }
}
