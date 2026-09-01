use super::*;

impl App {
    pub fn resolve_review_thread_at_cursor(&mut self, resolved: bool) -> Result<()> {
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return Err(TuicrError::UnsupportedOperation(
                "Not in PR mode".to_string(),
            ));
        };
        let thread_idx = if self.focused_panel == FocusedPanel::Comments {
            self.build_comment_navigator_items()
                .get(self.comment_navigator_state.selected())
                .and_then(|item| match item.key {
                    CommentNavigatorKey::Remote { thread_idx } => Some(thread_idx),
                    _ => None,
                })
        } else {
            None
        }
        .or_else(
            || match self.line_annotations.get(self.diff_state.cursor_line) {
                Some(AnnotatedLine::RemoteThreadLine { thread_idx }) => Some(*thread_idx),
                _ => None,
            },
        )
        .or_else(|| {
            let (line, side) = self.get_line_at_cursor()?;
            let path = self
                .diff_files
                .get(self.diff_state.current_file_idx)?
                .display_path();
            self.forge_review_threads.iter().position(|thread| {
                thread.path == path.to_string_lossy()
                    && thread.line == Some(line)
                    && matches!(
                        (thread.side, side),
                        (
                            crate::forge::remote_comments::RemoteCommentSide::Right,
                            LineSide::New
                        ) | (
                            crate::forge::remote_comments::RemoteCommentSide::Left,
                            LineSide::Old
                        )
                    )
            })
        })
        .ok_or_else(|| TuicrError::Forge("No review thread at cursor".to_string()))?;

        let thread_id = self
            .forge_review_threads
            .get(thread_idx)
            .map(|thread| thread.id.clone())
            .ok_or_else(|| TuicrError::Forge("No review thread at cursor".to_string()))?;
        let details = match self.pr_info.as_ref() {
            Some(info) => info.details.clone(),
            None => crate::forge::traits::PullRequestDetails {
                repository: pr.key.repository.clone(),
                number: pr.key.number,
                title: pr.title.clone(),
                url: pr.url.clone(),
                state: pr.state.clone(),
                is_draft: false,
                author: None,
                head_ref_name: pr.head_ref_name.clone(),
                base_ref_name: pr.base_ref_name.clone(),
                head_sha: pr.key.head_sha.clone(),
                base_sha: pr.base_sha.clone(),
                body: String::new(),
                updated_at: None,
                closed: pr.closed,
                merged_at: None,
                diff_start_sha: None,
            },
        };
        let backend = self
            .forge_backend
            .as_deref()
            .ok_or_else(|| TuicrError::UnsupportedOperation("Not in PR mode".to_string()))?;
        backend.resolve_thread(&details, &thread_id, resolved)?;
        self.forge_review_threads[thread_idx].is_resolved = resolved;
        self.rebuild_annotations();
        let navigator_items = self.build_comment_navigator_items();
        self.sync_comment_navigator_selection(&navigator_items);
        if navigator_items.is_empty() && self.focused_panel == FocusedPanel::Comments {
            self.focused_panel = FocusedPanel::Diff;
        }
        self.diff_state.cursor_line = self.diff_state.cursor_line.min(self.max_cursor_line());
        self.ensure_cursor_visible();
        self.set_message(if resolved {
            "Thread resolved"
        } else {
            "Thread reopened"
        });
        Ok(())
    }
}
