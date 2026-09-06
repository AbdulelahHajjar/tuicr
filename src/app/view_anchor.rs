use std::path::PathBuf;

use super::*;
use crate::forge::remote_comments::{RemoteCommentSide, RemoteReviewThread};

/// Where the cursor sits, in terms that survive a rebuild of the rendered
/// rows. Rows get inserted or removed above the cursor when threads land,
/// when a thread is hidden or deleted, or when a reload replaces the diff;
/// restoring an anchor puts the view back on the same thing, at the same
/// screen row, instead of leaving a row index that now points elsewhere.
#[derive(Debug, Clone)]
pub struct ViewAnchor {
    target: ViewAnchorTarget,
    /// Rows between the top of the viewport and the cursor.
    rows_from_top: usize,
}

#[derive(Debug, Clone)]
enum ViewAnchorTarget {
    /// A row above the first file, kept by index.
    Overview(usize),
    /// A diff line or another file-scoped row.
    Line(PrCursorAnchor),
    /// A thread row: the thread by id and the row offset inside its block,
    /// with the line it hangs off as the fallback when it is hidden or gone.
    Thread {
        id: String,
        offset: usize,
        fallback: PrCursorAnchor,
    },
}

/// The diff line a thread is attached to, as a cursor anchor.
pub(in crate::app) fn thread_line_anchor(thread: &RemoteReviewThread) -> PrCursorAnchor {
    let (new_lineno, old_lineno) = match thread.side {
        RemoteCommentSide::Right => (thread.line, None),
        RemoteCommentSide::Left => (None, thread.line),
    };
    PrCursorAnchor {
        path: PathBuf::from(&thread.path),
        new_lineno,
        old_lineno,
    }
}

impl App {
    pub(in crate::app) fn capture_view_anchor(&self) -> ViewAnchor {
        let cursor = self.diff_state.cursor_line;
        let rows_from_top = cursor.saturating_sub(self.diff_state.scroll_offset);
        let target = match self.line_annotations.get(cursor) {
            Some(AnnotatedLine::RemoteThreadLine { thread_idx }) => {
                self.forge_review_threads.get(*thread_idx).map(|thread| {
                    let offset = (0..cursor)
                        .rev()
                        .take_while(|row| {
                            matches!(
                                self.line_annotations.get(*row),
                                Some(AnnotatedLine::RemoteThreadLine { thread_idx: above })
                                    if above == thread_idx
                            )
                        })
                        .count();
                    ViewAnchorTarget::Thread {
                        id: thread.id.clone(),
                        offset,
                        fallback: thread_line_anchor(thread),
                    }
                })
            }
            _ => self.capture_pr_cursor_anchor().map(ViewAnchorTarget::Line),
        };
        // Anything without a file behind it (the PR description, review
        // summaries, issue comments) keeps its row: those rows sit above the
        // first file and are not moved by thread or diff changes.
        ViewAnchor {
            target: target.unwrap_or(ViewAnchorTarget::Overview(cursor)),
            rows_from_top,
        }
    }

    pub(in crate::app) fn restore_view_anchor(&mut self, anchor: &ViewAnchor) {
        if self.line_annotations.is_empty() {
            return;
        }
        match &anchor.target {
            ViewAnchorTarget::Overview(row) => {
                self.move_cursor_to_annotation((*row).min(self.max_cursor_line()));
            }
            ViewAnchorTarget::Line(line) => self.restore_pr_cursor_to_anchor(line),
            ViewAnchorTarget::Thread {
                id,
                offset,
                fallback,
            } => match self.thread_row(id, *offset) {
                Some(row) => self.move_cursor_to_annotation(row),
                None => self.restore_pr_cursor_to_anchor(fallback),
            },
        }
        // Put the cursor back at the same screen row rather than wherever the
        // visibility rules happened to leave it.
        self.diff_state.scroll_offset = self
            .diff_state
            .cursor_line
            .saturating_sub(anchor.rows_from_top);
        self.ensure_cursor_visible();
    }

    /// The anchor to restore after the next rebuild: the view carried across
    /// a head-follow reload once that reload has moved the head, otherwise
    /// the current position. Take it before touching the thread list or the
    /// rows, since both are what the anchor is resolved against.
    pub(in crate::app) fn view_anchor_for_rebuild(&mut self) -> ViewAnchor {
        match self.pending_view_anchor.take() {
            Some((captured_head, anchor))
                if self.current_pr_head.as_ref() != Some(&captured_head) =>
            {
                anchor
            }
            _ => self.capture_view_anchor(),
        }
    }

    /// Rebuild the rendered rows and land the view back where it was.
    pub(in crate::app) fn rebuild_annotations_keeping_view(&mut self) {
        let anchor = self.view_anchor_for_rebuild();
        self.rebuild_annotations();
        self.restore_view_anchor(&anchor);
    }

    /// Row of the thread with `id`, `offset` rows into its block, clamped to
    /// the block.
    fn thread_row(&self, id: &str, offset: usize) -> Option<usize> {
        let mut rows =
            self.line_annotations.iter().enumerate().filter_map(
                |(row, annotation)| match annotation {
                    AnnotatedLine::RemoteThreadLine { thread_idx }
                        if self
                            .forge_review_threads
                            .get(*thread_idx)
                            .is_some_and(|thread| thread.id == id) =>
                    {
                        Some(row)
                    }
                    _ => None,
                },
            );
        let first = rows.next()?;
        let last = rows.last().unwrap_or(first);
        Some((first + offset).min(last))
    }
}
