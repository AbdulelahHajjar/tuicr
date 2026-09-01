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
    pub review_id: u64,
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
            let id = reviews
                .reviews
                .iter()
                .map(|review| review.id)
                .max()
                .unwrap_or(0)
                + 1;
            let submitted_at = Utc::now();
            let review = LocalReview {
                id,
                event: event.to_string(),
                body: body.to_string(),
                commit_id: commit_id.to_string(),
                author: author.to_string(),
                submitted_at,
            };
            if event != "PENDING" {
                for pending in &mut reviews.reviews {
                    if pending.event == "PENDING" && pending.author == author {
                        pending.event = event.to_string();
                        pending.submitted_at = submitted_at;
                    }
                }
            }
            for thread in &mut new_threads {
                thread.review_id = id;
            }
            reviews.reviews.push(review.clone());
            threads.threads.extend(new_threads);
            self.save_json(&self.reviews_path(number), &reviews)?;
            self.save_json(&self.threads_path(number), &threads)?;
            Ok(review)
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
        repository.owner.replace('/', "-"),
        repository.name
    )
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
    fn should_sanitize_owner_slashes_in_store_directory_name() {
        assert_eq!(
            store_directory_name(&ForgeRepository::local("org/team", "project")),
            "org-team__project"
        );
    }
}
