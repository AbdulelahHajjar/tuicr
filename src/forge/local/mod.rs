mod anchor;
pub(crate) mod store;
pub(crate) mod target;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use git2::{Oid, Repository, Sort};

use crate::error::{Result, TuicrError};
use crate::forge::remote_comments::{
    RemoteCommentSide, RemoteReviewComment, RemoteReviewState, RemoteReviewSummary,
    RemoteReviewThread,
};
use crate::forge::submit::GhSide;
use crate::forge::traits::{
    CreateReviewRequest, CreateThreadRequest, ForgeBackend, ForgeFileLinesRequest, ForgeRepository,
    GhCreateReviewResponse, PagedPullRequests, PullRequestCommit, PullRequestDetails,
    PullRequestHeadStatus, PullRequestInfo, PullRequestListQuery, PullRequestReviewMetadata,
    PullRequestReviewRecord, PullRequestReviewStatus, PullRequestSummary, PullRequestTarget,
};
use crate::model::{DiffFile, DiffLine, FilePatch};
use crate::syntax::SyntaxHighlighter;
use crate::vcs::diff_parser::parse_file_patches;
use crate::vcs::git::raw::run_git_diff;
use crate::vcs::slice_context_lines;

use self::store::{LocalForgeStore, LocalThread, LocalThreadComment};
use self::target::{branch_tip, pull_is_closed, resolve_default_base, resolve_ref_oid};

/// Forge backend that reviews branches and stores review state locally.
#[derive(Debug, Clone)]
pub struct LocalForgeBackend {
    repository: ForgeRepository,
    checkout: Option<PathBuf>,
    store: Option<LocalForgeStore>,
    author: Option<String>,
}

impl LocalForgeBackend {
    /// Create a local forge backed by `checkout` and tuicr's data directory.
    pub fn new(repository: ForgeRepository, checkout: Option<PathBuf>) -> Self {
        Self {
            repository,
            checkout,
            store: None,
            author: None,
        }
    }

    #[cfg(test)]
    fn with_store(repository: ForgeRepository, checkout: PathBuf, store: LocalForgeStore) -> Self {
        Self {
            repository,
            checkout: Some(checkout),
            store: Some(store),
            author: Some("Test User".to_string()),
        }
    }

    fn git(&self) -> Result<Repository> {
        Ok(Repository::discover(self.checkout()?)?)
    }

    fn checkout(&self) -> Result<&Path> {
        self.checkout.as_deref().ok_or_else(|| {
            TuicrError::Forge("Local pull requests require a checkout path".to_string())
        })
    }

    fn store(&self) -> Result<LocalForgeStore> {
        self.store
            .clone()
            .map(Ok)
            .unwrap_or_else(|| LocalForgeStore::new(&self.repository))
    }

