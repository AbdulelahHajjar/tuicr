use std::path::{Path, PathBuf};

use git2::{Oid, Repository};

use crate::error::{Result, TuicrError};
use crate::forge::traits::{ForgeRepository, PullRequestTarget};
use crate::slug;

use super::store::{LocalForgeStore, LocalPull};

#[derive(Debug, Clone)]
pub(crate) struct ResolvedLocalTarget {
    pub repository: ForgeRepository,
    pub target: PullRequestTarget,
    pub checkout: PathBuf,
}

pub(crate) fn resolve_local_target(
    checkout: &Path,
    target: Option<&str>,
    base_override: Option<&str>,
) -> Result<ResolvedLocalTarget> {
    let git = Repository::discover(checkout)?;
    let checkout = git
        .workdir()
        .ok_or(TuicrError::NotARepository)?
        .canonicalize()?;
    let git = Repository::open(&checkout)?;
    let head_ref = match target {
        Some(branch) => {
            if !local_branch_exists(&git, branch) {
                return Err(TuicrError::Forge(format!(
                    "`{branch}` is neither a supported pull request target nor a local branch"
                )));
            }
            branch.to_string()
        }
        None => current_branch(&git)?,
    };

    let repository = local_repository(&checkout)?;
    let store = LocalForgeStore::new(&repository)?;
    let existing = store.find_pull_by_head(&head_ref)?;
    let base_ref = match base_override {
        Some(base) => {
            resolve_ref_oid(&git, base).map_err(|_| {
                TuicrError::Forge(format!("Local base ref `{base}` does not exist"))
            })?;
            base.to_string()
        }
        None => existing
            .as_ref()
            .map(|pull| pull.base_ref.clone())
            .unwrap_or(resolve_default_base(&git)?),
    };
    if head_ref == base_ref {
        return Err(nothing_to_review(&base_ref));
    }
    let head_sha = branch_tip(&git, &head_ref)?.to_string();
    let pull = store.open_pull(&head_ref, &base_ref, &head_sha, base_override.is_some())?;

    Ok(ResolvedLocalTarget {
        target: PullRequestTarget::with_repository(
            repository.clone(),
            pull.number,
            target.unwrap_or(&head_ref),
        ),
        repository,
        checkout,
    })
}

pub(crate) fn local_repository(checkout: &Path) -> Result<ForgeRepository> {
    let (owner, name) = slug::resolve_owner_repo(checkout).map_err(|error| {
        TuicrError::Forge(format!("Could not identify local repository: {error}"))
    })?;
    Ok(ForgeRepository::local(
        owner.unwrap_or_else(|| "local".to_string()),
        name,
    ))
}

pub(crate) fn resolve_default_base(repository: &Repository) -> Result<String> {
    if let Ok(origin_head) = repository.find_reference("refs/remotes/origin/HEAD")
        && let Some(target) = origin_head.symbolic_target()
        && let Some(remote_name) = target.strip_prefix("refs/remotes/origin/")
    {
        if local_branch_exists(repository, remote_name) {
            return Ok(remote_name.to_string());
        }
        return Ok(format!("origin/{remote_name}"));
    }
    for candidate in ["develop", "main", "master"] {
        if local_branch_exists(repository, candidate) {
            return Ok(candidate.to_string());
        }
    }
    Err(TuicrError::Forge(
        "Could not determine a local pull request base: pass --base <ref>, configure origin/HEAD, or create develop, main, or master"
            .to_string(),
    ))
}

pub(crate) fn branch_tip(repository: &Repository, branch: &str) -> Result<Oid> {
    Ok(repository
        .find_reference(&format!("refs/heads/{branch}"))?
        .peel_to_commit()?
        .id())
}

pub(crate) fn resolve_ref_oid(repository: &Repository, reference: &str) -> Result<Oid> {
    Ok(repository
        .revparse_single(reference)?
        .peel_to_commit()?
        .id())
}

pub(crate) fn local_branch_exists(repository: &Repository, branch: &str) -> bool {
    repository
        .find_reference(&format!("refs/heads/{branch}"))
        .is_ok()
}

pub(crate) fn pull_is_closed(repository: &Repository, pull: &LocalPull) -> bool {
    !local_branch_exists(repository, &pull.head_ref)
}

fn current_branch(repository: &Repository) -> Result<String> {
    let head = repository.head().map_err(|_| {
        TuicrError::Forge("Detached HEAD has no local branch to review".to_string())
    })?;
    if !head.is_branch() {
        return Err(TuicrError::Forge(
            "Detached HEAD has no local branch to review".to_string(),
        ));
    }
    head.shorthand()
        .map(str::to_string)
        .ok_or_else(|| TuicrError::Forge("Detached HEAD has no local branch to review".to_string()))
}

