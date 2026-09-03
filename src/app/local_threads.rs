use std::path::PathBuf;

use super::*;
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
