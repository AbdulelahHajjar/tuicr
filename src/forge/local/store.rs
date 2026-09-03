use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Result, TuicrError};
use crate::forge::traits::ForgeRepository;
use crate::persistence::storage::{get_reviews_dir, with_directory_lock, write_atomic};

const STORE_VERSION: u32 = 1;
const LOCK_FILENAME: &str = ".lock";

#[derive(Debug, Clone)]
pub(crate) struct LocalForgeStore {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocalPull {
    pub number: u64,
    pub head_ref: String,
    pub base_ref: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_head_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocalReview {
    pub id: u64,
    pub event: String,
    pub body: String,
    pub commit_id: String,
    pub author: String,
    pub submitted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocalThreadComment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocalThread {
    pub id: String,
    pub path: String,
    pub side: String,
    pub original_line: u32,
    pub original_commit: String,
    pub base_commit: String,
    pub line_text: String,
    pub created_at: DateTime<Utc>,
    pub is_resolved: bool,
    pub resolved_at: Option<DateTime<Utc>>,
    /// Review this thread was submitted with; `None` for threads created
    /// directly, outside any review.
    #[serde(default)]
    pub review_id: Option<u64>,
    pub comments: Vec<LocalThreadComment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PullsFile {
    version: u32,
    next_number: u64,
    pulls: Vec<LocalPull>,
}

impl Default for PullsFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            next_number: 1,
            pulls: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewsFile {
    version: u32,
    reviews: Vec<LocalReview>,
}

impl Default for ReviewsFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            reviews: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThreadsFile {
    version: u32,
    threads: Vec<LocalThread>,
}

impl Default for ThreadsFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            threads: Vec::new(),
        }
    }
}

impl LocalForgeStore {
    pub(crate) fn new(repository: &ForgeRepository) -> Result<Self> {
        let reviews_dir = get_reviews_dir()?;
        let data_dir = reviews_dir.parent().ok_or_else(|| {
            TuicrError::Io(std::io::Error::other(
                "tuicr reviews directory has no parent",
            ))
        })?;
        Ok(Self::at(
            data_dir
                .join("local-forge")
                .join(store_directory_name(repository)),
        ))
    }

    pub(crate) fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn find_pull_by_head(&self, head_ref: &str) -> Result<Option<LocalPull>> {
        Ok(self
            .load_pulls()?
            .pulls
            .into_iter()
            .find(|pull| pull.head_ref == head_ref))
    }

    pub(crate) fn pull(&self, number: u64) -> Result<LocalPull> {
        self.load_pulls()?
            .pulls
            .into_iter()
            .find(|pull| pull.number == number)
            .ok_or_else(|| TuicrError::Forge(format!("Local pull request #{number} was not found")))
    }

    pub(crate) fn open_pull(
        &self,
        head_ref: &str,
        base_ref: &str,
        head_sha: &str,
        update_base: bool,
    ) -> Result<LocalPull> {
        with_directory_lock(&self.root, LOCK_FILENAME, || {
            let mut file = self.load_pulls()?;
            let now = Utc::now();
            let pull =
                if let Some(pull) = file.pulls.iter_mut().find(|pull| pull.head_ref == head_ref) {
                    if update_base {
                        pull.base_ref = base_ref.to_string();
                    }
                    pull.last_head_sha = head_sha.to_string();
                    pull.updated_at = now;
                    pull.clone()
                } else {
                    let pull = LocalPull {
                        number: file.next_number,
                        head_ref: head_ref.to_string(),
                        base_ref: base_ref.to_string(),
                        created_at: now,
                        updated_at: now,
                        last_head_sha: head_sha.to_string(),
                    };
                    file.next_number += 1;
                    file.pulls.push(pull.clone());
                    pull
                };
            self.save_json(&self.pulls_path(), &file)?;
            Ok(pull)
        })
    }

    pub(crate) fn reviews(&self, number: u64) -> Result<Vec<LocalReview>> {
        Ok(self.load_reviews(number)?.reviews)
    }

    pub(crate) fn threads(&self, number: u64) -> Result<Vec<LocalThread>> {
        Ok(self.load_threads(number)?.threads)
    }

    pub(crate) fn add_review(
        &self,
        number: u64,
        event: &str,
        body: &str,
        commit_id: &str,
        author: &str,
        mut new_threads: Vec<LocalThread>,
    ) -> Result<LocalReview> {
        with_directory_lock(&self.root, LOCK_FILENAME, || {
            let mut reviews = self.load_reviews(number)?;
            let mut threads = self.load_threads(number)?;
            let submitted_at = Utc::now();
            let pending = reviews
                .reviews
                .iter_mut()
                .find(|review| review.event == "PENDING" && review.author == author);
            let review = if let Some(pending) = pending {
                pending.event = event.to_string();
                if !body.is_empty() {
                    pending.body = body.to_string();
                }
                pending.commit_id = commit_id.to_string();
                pending.submitted_at = submitted_at;
                pending.clone()
            } else {
                let id = reviews
                    .reviews
                    .iter()
                    .map(|review| review.id)
                    .max()
                    .unwrap_or(0)
                    + 1;
                let review = LocalReview {
                    id,
                    event: event.to_string(),
                    body: body.to_string(),
                    commit_id: commit_id.to_string(),
                    author: author.to_string(),
                    submitted_at,
                };
                reviews.reviews.push(review.clone());
                review
            };
            for thread in &mut new_threads {
                thread.review_id = Some(review.id);
            }
            threads.threads.extend(new_threads);
            self.save_json(&self.reviews_path(number), &reviews)?;
            self.save_json(&self.threads_path(number), &threads)?;
            Ok(review)
        })
    }

