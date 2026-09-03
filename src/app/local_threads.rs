use std::path::PathBuf;

use super::*;
use crate::forge::remote_comments::RemoteReviewComment;
use crate::forge::submit::comment_type_prefix;
use crate::forge::traits::{CreateThreadRequest, ForgeKind, PullRequestDetails};

impl App {
    /// Details of the open pull request for a backend call: the fetched
    /// info when present, otherwise the fields the diff source carries.
    pub(in crate::app) fn pr_details_snapshot(&self) -> Option<PullRequestDetails> {
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return None;
        };
        Some(match self.pr_info.as_ref() {
            Some(info) => info.details.clone(),
            None => PullRequestDetails {
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
        })
    }

    /// Whether a new inline comment on the open pull request becomes a
    /// forge thread at save time instead of a session draft. Only open
    /// Local pull requests take threads; a closed one is read-only.
    pub(in crate::app) fn comments_go_to_threads(&self) -> bool {
        match &self.diff_source {
            DiffSource::PullRequest(pr) => {
                pr.key.repository.kind == ForgeKind::Local && !pr.is_read_only()
            }
            _ => false,
        }
    }

    /// True when the diff cursor is on a thread row of a Local pull request,
    /// where threads can be edited and deleted by their author.
    pub fn cursor_on_local_thread(&self) -> bool {
        self.cursor_on_remote_thread() && self.forge_kind() == Some(ForgeKind::Local)
    }

    fn local_thread_at_cursor(&self) -> Result<usize> {
        if self.forge_kind() != Some(ForgeKind::Local) {
            let forge = self.forge_display_name();
            return Err(TuicrError::UnsupportedOperation(format!(
                "Editing review threads is not supported on {forge}"
            )));
        }
        match self.line_annotations.get(self.diff_state.cursor_line) {
            Some(AnnotatedLine::RemoteThreadLine { thread_idx }) => Ok(*thread_idx),
            _ => Err(TuicrError::Forge("No review thread at cursor".to_string())),
        }
    }

    /// The thread under the cursor together with its root comment, provided
    /// the viewer wrote that comment.
    fn own_thread_root_at_cursor(&self, verb: &str) -> Result<(usize, &RemoteReviewComment)> {
        let thread_idx = self.local_thread_at_cursor()?;
        let root = self
            .forge_review_threads
            .get(thread_idx)
            .and_then(|thread| thread.comments.first())
            .ok_or_else(|| TuicrError::Forge("No review thread at cursor".to_string()))?;
        if root.author.as_deref() != Some(self.username.as_str()) {
            return Err(TuicrError::Forge(format!(
                "Thread by {} — only its author can {verb} it",
                root.author.as_deref().unwrap_or("someone else")
            )));
        }
        Ok((thread_idx, root))
    }

    /// Delete the thread under the cursor. The backend refuses a root
    /// comment that already has replies, so the replies keep their context.
    pub fn delete_local_thread_at_cursor(&mut self) -> Result<()> {
        let (thread_idx, _) = self.own_thread_root_at_cursor("delete")?;
        let thread_id = self.forge_review_threads[thread_idx].id.clone();
        let details = self
            .pr_details_snapshot()
            .ok_or_else(|| TuicrError::UnsupportedOperation("Not in PR mode".to_string()))?;
        let backend = self
            .forge_backend
            .as_deref()
            .ok_or_else(|| TuicrError::UnsupportedOperation("Not in PR mode".to_string()))?;
        let thread_deleted =
            backend.delete_thread_comment(&details, &thread_id, None, &self.username)?;
        if thread_deleted {
            self.forge_review_threads.remove(thread_idx);
        }
        self.rebuild_annotations();
        let items = self.build_comment_navigator_items();
        self.sync_comment_navigator_selection(&items);
        if items.is_empty() && self.focused_panel == FocusedPanel::Comments {
            self.focused_panel = FocusedPanel::Diff;
        }
        self.diff_state.cursor_line = self.diff_state.cursor_line.min(self.max_cursor_line());
        self.ensure_cursor_visible();
        self.set_message("Thread deleted");
        Ok(())
    }