fn nothing_to_review(base_ref: &str) -> TuicrError {
    TuicrError::Forge(format!(
        "nothing to review on `{base_ref}`; run `tuicr pr <branch>`"
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use git2::{IndexAddOption, Signature};

    use super::*;
    use crate::forge::traits::ForgeBackend;

    struct ReviewsDirGuard;

    impl Drop for ReviewsDirGuard {
        fn drop(&mut self) {
            crate::persistence::storage::set_test_reviews_dir(None);
        }
    }

    fn repository_with_branches() -> (tempfile::TempDir, Repository, Oid, Oid, ReviewsDirGuard) {
        let temp = tempfile::tempdir().unwrap();
        crate::persistence::storage::set_test_reviews_dir(Some(temp.path().join("data/reviews")));
        let checkout = temp.path().join("repo");
        fs::create_dir_all(&checkout).unwrap();
        let repository = Repository::init(&checkout).unwrap();
        repository
            .remote("origin", "https://github.com/owner/project.git")
            .unwrap();
        let base = commit(&repository, "develop", "base\n", "base");
        repository
            .reference("refs/heads/main", base, true, "test")
            .unwrap();
        repository
            .reference("refs/heads/feature", base, true, "test")
            .unwrap();
        commit(&repository, "feature", "feature one\n", "feature one");
        let feature = commit(&repository, "feature", "feature two\n", "feature two");
        repository.set_head("refs/heads/feature").unwrap();
        repository
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        (temp, repository, base, feature, ReviewsDirGuard)
    }

    fn commit(repository: &Repository, branch: &str, content: &str, message: &str) -> Oid {
        fs::write(repository.workdir().unwrap().join("file.txt"), content).unwrap();
        let mut index = repository.index().unwrap();
        index
            .add_all(["file.txt"], IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = Signature::now("Test User", "test@example.com").unwrap();
        let reference = format!("refs/heads/{branch}");
        let parents = repository
            .find_reference(&reference)
            .ok()
            .and_then(|reference| reference.target())
            .map(|oid| repository.find_commit(oid).unwrap())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        repository
            .commit(
                Some(&reference),
                &signature,
                &signature,
                message,
                &tree,
                &parent_refs,
            )
            .unwrap()
    }

    #[test]
    fn should_require_one_of_the_documented_base_sources() {
        let temp = tempfile::tempdir().unwrap();
        let repository = Repository::init(temp.path()).unwrap();

        let error = resolve_default_base(&repository).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("--base"));
        assert!(message.contains("origin/HEAD"));
        assert!(message.contains("develop, main, or master"));
    }

    #[test]
    fn should_resolve_current_branch_when_target_is_absent() {
        let (temp, _git, _base, feature, _guard) = repository_with_branches();

        let resolved = resolve_local_target(&temp.path().join("repo"), None, None).unwrap();
        let store = LocalForgeStore::new(&resolved.repository).unwrap();
        let backend = crate::forge::local::LocalForgeBackend::new(
            resolved.repository.clone(),
            resolved.checkout.clone(),
        )
        .unwrap();
        let details = backend.get_pull_request(resolved.target.clone()).unwrap();

        assert_eq!(
            resolved.repository,
            ForgeRepository::local("owner", "project")
        );
        assert_eq!(resolved.target.number, 1);
        assert_eq!(
            store.root(),
            temp.path().join("data/local-forge/owner__project")
        );
        assert_eq!(store.pull(1).unwrap().last_head_sha, feature.to_string());
        assert_eq!(
            backend.list_pull_request_commits(&details).unwrap().len(),
            2
        );
    }

    #[test]
    fn should_resolve_named_branch_and_base_override() {
        let (temp, _git, _base, _feature, _guard) = repository_with_branches();

        let resolved =
            resolve_local_target(&temp.path().join("repo"), Some("feature"), Some("main")).unwrap();
        let pull = LocalForgeStore::new(&resolved.repository)
            .unwrap()
            .pull(resolved.target.number)
            .unwrap();

        assert_eq!(pull.base_ref, "main");
    }

    #[test]
    fn should_reject_detached_head_and_base_branch() {
        let (temp, repository, base, feature, _guard) = repository_with_branches();
        repository.set_head_detached(feature).unwrap();
        let detached = resolve_local_target(&temp.path().join("repo"), None, None).unwrap_err();
        repository.set_head("refs/heads/develop").unwrap();
        repository
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let on_base = resolve_local_target(&temp.path().join("repo"), None, None).unwrap_err();

        assert!(detached.to_string().contains("Detached HEAD"));
        assert_eq!(
            on_base.to_string(),
            "nothing to review on `develop`; run `tuicr pr <branch>`"
        );
        assert_eq!(branch_tip(&repository, "develop").unwrap(), base);
    }

    #[test]
    fn should_report_both_interpretations_for_unknown_target() {
        let (temp, _git, _base, _feature, _guard) = repository_with_branches();

        let error =
            resolve_local_target(&temp.path().join("repo"), Some("missing"), None).unwrap_err();

        assert!(error.to_string().contains("pull request target"));
        assert!(error.to_string().contains("local branch"));
    }
}