    /// Append a thread that belongs to no review.
    pub(crate) fn add_thread(&self, number: u64, thread: LocalThread) -> Result<()> {
        with_directory_lock(&self.root, LOCK_FILENAME, || {
            let mut file = self.load_threads(number)?;
            file.threads.push(thread);
            self.save_json(&self.threads_path(number), &file)
        })
    }

    pub(crate) fn resolve_thread(
        &self,
        number: u64,
        thread_id: &str,
        resolved: bool,
    ) -> Result<()> {
        with_directory_lock(&self.root, LOCK_FILENAME, || {
            let mut file = self.load_threads(number)?;
            let thread = file
                .threads
                .iter_mut()
                .find(|thread| thread.id == thread_id)
                .ok_or_else(|| {
                    TuicrError::Forge(format!("Local review thread `{thread_id}` was not found"))
                })?;
            thread.is_resolved = resolved;
            thread.resolved_at = resolved.then(Utc::now);
            self.save_json(&self.threads_path(number), &file)
        })
    }

    pub(crate) fn reply_to_thread(
        &self,
        number: u64,
        thread_id: &str,
        author: &str,
        body: &str,
    ) -> Result<LocalThreadComment> {
        with_directory_lock(&self.root, LOCK_FILENAME, || {
            let mut file = self.load_threads(number)?;
            let thread = file
                .threads
                .iter_mut()
                .find(|thread| thread.id == thread_id)
                .ok_or_else(|| {
                    TuicrError::Forge(format!("Local review thread `{thread_id}` was not found"))
                })?;
            let comment = LocalThreadComment {
                id: uuid::Uuid::new_v4().to_string(),
                author: author.to_string(),
                body: body.to_string(),
                created_at: Utc::now(),
            };
            thread.comments.push(comment.clone());
            self.save_json(&self.threads_path(number), &file)?;
            Ok(comment)
        })
    }

    fn pulls_path(&self) -> PathBuf {
        self.root.join("pulls.json")
    }

    fn reviews_path(&self, number: u64) -> PathBuf {
        self.root
            .join("pulls")
            .join(number.to_string())
            .join("reviews.json")
    }

    fn threads_path(&self, number: u64) -> PathBuf {
        self.root
            .join("pulls")
            .join(number.to_string())
            .join("threads.json")
    }

    fn load_pulls(&self) -> Result<PullsFile> {
        load_json(&self.pulls_path())
    }

    fn load_reviews(&self, number: u64) -> Result<ReviewsFile> {
        load_json(&self.reviews_path(number))
    }

    fn load_threads(&self, number: u64) -> Result<ThreadsFile> {
        load_json(&self.threads_path(number))
    }

    fn save_json(&self, path: &Path, value: &impl Serialize) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        write_atomic(path, &bytes)
    }
}

fn store_directory_name(repository: &ForgeRepository) -> String {
    format!(
        "{}__{}",
        sanitize_store_component(&repository.owner),
        sanitize_store_component(&repository.name)
    )
}

fn sanitize_store_component(value: &str) -> String {
    let mut value = value.replace(['/', '\\'], "-");
    while value.contains("..") {
        value = value.replace("..", "-");
    }
    if value.starts_with('.') {
        value.replace_range(..1, "-");
    }
    value
}

fn load_json<T>(path: &Path) -> Result<T>
where
    T: Default + for<'de> Deserialize<'de> + Versioned,
{
    if !path.exists() {
        return Ok(T::default());
    }
    let value: T = serde_json::from_slice(&fs::read(path)?)?;
    if value.version() != STORE_VERSION {
        return Err(TuicrError::CorruptedSession(format!(
            "unsupported local forge store version {} in {}",
            value.version(),
            path.display()
        )));
    }
    Ok(value)
}

trait Versioned {
    fn version(&self) -> u32;
}

impl Versioned for PullsFile {
    fn version(&self) -> u32 {
        self.version
    }
}

impl Versioned for ReviewsFile {
    fn version(&self) -> u32 {
        self.version
    }
}

