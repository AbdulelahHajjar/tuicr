use super::*;

pub(super) struct PrStartupOptions {
    pub(super) theme: Theme,
    pub(super) comment_type_configs: Option<Vec<CommentTypeConfig>>,
    pub(super) output_to_stdout: bool,
    pub(super) repo_url_override: Option<ForgeRepository>,
    pub(super) commit_selection: CommitSelectionStart,
    pub(super) display: super::init::PrDisplayOptions,
    pub(super) local_repo_root: Option<PathBuf>,
}

pub(super) fn parse_forge_pr_target(
    target: &str,
) -> Option<crate::forge::traits::PullRequestTarget> {
    use crate::forge::azure::az::parse_pull_request_target_azure;
    use crate::forge::bitbucket::bkt::parse_pull_request_target_bitbucket;
    use crate::forge::gerrit::api::parse_pull_request_target_gerrit;
    use crate::forge::gitea::tea::parse_pull_request_target_gitea;
    use crate::forge::github::gh::parse_pull_request_target;
    use crate::forge::gitlab::glab::parse_pull_request_target_gitlab;

    // Gitea must recognize host-qualified shorthand before GitHub's catch-all.
    parse_pull_request_target_bitbucket(target)
        .or_else(|_| parse_pull_request_target_gitea(target))
        .or_else(|_| parse_pull_request_target(target))
        .or_else(|_| parse_pull_request_target_gitlab(target))
        .or_else(|_| parse_pull_request_target_azure(target))
        .or_else(|_| parse_pull_request_target_gerrit(target))
        .ok()
}

pub(super) fn validate_pr_base_target(is_forge_target: bool, base: Option<&str>) -> Result<()> {
    if is_forge_target && base.is_some() {
        return Err(TuicrError::Forge(
            "--base cannot be used with a forge pull request target".to_string(),
        ));
    }
    Ok(())
}

impl App {
    pub(super) fn new_from_local_pr_target(
        startup: PrStartupOptions,
        invocation: &crate::cli::PrInvocation,
    ) -> Result<Self> {
        let checkout = startup
            .local_repo_root
            .as_deref()
            .ok_or(TuicrError::NotARepository)?;
        let resolved = crate::forge::local::target::resolve_local_target(
            checkout,
            invocation.target.as_deref(),
            invocation.base.as_deref(),
        )?;
        Self::new_from_resolved_pr_target(
            startup,
            resolved.target,
            resolved.repository,
            Some(resolved.checkout),
        )
    }