    /// Open the root comment of the thread under the cursor in the comment
    /// box. `cursor_at_end` places the text cursor after the last character.
    pub fn edit_local_thread_at_cursor(&mut self, cursor_at_end: bool) -> Result<()> {
        let (thread_idx, root) = self.own_thread_root_at_cursor("edit")?;
        let edit = EditingThread {
            thread_idx,
            thread_id: self.forge_review_threads[thread_idx].id.clone(),
            comment_id: root.id.clone(),
        };
        let body = root.body.clone();
        self.input_mode = InputMode::Comment;
        self.diff_state.scroll_x = 0;
        self.comment_cursor = if cursor_at_end { body.len() } else { 0 };
        self.comment_buffer = body;
        self.comment_type = self.default_comment_type();
        self.comment_is_review_level = false;
        self.comment_is_file_level = false;
        self.comment_line = None;
        self.comment_line_range = None;
        self.editing_comment_id = None;
        self.editing_thread = Some(edit);
        Ok(())
    }

    /// Write the edited body of `edit` through the backend and mirror it in
    /// the in-memory thread.
    pub(in crate::app) fn save_local_thread_edit(
        &mut self,
        edit: &EditingThread,
        content: &str,
    ) -> Result<()> {
        let details = self
            .pr_details_snapshot()
            .ok_or_else(|| TuicrError::UnsupportedOperation("Not in PR mode".to_string()))?;
        let backend = self
            .forge_backend
            .as_deref()
            .ok_or_else(|| TuicrError::UnsupportedOperation("Not in PR mode".to_string()))?;
        backend.update_thread_comment(
            &details,
            &edit.thread_id,
            Some(&edit.comment_id),
            &self.username,
            content,
        )?;
        if let Some(comment) = self
            .forge_review_threads
            .iter_mut()
            .find(|thread| thread.id == edit.thread_id)
            .and_then(|thread| {
                thread
                    .comments
                    .iter_mut()
                    .find(|comment| comment.id == edit.comment_id)
            })
        {
            comment.body = content.to_string();
        }
        self.rebuild_annotations();
        Ok(())
    }

    /// Open a thread at `line` on `side` of `path` through the forge backend
    /// and show it without waiting for a refetch. Returns the anchored line.
    pub(in crate::app) fn create_local_thread_for_comment(
        &mut self,
        path: PathBuf,
        line: u32,
        side: LineSide,
        content: &str,
    ) -> Result<u32> {
        let not_in_pr_mode = || TuicrError::UnsupportedOperation("Not in PR mode".to_string());
        let details = self.pr_details_snapshot().ok_or_else(not_in_pr_mode)?;
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return Err(not_in_pr_mode());
        };
        // Anchor against the displayed diff: the selected commit subset when
        // the inline selector narrows the view, otherwise the full pull request.
        let pair = self.pr_range_sha_pair().filter(|_| {
            Self::is_strict_commit_selection(self.commit_selection_range, self.pr_commits.len())
        });
        let commit_id = pair
            .as_ref()
            .map(|(_, end)| end.clone())
            .unwrap_or_else(|| pr.key.head_sha.clone());
        let diff_start_sha = pair.map(|(start, _)| start);
        let body = format!(
            "{}{content}",
            comment_type_prefix(&self.comment_type, &self.forge_config)
        );
        let backend = self.forge_backend.as_deref().ok_or_else(not_in_pr_mode)?;
        let thread = backend.create_thread(
            &details,
            CreateThreadRequest {
                path: &path,
                line,
                side: side.into(),
                body: &body,
                author: Some(&self.username),
                commit_id: &commit_id,
                diff_start_sha: diff_start_sha.as_deref(),
            },
        )?;
        if self.forge_review_threads_loading {
            // A fetch in flight replaces the list when it lands, so re-read
            // the store (which now holds the thread) instead of pushing.
            self.refetch_pr_threads();
        } else {
            self.forge_review_threads.push(thread);
        }
        self.rebuild_annotations();
        let items = self.build_comment_navigator_items();
        self.sync_comment_navigator_selection(&items);
        self.ensure_cursor_visible();
        Ok(line)
    }
}