impl Versioned for ThreadsFile {
    fn version(&self) -> u32 {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_keep_pull_number_when_branch_closes_and_reopens() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalForgeStore::at(temp.path().join("store"));

        let first = store
            .open_pull("feature", "develop", "aaaa", false)
            .unwrap();
        let reopened = store
            .open_pull("feature", "develop", "bbbb", false)
            .unwrap();

        assert_eq!(first.number, reopened.number);
        assert_eq!(reopened.last_head_sha, "bbbb");
        assert!(store.root().join("pulls.json").is_file());
    }

    #[test]
    fn should_sanitize_repository_fields_in_store_directory_name() {
        assert_eq!(
            store_directory_name(&ForgeRepository::local(
                "../org\\team",
                ".project/../name\\part"
            )),
            "--org-team__-project---name-part"
        );
    }

    #[test]
    fn should_submit_pending_review_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalForgeStore::at(temp.path().join("store"));
        store.open_pull("feature", "main", "aaaa", false).unwrap();
        let draft = store
            .add_review(1, "PENDING", "draft", "aaaa", "author", Vec::new())
            .unwrap();
        let thread = LocalThread {
            id: "thread".to_string(),
            path: "src/lib.rs".to_string(),
            side: "RIGHT".to_string(),
            original_line: 1,
            original_commit: "aaaa".to_string(),
            base_commit: "base".to_string(),
            line_text: "line".to_string(),
            created_at: Utc::now(),
            is_resolved: false,
            resolved_at: None,
            review_id: None,
            comments: Vec::new(),
        };
        let submitted = store
            .add_review(1, "COMMENT", "final", "bbbb", "author", vec![thread])
            .unwrap();

        let reviews = store.reviews(1).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].event, "COMMENT");
        assert_eq!(reviews[0].body, "final");
        assert_eq!(submitted.id, draft.id);
        assert_eq!(store.threads(1).unwrap()[0].review_id, Some(draft.id));
    }

    #[test]
    fn should_reuse_pending_review_across_drafts_and_submission() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalForgeStore::at(temp.path().join("store"));
        store.open_pull("feature", "main", "aaaa", false).unwrap();
        let first = thread("first");
        let second = thread("second");

        let draft = store
            .add_review(1, "PENDING", "first body", "aaaa", "author", vec![first])
            .unwrap();
        let second_draft = store
            .add_review(1, "PENDING", "second body", "bbbb", "author", vec![second])
            .unwrap();
        let approved = store
            .add_review(1, "APPROVE", "", "cccc", "author", Vec::new())
            .unwrap();

        assert_eq!(draft.id, second_draft.id);
        assert_eq!(draft.id, approved.id);
        assert_eq!(store.reviews(1).unwrap().len(), 1);
        assert_eq!(approved.event, "APPROVE");
        assert_eq!(approved.body, "second body");
        let threads = store.threads(1).unwrap();
        assert_eq!(threads.len(), 2);
        assert!(
            threads
                .iter()
                .all(|thread| thread.review_id == Some(draft.id))
        );
    }

    fn thread(id: &str) -> LocalThread {
        LocalThread {
            id: id.to_string(),
            path: "src/lib.rs".to_string(),
            side: "RIGHT".to_string(),
            original_line: 1,
            original_commit: "aaaa".to_string(),
            base_commit: "base".to_string(),
            line_text: "line".to_string(),
            created_at: Utc::now(),
            is_resolved: false,
            resolved_at: None,
            review_id: None,
            comments: Vec::new(),
        }
    }

    #[test]
    fn should_append_thread_without_review() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalForgeStore::at(temp.path().join("store"));
        store.open_pull("feature", "main", "aaaa", false).unwrap();

        store.add_thread(1, thread("direct")).unwrap();

        let threads = store.threads(1).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "direct");
        assert_eq!(threads[0].review_id, None);
        assert!(store.reviews(1).unwrap().is_empty());
        assert!(!store.reviews_path(1).exists());
    }

    #[test]
    fn should_load_threads_with_numeric_or_missing_review_id() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalForgeStore::at(temp.path().join("store"));
        let mut reviewed = thread("reviewed");
        reviewed.review_id = Some(3);
        let mut stored = serde_json::to_value(&reviewed).unwrap();
        let mut direct = serde_json::to_value(thread("direct")).unwrap();
        direct.as_object_mut().unwrap().remove("review_id");
        stored = serde_json::json!({ "version": 1, "threads": [stored, direct] });
        fs::create_dir_all(store.threads_path(1).parent().unwrap()).unwrap();
        fs::write(store.threads_path(1), stored.to_string()).unwrap();

        let threads = store.threads(1).unwrap();

        assert_eq!(threads[0].review_id, Some(3));
        assert_eq!(threads[1].review_id, None);
        assert!(serde_json::to_value(&threads[1]).unwrap()["review_id"].is_null());
    }

    #[test]
    fn should_reject_unknown_store_version() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalForgeStore::at(temp.path().join("store"));
        fs::create_dir_all(store.root()).unwrap();
        fs::write(
            store.root().join("pulls.json"),
            r#"{"version":2,"next_number":1,"pulls":[]}"#,
        )
        .unwrap();

        assert!(store.find_pull_by_head("feature").is_err());
    }
}