    fn author(&self) -> Result<String> {
        if let Some(author) = &self.author {
            return Ok(author.clone());
        }
        Ok(self
            .git()?
            .config()?
            .get_string("user.name")
            .ok()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|name| !name.trim().is_empty())
            })
            .unwrap_or_else(|| "you".to_string()))
    }

    fn pull_url(&self, number: u64) -> String {
        format!(
            "local:{}/{}/pull/{number}",
            self.repository.owner, self.repository.name
        )
    }

    fn details_for_number(&self, number: u64) -> Result<PullRequestDetails> {
        let git = self.git()?;
        let store = self.store()?;
        let mut pull = store.pull(number)?;
        let closed = pull_is_closed(&git, &pull);
        let head_oid = if closed {
            Oid::from_str(&pull.last_head_sha).map_err(TuicrError::Git)?
        } else {
            let oid = branch_tip(&git, &pull.head_ref)?;
            pull = store.open_pull(&pull.head_ref, &pull.base_ref, &oid.to_string(), false)?;
            oid
        };
        let base_tip = resolve_ref_oid(&git, &pull.base_ref)?;
        let base_oid = git.merge_base(base_tip, head_oid)?;
        let tip = git.find_commit(head_oid)?;
        let commits = self.commits_between(&git, base_oid, head_oid)?;
        Ok(PullRequestDetails {
            repository: self.repository.clone(),
            number,
            title: tip.summary().unwrap_or("").to_string(),
            url: self.pull_url(number),
            state: if closed { "CLOSED" } else { "OPEN" }.to_string(),
            is_draft: false,
            author: tip.author().name().map(str::to_string),
            head_ref_name: pull.head_ref,
            base_ref_name: pull.base_ref,
            head_sha: head_oid.to_string(),
            base_sha: base_oid.to_string(),
            body: pull_body(&git, &commits)?,
            updated_at: commit_time(&tip),
            closed,
            merged_at: None,
            diff_start_sha: None,
        })
    }

    fn commits_between(
        &self,
        git: &Repository,
        base: Oid,
        head: Oid,
    ) -> Result<Vec<PullRequestCommit>> {
        let mut walk = git.revwalk()?;
        walk.push(head)?;
        walk.hide(base)?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
        walk.map(|oid| {
            let commit = git.find_commit(oid?)?;
            let oid = commit.id().to_string();
            Ok(PullRequestCommit {
                short_oid: oid.chars().take(7).collect(),
                oid,
                summary: commit.summary().unwrap_or("").to_string(),
                author: commit.author().name().unwrap_or("").to_string(),
                timestamp: commit_time(&commit),
            })
        })
        .collect()
    }

    fn read_blob(&self, sha: &str, path: &Path) -> Result<String> {
        let git = self.git()?;
        let commit = git.revparse_single(sha)?.peel_to_commit()?;
        let entry = commit.tree()?.get_path(path)?;
        let blob = git.find_blob(entry.id())?;
        Ok(String::from_utf8_lossy(blob.content()).into_owned())
    }

    fn parsed_diff(&self, patches: Vec<FilePatch>) -> Result<Vec<DiffFile>> {
        match parse_file_patches(patches, &SyntaxHighlighter::default()) {
            Ok(files) => Ok(files),
            Err(TuicrError::NoChanges) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// The diff a comment anchor was mapped against: the displayed commit
    /// subset when `diff_start_sha` is set, otherwise the full pull request.
    fn anchor_diff(
        &self,
        pr: &PullRequestDetails,
        diff_start_sha: Option<&str>,
        commit_id: &str,
    ) -> Result<Vec<DiffFile>> {
        let range = format!("{}..{}", diff_start_sha.unwrap_or(&pr.base_sha), commit_id);
        self.parsed_diff(run_git_diff(self.checkout()?, &[&range])?)
    }

    fn new_thread(
        files: &[DiffFile],
        request: &CreateThreadRequest<'_>,
        base_sha: &str,
        author: &str,
        now: DateTime<Utc>,
    ) -> LocalThread {
        let side = match request.side {
            GhSide::Right => RemoteCommentSide::Right,
            GhSide::Left => RemoteCommentSide::Left,
        };
        LocalThread {
            id: uuid::Uuid::new_v4().to_string(),
            path: request.path.to_string_lossy().replace('\\', "/"),
            side: request.side.as_str().to_string(),
            original_line: request.line,
            original_commit: request.commit_id.to_string(),
            base_commit: base_sha.to_string(),
            line_text: anchor::line_text(files, request.path, side, request.line)
                .unwrap_or_default(),
            created_at: now,
            is_resolved: false,
            resolved_at: None,
            review_id: None,
            comments: vec![LocalThreadComment {
                id: uuid::Uuid::new_v4().to_string(),
                author: author.to_string(),
                body: request.body.to_string(),
                created_at: now,
                updated_at: None,
            }],
        }
    }

    fn writable(&self, pr: &PullRequestDetails) -> Result<()> {
        if pr.is_read_only() {
            return Err(TuicrError::Forge(
                "Cannot update a closed local pull request".to_string(),
            ));
        }
        Ok(())
    }

    fn remote_thread(
        &self,
        number: u64,
        thread: LocalThread,
        line: Option<u32>,
        is_outdated: bool,
    ) -> RemoteReviewThread {
        let root_id = thread.comments.first().map(|comment| comment.id.clone());
        let comments = thread
            .comments
            .into_iter()
            .enumerate()
            .map(|(index, comment)| RemoteReviewComment {
                url: format!("{}#comment-{}", self.pull_url(number), comment.id),
                id: comment.id,
                author: Some(comment.author),
                body: comment.body,
                created_at: Some(comment.created_at),
                in_reply_to: (index > 0).then(|| root_id.clone()).flatten(),
            })
            .collect();
        RemoteReviewThread {
            id: thread.id,
            path: thread.path,
            line,
            side: RemoteCommentSide::parse(&thread.side),
            is_resolved: thread.is_resolved,
            is_outdated,
            comments,
        }
    }

    /// Open a thread that belongs to no review and return the stored record.
    pub(crate) fn create_local_thread(
        &self,
        pr: &PullRequestDetails,
        request: &CreateThreadRequest<'_>,
    ) -> Result<LocalThread> {
        if pr.is_read_only() {
            return Err(TuicrError::Forge(
                "Cannot comment on a closed local pull request".to_string(),
            ));
        }
        let files = self.anchor_diff(pr, request.diff_start_sha, request.commit_id)?;
        if !files.iter().any(|file| file.display_path() == request.path) {
            return Err(TuicrError::Forge(format!(
                "`{}` is not part of the pull request diff",
                request.path.display()
            )));
        }
        let author = match request.author {
            Some(author) => author.to_string(),
            None => self.author()?,
        };
        let thread = Self::new_thread(&files, request, &pr.base_sha, &author, Utc::now());
        self.store()?.add_thread(pr.number, thread.clone())?;
        Ok(thread)
    }
}

impl ForgeBackend for LocalForgeBackend {
    fn list_pull_requests(&self, query: PullRequestListQuery) -> Result<PagedPullRequests> {
        let git = self.git()?;
        let store = self.store()?;
        let default_base = resolve_default_base(&git)?;
        let default_base_oid = resolve_ref_oid(&git, &default_base)?;
        let mut rows = Vec::new();
        for branch in git.branches(Some(git2::BranchType::Local))? {
            let (branch, _) = branch?;
            let Some(name) = branch.name()?.map(str::to_string) else {
                continue;
            };
            if name == default_base {
                continue;
            }
            let tip = branch.get().peel_to_commit()?;
            let base_ref = store
                .find_pull_by_head(&name)?
                .map(|pull| pull.base_ref)
                .unwrap_or_else(|| default_base.clone());
            let base_oid = resolve_ref_oid(&git, &base_ref).unwrap_or(default_base_oid);
            if git.graph_ahead_behind(tip.id(), base_oid)?.0 == 0 {
                continue;
            }
            let pull = store.open_pull(&name, &base_ref, &tip.id().to_string(), false)?;
            rows.push((tip.time().seconds(), pull, tip));
        }
        rows.sort_by_key(|(seconds, _, _)| std::cmp::Reverse(*seconds));
        let page_size = query.page_size.max(1);
        let has_more = rows.len() > query.already_loaded + page_size;
        let pull_requests = rows
            .into_iter()
            .skip(query.already_loaded)
            .take(page_size)
            .map(|(_, pull, tip)| PullRequestSummary {
                repository: self.repository.clone(),
                number: pull.number,
                title: tip.summary().unwrap_or("").to_string(),
                author: tip.author().name().map(str::to_string),
                head_ref_name: pull.head_ref,
                base_ref_name: pull.base_ref,
                updated_at: commit_time(&tip),
                url: self.pull_url(pull.number),
                state: "OPEN".to_string(),
                is_draft: false,
            })
            .collect::<Vec<_>>();
        Ok(PagedPullRequests {
            total_loaded: query.already_loaded + pull_requests.len(),
            pull_requests,
            has_more,
        })
    }

    fn get_pull_request(&self, target: PullRequestTarget) -> Result<PullRequestDetails> {
        self.details_for_number(target.number)
    }

    fn get_pull_request_info(&self, target: PullRequestTarget) -> Result<PullRequestInfo> {
        let details = self.details_for_number(target.number)?;
        let reviews = self.store()?.reviews(target.number)?;
        let non_pending = reviews
            .iter()
            .filter(|review| review.event != "PENDING")
            .collect::<Vec<_>>();
        let review_decision = non_pending
            .last()
            .map(|review| review_state_name(&review.event).to_string());
        let mut seen = HashSet::new();
        let mut latest_reviews = non_pending
            .into_iter()
            .rev()
            .filter(|review| seen.insert(review.author.clone()))
            .map(|review| PullRequestReviewStatus {
                author: Some(review.author.clone()),
                state: review_state_name(&review.event).to_string(),
                submitted_at: Some(review.submitted_at),
            })
            .collect::<Vec<_>>();
        latest_reviews.reverse();
        let mut info = PullRequestInfo::from_details(details);
        info.review_decision = review_decision;
        info.latest_reviews = latest_reviews;
        Ok(info)
    }

    fn get_pull_request_diff(&self, pr: &PullRequestDetails) -> Result<Vec<FilePatch>> {
        let range = format!("{}..{}", pr.base_sha, pr.head_sha);
        run_git_diff(self.checkout()?, &[&range])
    }

    fn head_status(&self, pr: &PullRequestDetails) -> Result<Option<PullRequestHeadStatus>> {
        let git = self.git()?;
        let Ok(reference) = git.find_reference(&format!("refs/heads/{}", pr.head_ref_name)) else {
            return Ok(Some(PullRequestHeadStatus::Closed));
        };
        let head = reference.peel_to_commit()?.id().to_string();
        Ok(Some(PullRequestHeadStatus::Open(head)))
    }

    fn fetch_file_lines(&self, request: ForgeFileLinesRequest) -> Result<Vec<DiffLine>> {
        if request.start_line == 0 || request.start_line > request.end_line {
            return Ok(Vec::new());
        }
        let content = self.read_blob(request.sha(), &request.path)?;
        Ok(slice_context_lines(
            &content,
            request.start_line,
            request.end_line,
        ))
    }

    fn file_line_count(&self, request: ForgeFileLinesRequest) -> Result<u32> {
        Ok(self
            .read_blob(request.sha(), &request.path)?
            .lines()
            .count() as u32)
    }

    fn list_review_threads(&self, pr: &PullRequestDetails) -> Result<Vec<RemoteReviewThread>> {
        let threads = self.store()?.threads(pr.number)?;
        if threads.is_empty() {
            return Ok(Vec::new());
        }
        let files = self.parsed_diff(self.get_pull_request_diff(pr)?)?;
        Ok(threads
            .into_iter()
            .map(|thread| {
                let (line, is_outdated) = anchor::reanchor(&thread, &pr.head_sha, &files);
                self.remote_thread(pr.number, thread, line, is_outdated)
            })
            .collect())
    }

    fn list_review_summaries(&self, pr: &PullRequestDetails) -> Result<Vec<RemoteReviewSummary>> {
        Ok(self
            .store()?
            .reviews(pr.number)?
            .into_iter()
            .filter(|review| review.event != "PENDING" && !review.body.trim().is_empty())
            .map(|review| RemoteReviewSummary {
                id: review.id.to_string(),
                author: Some(review.author),
                body: review.body,
                state: RemoteReviewState::parse(&review.event),
                created_at: Some(review.submitted_at),
                url: format!("{}#review-{}", self.pull_url(pr.number), review.id),
            })
            .collect())
    }

    fn list_pull_request_commits(&self, pr: &PullRequestDetails) -> Result<Vec<PullRequestCommit>> {
        self.commits_between(
            &self.git()?,
            Oid::from_str(&pr.base_sha)?,
            Oid::from_str(&pr.head_sha)?,
        )
    }

    fn list_pull_request_review_metadata(
        &self,
        pr: &PullRequestDetails,
    ) -> Result<PullRequestReviewMetadata> {
        let author = self.author()?;
        Ok(PullRequestReviewMetadata {
            viewer_login: Some(author),
            reviews: self
                .store()?
                .reviews(pr.number)?
                .into_iter()
                .filter(|review| review.event != "PENDING")
                .map(|review| PullRequestReviewRecord {
                    author: Some(review.author),
                    submitted_at: Some(review.submitted_at),
                    commit_oid: Some(review.commit_id),
                })
                .collect(),
        })
    }

    fn get_pull_request_commit_range_diff(
        &self,
        _pr: &PullRequestDetails,
        start_sha: &str,
        end_sha: &str,
    ) -> Result<Vec<FilePatch>> {
        let range = format!("{start_sha}..{end_sha}");
        run_git_diff(self.checkout()?, &[&range])
    }

    fn local_checkout_path(&self) -> Option<PathBuf> {
        self.checkout.clone()
    }

    fn create_review(
        &self,
        pr: &PullRequestDetails,
        request: CreateReviewRequest<'_>,
    ) -> Result<GhCreateReviewResponse> {
        if pr.is_read_only() {
            return Err(TuicrError::Forge(
                "Cannot review a closed local pull request".to_string(),
            ));
        }
        let files = self.anchor_diff(pr, request.diff_start_sha, request.commit_id)?;
        let author = self.author()?;
        let now = Utc::now();
        let threads = request
            .comments
            .iter()
            .map(|comment| {
                Self::new_thread(
                    &files,
                    &CreateThreadRequest {
                        path: &comment.path,
                        line: comment.line,
                        side: comment.side,
                        body: &comment.body,
                        author: None,
                        commit_id: request.commit_id,
                        diff_start_sha: request.diff_start_sha,
                    },
                    &pr.base_sha,
                    &author,
                    now,
                )
            })
            .collect();
        let event = request.event.github_event().unwrap_or("PENDING");
        let review = self.store()?.add_review(
            pr.number,
            event,
            request.body,
            request.commit_id,
            &author,
            threads,
        )?;
        Ok(GhCreateReviewResponse {
            id: review.id,
            html_url: format!("{}#review-{}", self.pull_url(pr.number), review.id),
            state: review_state_name(&review.event).to_string(),
        })
    }

    fn resolve_thread(
        &self,
        pr: &PullRequestDetails,
        thread_id: &str,
        resolved: bool,
    ) -> Result<()> {
        self.writable(pr)?;
        self.store()?.resolve_thread(pr.number, thread_id, resolved)
    }

    fn update_thread_comment(
        &self,
        pr: &PullRequestDetails,
        thread_id: &str,
        comment_id: Option<&str>,
        author: &str,
        body: &str,
    ) -> Result<()> {
        self.writable(pr)?;
        self.store()?
            .update_thread_comment(pr.number, thread_id, comment_id, author, body)?;
        Ok(())
    }

    fn delete_thread_comment(
        &self,
        pr: &PullRequestDetails,
        thread_id: &str,
        comment_id: Option<&str>,
        author: &str,
    ) -> Result<bool> {
        self.writable(pr)?;
        let (_, thread_deleted) = self
            .store()?
            .delete_thread_comment(pr.number, thread_id, comment_id, author)?;
        Ok(thread_deleted)
    }

    fn create_thread(
        &self,
        pr: &PullRequestDetails,
        request: CreateThreadRequest<'_>,
    ) -> Result<RemoteReviewThread> {
        let thread = self.create_local_thread(pr, &request)?;
        let line = Some(thread.original_line);
        Ok(self.remote_thread(pr.number, thread, line, false))
    }
}

fn commit_time(commit: &git2::Commit<'_>) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(commit.time().seconds(), 0)
}

fn review_state_name(event: &str) -> &str {
    match event {
        "APPROVE" => "APPROVED",
        "REQUEST_CHANGES" => "CHANGES_REQUESTED",
        "PENDING" => "PENDING",
        _ => "COMMENTED",
    }
}

fn pull_body(git: &Repository, commits: &[PullRequestCommit]) -> Result<String> {
    if commits.len() == 1 {
        let commit = git.find_commit(Oid::from_str(&commits[0].oid)?)?;
        return Ok(commit.body().unwrap_or("").to_string());
    }
    let mut body = commits
        .iter()
        .map(|commit| format!("- {} {}", commit.short_oid, commit.summary))
        .collect::<Vec<_>>()
        .join("\n");
    for item in commits {
        let commit = git.find_commit(Oid::from_str(&item.oid)?)?;
        if let Some(commit_body) = commit.body().filter(|body| !body.trim().is_empty()) {
            body.push('\n');
            for line in commit_body.lines() {
                body.push_str("    ");
                body.push_str(line);
                body.push('\n');
            }
            body.pop();
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use git2::{IndexAddOption, Signature, Time};

    use super::*;
    use crate::forge::submit::{InlineComment, SubmitEvent};
    use crate::forge::traits::{PullRequestListScope, PullRequestTarget};

    struct Fixture {
        _temp: tempfile::TempDir,
        checkout: PathBuf,
        store: LocalForgeStore,
        backend: LocalForgeBackend,
        base: Oid,
        first: Oid,
        second: Oid,
        pull_number: u64,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let checkout = temp.path().join("repo");
            fs::create_dir_all(&checkout).unwrap();
            let git = Repository::init(&checkout).unwrap();
            git.remote("origin", "https://github.com/owner/project.git")
                .unwrap();
            let base = commit(&git, "develop", "alpha\nbeta\n", "base", 1);
            git.reference("refs/heads/feature", base, true, "test")
                .unwrap();
            let first = commit(
                &git,
                "feature",
                "alpha\nbeta changed\n",
                "first\n\nfirst body",
                2,
            );
            let second = commit(
                &git,
                "feature",
                "alpha\nbeta changed\ngamma\n",
                "second\n\nsecond body",
                3,
            );
            git.set_head("refs/heads/feature").unwrap();
            git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .unwrap();
            let repository = ForgeRepository::local("owner", "project");
            let store = LocalForgeStore::at(temp.path().join("data/local-forge/owner__project"));
            let pull = store
                .open_pull("feature", "develop", &second.to_string(), false)
                .unwrap();
            let backend =
                LocalForgeBackend::with_store(repository, checkout.clone(), store.clone());
            Self {
                _temp: temp,
                checkout,
                store,
                backend,
                base,
                first,
                second,
                pull_number: pull.number,
            }
        }

        fn details(&self) -> PullRequestDetails {
            self.backend
                .get_pull_request(PullRequestTarget::with_repository(
                    self.backend.repository.clone(),
                    self.pull_number,
                    self.pull_number.to_string(),
                ))
                .unwrap()
        }
    }

    fn commit(git: &Repository, branch: &str, content: &str, message: &str, seconds: i64) -> Oid {
        fs::write(git.workdir().unwrap().join("file.txt"), content).unwrap();
        let mut index = git.index().unwrap();
        index
            .add_all(["file.txt"], IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let signature =
            Signature::new("Test User", "test@example.com", &Time::new(seconds, 0)).unwrap();
        let reference = format!("refs/heads/{branch}");
        let parents = git
            .find_reference(&reference)
            .ok()
            .and_then(|reference| reference.target())
            .map(|oid| git.find_commit(oid).unwrap())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        git.commit(
            Some(&reference),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )
        .unwrap()
    }

    fn inline_comment() -> InlineComment {
        InlineComment {
            path: PathBuf::from("file.txt"),
            line: 2,
            side: GhSide::Right,
            counterpart_line: None,
            start_line: None,
            start_side: None,
            range_anchors: None,
            old_path: None,
            body: "line comment".to_string(),
            comment_id: "local-comment".to_string(),
        }
    }

    #[test]
    fn should_list_commits_and_cumulative_and_range_diffs() {
        let fixture = Fixture::new();
        let details = fixture.details();

        let commits = fixture.backend.list_pull_request_commits(&details).unwrap();
        let cumulative = fixture.backend.get_pull_request_diff(&details).unwrap();
        let first_only = fixture
            .backend
            .get_pull_request_commit_range_diff(
                &details,
                &fixture.base.to_string(),
                &fixture.first.to_string(),
            )
            .unwrap();

        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].summary, "first");
        assert_eq!(commits[1].summary, "second");
        assert_eq!(cumulative.len(), 1);
        assert_eq!(first_only.len(), 1);
        assert!(details.body.contains("- "));
        assert!(details.body.contains("first body"));
        assert!(details.body.contains("second body"));
        assert!(!fixture.checkout.join(".git/local-forge").exists());
    }

    #[test]
    fn should_list_feature_branch_and_keep_its_pull_number() {
        let fixture = Fixture::new();
        let query = PullRequestListQuery {
            repository: fixture.backend.repository.clone(),
            already_loaded: 0,
            page_size: 10,
            scope: PullRequestListScope::ReviewRequested,
        };

        let page = fixture.backend.list_pull_requests(query).unwrap();

        assert_eq!(page.pull_requests.len(), 1);
        assert_eq!(page.pull_requests[0].number, fixture.pull_number);
        assert_eq!(page.pull_requests[0].head_ref_name, "feature");
    }

    #[test]
    fn should_list_newest_first_and_exclude_zero_ahead_branches() {
        let fixture = Fixture::new();
        let git = Repository::open(&fixture.checkout).unwrap();
        git.reference("refs/heads/newer", fixture.base, true, "test")
            .unwrap();
        commit(&git, "newer", "newer\n", "newer", 10);

        let page = fixture
            .backend
            .list_pull_requests(PullRequestListQuery {
                repository: fixture.backend.repository.clone(),
                already_loaded: 0,
                page_size: 10,
                scope: PullRequestListScope::Open,
            })
            .unwrap();

        assert_eq!(
            page.pull_requests
                .iter()
                .map(|pull| pull.head_ref_name.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "feature"]
        );
        assert!(
            !page
                .pull_requests
                .iter()
                .any(|pull| pull.head_ref_name == "develop")
        );
    }

    #[test]
    fn should_report_review_decision_and_latest_review_per_author() {
        let fixture = Fixture::new();
        fixture
            .store
            .add_review(
                fixture.pull_number,
                "COMMENT",
                "first",
                &fixture.first.to_string(),
                "alice",
                Vec::new(),
            )
            .unwrap();
        fixture
            .store
            .add_review(
                fixture.pull_number,
                "APPROVE",
                "approved",
                &fixture.second.to_string(),
                "bob",
                Vec::new(),
            )
            .unwrap();
        fixture
            .store
            .add_review(
                fixture.pull_number,
                "REQUEST_CHANGES",
                "latest",
                &fixture.second.to_string(),
                "alice",
                Vec::new(),
            )
            .unwrap();

        let info = fixture
            .backend
            .get_pull_request_info(PullRequestTarget::with_repository(
                fixture.backend.repository.clone(),
                fixture.pull_number,
                fixture.pull_number.to_string(),
            ))
            .unwrap();

        assert_eq!(info.review_decision.as_deref(), Some("CHANGES_REQUESTED"));
        assert_eq!(info.latest_reviews.len(), 2);
        assert_eq!(info.latest_reviews[0].author.as_deref(), Some("bob"));
        assert_eq!(info.latest_reviews[1].author.as_deref(), Some("alice"));
        assert_eq!(info.latest_reviews[1].state, "CHANGES_REQUESTED");
    }

    #[test]
    fn should_keep_session_key_at_same_head_and_change_it_after_commit() {
        let fixture = Fixture::new();
        let first_open = fixture.details();
        let second_open = fixture.details();
        let first_key = crate::forge::traits::PrSessionKey::from_details(&first_open);
        let second_key = crate::forge::traits::PrSessionKey::from_details(&second_open);
        let git = Repository::open(&fixture.checkout).unwrap();
        let advanced = commit(
            &git,
            "feature",
            "alpha\nbeta changed\ngamma\ndelta\n",
            "third",
            4,
        );
        let advanced_details = fixture.details();
        let advanced_key = crate::forge::traits::PrSessionKey::from_details(&advanced_details);

        assert_eq!(first_key, second_key);
        assert_eq!(first_key.number, advanced_key.number);
        assert_ne!(first_key.head_sha, advanced_key.head_sha);
        assert_eq!(advanced_key.head_sha, advanced.to_string());
    }

    #[test]
    fn should_close_deleted_branch_and_reopen_the_same_number() {
        let fixture = Fixture::new();
        let git = Repository::open(&fixture.checkout).unwrap();
        git.set_head("refs/heads/develop").unwrap();
        git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        git.find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();

        let closed = fixture.details();
        git.reference("refs/heads/feature", fixture.second, true, "reopen")
            .unwrap();
        let reopened = fixture.details();

        assert!(closed.closed);
        assert_eq!(closed.state, "CLOSED");
        assert!(!reopened.closed);
        assert_eq!(closed.number, reopened.number);
        assert_eq!(closed.head_sha, fixture.second.to_string());
    }

    #[test]
    fn should_reject_review_on_closed_pull() {
        let fixture = Fixture::new();
        let git = Repository::open(&fixture.checkout).unwrap();
        git.set_head("refs/heads/develop").unwrap();
        git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        git.find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();
        let details = fixture.details();

        let error = fixture
            .backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: &details.head_sha,
                    diff_start_sha: None,
                    body: "",
                    comments: &[],
                },
            )
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Cannot review a closed local pull request"
        );
    }

    #[test]
    fn should_create_all_review_events_promote_pending_and_persist_thread_text() {
        let fixture = Fixture::new();
        let details = fixture.details();
        let comment = inline_comment();

        let draft = fixture
            .backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Draft,
                    commit_id: &details.head_sha,
                    diff_start_sha: None,
                    body: "draft body",
                    comments: std::slice::from_ref(&comment),
                },
            )
            .unwrap();
        let commented = fixture
            .backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: &details.head_sha,
                    diff_start_sha: None,
                    body: "comment body",
                    comments: &[],
                },
            )
            .unwrap();
        let approved = fixture
            .backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Approve,
                    commit_id: &details.head_sha,
                    diff_start_sha: None,
                    body: "approved",
                    comments: &[],
                },
            )
            .unwrap();
        let changes = fixture
            .backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::RequestChanges,
                    commit_id: &details.head_sha,
                    diff_start_sha: None,
                    body: "changes",
                    comments: &[],
                },
            )
            .unwrap();

        let reviews = fixture.store.reviews(fixture.pull_number).unwrap();
        let stored_threads = fixture.store.threads(fixture.pull_number).unwrap();
        assert_eq!(draft.state, "PENDING");
        assert_eq!(commented.state, "COMMENTED");
        assert_eq!(approved.state, "APPROVED");
        assert_eq!(changes.state, "CHANGES_REQUESTED");
        assert_eq!(reviews[0].event, "COMMENT");
        assert_eq!(stored_threads[0].line_text, "beta changed");
        assert_eq!(stored_threads[0].review_id, Some(draft.id));

        let metadata = fixture
            .backend
            .list_pull_request_review_metadata(&details)
            .unwrap();
        assert_eq!(metadata.viewer_login.as_deref(), Some("Test User"));
        assert_eq!(metadata.reviews.len(), 3);

        let thread_id = stored_threads[0].id.clone();
        fixture
            .backend
            .resolve_thread(&details, &thread_id, true)
            .unwrap();
        let threads = fixture.backend.list_review_threads(&details).unwrap();
        assert_eq!(threads[0].line, Some(2));
        assert!(threads[0].is_resolved);
        assert!(!threads[0].is_outdated);
    }

    #[test]
    fn should_snapshot_line_text_from_the_selected_commit_range() {
        let fixture = Fixture::new();
        let git = Repository::open(&fixture.checkout).unwrap();
        let third = commit(&git, "feature", "alpha\nbeta final\ngamma\n", "third", 4);
        let details = fixture.details();
        let diff_start_sha = fixture.second.to_string();
        let comment = InlineComment {
            path: PathBuf::from("file.txt"),
            line: 2,
            side: GhSide::Left,
            counterpart_line: None,
            start_line: None,
            start_side: None,
            range_anchors: None,
            old_path: None,
            body: "line comment".to_string(),
            comment_id: "selected-range-comment".to_string(),
        };

        fixture
            .backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: &third.to_string(),
                    diff_start_sha: Some(&diff_start_sha),
                    body: "",
                    comments: &[comment],
                },
            )
            .unwrap();

        let threads = fixture.store.threads(fixture.pull_number).unwrap();
        assert_eq!(threads[0].line_text, "beta changed");
        assert_eq!(threads[0].base_commit, fixture.base.to_string());
    }

    fn thread_request<'a>(
        path: &'a Path,
        line: u32,
        side: GhSide,
        author: Option<&'a str>,
        commit_id: &'a str,
        diff_start_sha: Option<&'a str>,
    ) -> CreateThreadRequest<'a> {
        CreateThreadRequest {
            path,
            line,
            side,
            body: "[ISSUE] direct",
            author,
            commit_id,
            diff_start_sha,
        }
    }

    #[test]
    fn should_create_thread_with_line_text_author_and_no_review() {
        let fixture = Fixture::new();
        let details = fixture.details();
        let path = PathBuf::from("file.txt");

        let created = fixture
            .backend
            .create_thread(
                &details,
                thread_request(
                    &path,
                    2,
                    GhSide::Right,
                    Some("Claude Fable"),
                    &details.head_sha,
                    None,
                ),
            )
            .unwrap();

        let stored = fixture.store.threads(fixture.pull_number).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, created.id);
        assert_eq!(stored[0].side, "RIGHT");
        assert_eq!(stored[0].original_line, 2);
        assert_eq!(stored[0].line_text, "beta changed");
        assert_eq!(stored[0].original_commit, details.head_sha);
        assert_eq!(stored[0].base_commit, details.base_sha);
        assert_eq!(stored[0].review_id, None);
        assert_eq!(stored[0].comments[0].author, "Claude Fable");
        assert_eq!(stored[0].comments[0].body, "[ISSUE] direct");
        assert!(
            fixture
                .store
                .reviews(fixture.pull_number)
                .unwrap()
                .is_empty()
        );
        assert_eq!(created.line, Some(2));
        assert!(!created.is_outdated);
        assert_eq!(created.comments[0].author.as_deref(), Some("Claude Fable"));
        let listed = fixture.backend.list_review_threads(&details).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].line, Some(2));
    }

    #[test]
    fn should_default_thread_author_to_backend_author() {
        let fixture = Fixture::new();
        let details = fixture.details();
        let path = PathBuf::from("file.txt");

        fixture
            .backend
            .create_thread(
                &details,
                thread_request(&path, 2, GhSide::Right, None, &details.head_sha, None),
            )
            .unwrap();

        let stored = fixture.store.threads(fixture.pull_number).unwrap();
        assert_eq!(stored[0].comments[0].author, "Test User");
    }

    #[test]
    fn should_snapshot_thread_text_from_selected_commit_range() {
        let fixture = Fixture::new();
        let git = Repository::open(&fixture.checkout).unwrap();
        let third = commit(&git, "feature", "alpha\nbeta final\ngamma\n", "third", 4);
        let details = fixture.details();
        let path = PathBuf::from("file.txt");
        let diff_start_sha = fixture.second.to_string();
        let commit_id = third.to_string();

        fixture
            .backend
            .create_thread(
                &details,
                thread_request(
                    &path,
                    2,
                    GhSide::Left,
                    None,
                    &commit_id,
                    Some(&diff_start_sha),
                ),
            )
            .unwrap();

        let stored = fixture.store.threads(fixture.pull_number).unwrap();
        assert_eq!(stored[0].line_text, "beta changed");
        assert_eq!(stored[0].original_commit, commit_id);
        assert_eq!(stored[0].base_commit, fixture.base.to_string());
    }

    #[test]
    fn should_reject_creating_thread_on_closed_pull() {
        let fixture = Fixture::new();
        let mut details = fixture.details();
        details.closed = true;
        let path = PathBuf::from("file.txt");

        let error = fixture
            .backend
            .create_thread(
                &details,
                thread_request(&path, 2, GhSide::Right, None, &details.head_sha, None),
            )
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Cannot comment on a closed local pull request"
        );
        assert!(
            fixture
                .store
                .threads(fixture.pull_number)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn should_reject_thread_on_path_outside_diff() {
        let fixture = Fixture::new();
        let details = fixture.details();
        let path = PathBuf::from("missing.txt");

        let error = fixture
            .backend
            .create_thread(
                &details,
                thread_request(&path, 1, GhSide::Right, None, &details.head_sha, None),
            )
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "`missing.txt` is not part of the pull request diff"
        );
        assert!(
            fixture
                .store
                .threads(fixture.pull_number)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn should_edit_and_delete_thread_comments_through_the_backend() {
        let fixture = Fixture::new();
        let details = fixture.details();
        let path = PathBuf::from("file.txt");
        let created = fixture
            .backend
            .create_thread(
                &details,
                thread_request(
                    &path,
                    2,
                    GhSide::Right,
                    Some("user"),
                    &details.head_sha,
                    None,
                ),
            )
            .unwrap();

        fixture
            .backend
            .update_thread_comment(&details, &created.id, None, "user", "amended")
            .unwrap();
        let amended = fixture.store.threads(fixture.pull_number).unwrap();
        let thread_deleted = fixture
            .backend
            .delete_thread_comment(&details, &created.id, None, "user")
            .unwrap();

        assert_eq!(amended[0].comments[0].body, "amended");
        assert!(amended[0].comments[0].updated_at.is_some());
        assert!(thread_deleted);
        assert!(
            fixture
                .store
                .threads(fixture.pull_number)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn should_reject_thread_comment_changes_on_closed_pull() {
        let fixture = Fixture::new();
        let mut details = fixture.details();
        details.closed = true;

        let edit = fixture
            .backend
            .update_thread_comment(&details, "thread", None, "user", "x")
            .unwrap_err();
        let delete = fixture
            .backend
            .delete_thread_comment(&details, "thread", None, "user")
            .unwrap_err();

        assert_eq!(
            edit.to_string(),
            "Cannot update a closed local pull request"
        );
        assert_eq!(
            delete.to_string(),
            "Cannot update a closed local pull request"
        );
    }

    #[test]
    fn should_reject_resolving_thread_on_closed_pull_request() {
        let fixture = Fixture::new();
        let mut details = fixture.details();
        details.closed = true;

        let error = fixture
            .backend
            .resolve_thread(&details, "thread", true)
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Cannot update a closed local pull request"
        );
    }

    #[test]
    fn should_read_file_lines_from_git_blob() {
        let fixture = Fixture::new();
        let details = fixture.details();
        let request = ForgeFileLinesRequest {
            repository: fixture.backend.repository.clone(),
            base_sha: details.base_sha,
            head_sha: details.head_sha,
            path: PathBuf::from("file.txt"),
            status: crate::model::FileStatus::Modified,
            side: crate::forge::traits::ForgeFileSide::Head,
            start_line: 2,
            end_line: 3,
        };

        let lines = fixture.backend.fetch_file_lines(request.clone()).unwrap();
        let count = fixture.backend.file_line_count(request).unwrap();

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content, "beta changed");
        assert_eq!(count, 3);
    }
}