    pub(super) fn new_from_resolved_pr_target(
        startup: PrStartupOptions,
        parsed: crate::forge::traits::PullRequestTarget,
        target_repo: ForgeRepository,
        local_checkout_for_target: Option<PathBuf>,
    ) -> Result<Self> {
        use crate::forge::pr_open::open_pull_request;

        let backend = create_forge_backend(
            &target_repo,
            local_checkout_for_target.clone(),
            startup.display.show_checks,
            startup.display.show_comments,
        );
        let highlighter = startup.theme.syntax_highlighter();
        let opened = open_pull_request(
            backend.as_ref(),
            parsed,
            local_checkout_for_target.as_deref(),
            highlighter,
        )?;
        let opened = Self::opened_pr_with_persisted_session(opened)?;

        let pr_source = PullRequestDiffSource::from_details(&opened.details);
        let diff_source = DiffSource::PullRequest(Box::new(pr_source));
        let vcs_info = VcsInfo {
            root_path: opened.session.repo_path.clone(),
            head_commit: opened.details.head_sha.clone(),
            branch_name: Some(opened.details.head_ref_name.clone()),
            vcs_type: VcsType::File,
        };
        let vcs: Box<dyn VcsBackend> = Box::new(PrNoopVcs::new(vcs_info.clone()));

        let details_for_threads = opened.details.clone();
        let commits_for_selector = opened.commits.clone();
        let review_metadata = opened.review_metadata.clone();
        let mut app = Self::build(
            vcs,
            vcs_info,
            startup.theme,
            startup.comment_type_configs,
            startup.output_to_stdout,
            opened.diff_files,
            opened.session,
            diff_source,
            InputMode::Normal,
            Vec::new(),
            None,
            startup.repo_url_override,
        )?;
        app.show_pr_checks = startup.display.show_checks;
        app.show_pr_comments = startup.display.show_comments;
        app.local_repo_root = startup.local_repo_root;
        app.forge_backend = Some(backend);
        app.forge_repository = Some(target_repo);
        app.pr_info = Some(opened.pr_info);
        app.canonical_resolved = true;
        app.current_pr_head = Some(details_for_threads.head_sha.clone());
        app.commit_selection_start = startup.commit_selection;
        let since_last_review_message =
            app.apply_pr_commit_selector(commits_for_selector, review_metadata);
        if matches!(&app.diff_source, DiffSource::PullRequest(_))
            && let Some(range) = app.commit_selection_range
            && !app.pr_commits.is_empty()
            && (range.0 > 0 || range.1 + 1 < app.pr_commits.len())
        {
            app.spawn_pr_range_reload();
        }
        if let DiffSource::PullRequest(pr) = &app.diff_source.clone()
            && pr.is_read_only()
        {
            let reason = pr.read_only_reason().unwrap_or("read only");
            app.set_warning(format!("This PR is {reason} — review is read-only"));
        } else if let Some(message) = since_last_review_message {
            app.set_message(message);
        }
        app.spawn_pr_threads_fetch(&details_for_threads, local_checkout_for_target);
        Ok(app)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_forge_pr_target, validate_pr_base_target};
    use crate::forge::traits::ForgeRepository;

    #[test]
    fn should_classify_numeric_target_as_existing_forge_pr() {
        let target = parse_forge_pr_target("125").unwrap();

        assert_eq!(target.number, 125);
        assert!(target.repository.is_none());
    }

    #[test]
    fn should_leave_branch_name_for_local_target_resolution() {
        for branch in [
            "main",
            "feature/local-forge",
            "feature/gitea",
            "feature/gerrit",
        ] {
            assert!(parse_forge_pr_target(branch).is_none(), "{branch}");
        }
    }

    #[test]
    fn should_classify_gitea_targets_before_github_shorthand() {
        for input in [
            "https://gitea.example.com/team/service/pulls/42",
            "gitea.example.com/team/service#42",
        ] {
            let target = parse_forge_pr_target(input).expect("Gitea pull request");

            assert_eq!(target.number, 42);
            assert_eq!(
                target.repository,
                Some(ForgeRepository::gitea(
                    "gitea.example.com",
                    "team",
                    "service"
                )),
                "{input}"
            );
        }
    }

    #[test]
    fn should_classify_gerrit_urls_as_remote_targets() {
        let target = parse_forge_pr_target("https://gerrit.example.com/c/platform/base/+/3965/2")
            .expect("Gerrit change");

        assert_eq!(target.number, 3965);
        assert_eq!(
            target.repository,
            Some(ForgeRepository::gerrit(
                "gerrit.example.com",
                "platform/base"
            ))
        );

        let legacy = parse_forge_pr_target("https://gerrit.example.com/#/c/3965/")
            .expect("legacy Gerrit change");
        assert_eq!(legacy.number, 3965);
        assert!(legacy.repository.is_none());
    }

    #[test]
    fn should_keep_bare_repository_shorthand_on_github() {
        let target = parse_forge_pr_target("team/service#42").expect("GitHub pull request");

        assert_eq!(target.number, 42);
        assert_eq!(
            target.repository,
            Some(ForgeRepository::github("github.com", "team", "service"))
        );
    }

    #[test]
    fn should_reject_base_with_forge_target() {
        let error = validate_pr_base_target(true, Some("main")).unwrap_err();

        assert_eq!(
            error.to_string(),
            "--base cannot be used with a forge pull request target"
        );
    }
}
