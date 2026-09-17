//! Non-interactive review session commands.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::{LineSideArg, ReviewCommand};
use crate::config;
use crate::error::{Result, TuicrError};
use crate::forge::local::LocalForgeBackend;
use crate::forge::local::store::{LocalForgeStore, LocalThread};
use crate::forge::local::target::local_repository;
use crate::forge::submit::{GhSide, SubmitContext};
use crate::forge::traits::{
    CreateThreadRequest, ForgeBackend, ForgeKind, ForgeRepository, PullRequestTarget,
};
use crate::model::comment::{self, CommentLifecycleState};
use crate::model::{Comment, CommentType, LineRange, LineSide, ReviewSession};
use crate::review_store::{
    AddCommentRequest, CommentTarget, ReviewStore, SessionRef, SessionSummary,
};
use crate::slug::Slug;

pub fn run(command: ReviewCommand) -> Result<()> {
    let mut stdout = io::stdout();
    run_with_writer(command, &mut stdout)
}

fn run_with_writer(command: ReviewCommand, out: &mut impl Write) -> Result<()> {
    match command {
        ReviewCommand::List { repo, all } => list_sessions(&repo, all, out),
        ReviewCommand::Add {
            session,
            input,
            repo,
            comment_type,
            file,
            line,
            end_line,
            side,
            username,
            content,
        } => add_comment(
            &session,
            &repo,
            AddCommentOptions {
                input,
                comment_type,
                file,
                line,
                end_line,
                side,
                username,
                content,
            },
            out,
        ),
        ReviewCommand::Comments { session, repo } => show_comments(&session, &repo, out),
        ReviewCommand::Threads { session } => list_threads(&session, out),
        ReviewCommand::Reply {
            session,
            thread,
            username,
            content,
        } => reply_to_forge_thread(&session, &thread, username, &content, out),
        ReviewCommand::Resolve {
            session,
            thread,
            unresolve,
        } => set_thread_resolution(&session, &thread, !unresolve, out),
        ReviewCommand::Edit {
            session,
            thread,
            comment,
            username,
            content,
        } => edit_forge_thread_comment(
            &session,
            &thread,
            comment.as_deref(),
            username,
            &content,
            out,
        ),
        ReviewCommand::Delete {
            session,
            thread,
            comment,
            username,
        } => delete_forge_thread_comment(&session, &thread, comment.as_deref(), username, out),
    }
}

fn list_sessions(repo: &Path, all: bool, out: &mut impl Write) -> Result<()> {
    let store = ReviewStore::new();
    let summaries = if all {
        store.list_all_sessions()?
    } else {
        store.list_sessions_for_repo(repo)?
    };
    let output: Vec<_> = summaries
        .into_iter()
        .map(SessionSummaryOutput::from)
        .collect();
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

struct AddCommentOptions {
    input: Option<String>,
    comment_type: String,
    file: Option<PathBuf>,
    line: Option<u32>,
    end_line: Option<u32>,
    side: LineSideArg,
    username: Option<String>,
    content: Option<String>,
}

fn add_comment(
    session: &str,
    repo: &Path,
    options: AddCommentOptions,
    out: &mut impl Write,
) -> Result<()> {
    let request_parts = build_add_request_parts(options)?;
    let config = config::load_config()
        .ok()
        .and_then(|outcome| outcome.config);
    add_comment_with_config(session, repo, request_parts, config.as_ref(), out)
}

fn add_comment_with_config(
    session: &str,
    repo: &Path,
    request_parts: AddRequestParts,
    config: Option<&config::AppConfig>,
    out: &mut impl Write,
) -> Result<()> {
    let comment_type = CommentType::from_id(&request_parts.comment_type);
    if let Some(warning) = unknown_comment_type_warning(&comment_type, config) {
        eprintln!("{warning}");
    }
    let author = resolve_cli_author(request_parts.username, config);
    if let Some((repository, number)) = parse_local_pr_slug(session)
        && let Some((path, line, side)) = thread_anchor(&request_parts.target)
    {
        let forge_config = config
            .and_then(|config| config.forge.clone())
            .unwrap_or_default();
        let comment_types = crate::app::App::resolve_comment_types(
            config.and_then(|config| config.comment_types.clone()),
        );
        let thread = add_local_thread(
            session,
            repo,
            repository,
            number,
            LocalThreadInput {
                start_line: match &request_parts.target {
                    CommentTarget::LineRange { range, .. } => Some(range.start),
                    _ => None,
                },
                path,
                line,
                side,
                comment_type: &comment_type,
                content: &request_parts.content,
                author: &author,
            },
            SubmitContext::new(&forge_config, &comment_types),
        )?;
        serde_json::to_writer_pretty(&mut *out, &thread)?;
        writeln!(out)?;
        return Ok(());
    }
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let target = request_parts.target;
    let comment = store.add_comment(
        &session_ref,
        AddCommentRequest {
            target: target.clone(),
            content: request_parts.content,
            comment_type,
            author,
            commit_id: None,
        },
    )?;
    let output = CommentOutput::from_target(&target, &comment);
    serde_json::to_writer_pretty(&mut *out, &output)?;
    writeln!(out)?;
    Ok(())
}

struct AddRequestParts {
    target: CommentTarget,
    comment_type: String,
    content: String,
    username: Option<String>,
}

/// Resolve the author for a CLI-authored comment.
///
/// Priority: explicit `--username` / JSON `username` ► config `username` ►
/// `Comment::DEFAULT_AUTHOR`. Trims whitespace so `--username " "` doesn't
/// produce an awkward all-whitespace badge.
fn resolve_cli_author(explicit: Option<String>, config: Option<&config::AppConfig>) -> String {
    if let Some(name) = explicit.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    if let Some(name) = config
        .and_then(|cfg| cfg.username.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return name.to_string();
    }
    comment::DEFAULT_AUTHOR.to_string()
}

/// Warn when `--type` names a type that no `comment_types` entry defines.
///
/// `CommentType::from_id` turns any non-empty id into `Custom`, so a typo is
/// indistinguishable from a configured type: it is stored, exported as
/// `**[TYPO]**`, and shown with no badge in the TUI. Warning rather than
/// rejecting keeps the CLI usable when `comment_types` is unset — the default,
/// under which every id but `none` is technically undefined — and keeps
/// scripted callers from breaking on a config change they don't control.
fn unknown_comment_type_warning(
    comment_type: &CommentType,
    config: Option<&config::AppConfig>,
) -> Option<String> {
    if comment_type.is_none() {
        return None;
    }
    let configured = config.and_then(|cfg| cfg.comment_types.as_deref())?;
    if configured
        .iter()
        .any(|definition| definition.id == comment_type.id())
    {
        return None;
    }
    let known: Vec<&str> = configured
        .iter()
        .map(|definition| definition.id.as_str())
        .collect();
    Some(format!(
        "Warning: comment type '{}' is not configured; known types: {}",
        comment_type.id(),
        known.join(", ")
    ))
}

fn build_add_request_parts(options: AddCommentOptions) -> Result<AddRequestParts> {
    let mut comment_type = options.comment_type;
    let mut content = options.content;
    let mut file = options.file;
    let mut line = options.line;
    let mut end_line = options.end_line;
    let mut side = options.side;
    let mut username = options.username;
    let mut target = None;

    if let Some(input) = options.input {
        let payload = parse_add_payload(&read_json_input(&input)?)?;
        if let Some(payload_comment_type) = payload.comment_type {
            comment_type = payload_comment_type;
        }
        if payload.content.is_some() {
            content = payload.content;
        }
        if payload.username.is_some() {
            username = payload.username;
        }
        if let Some(payload_target) = payload.target {
            target = Some(payload_target.into_comment_target()?);
        } else {
            if let Some(payload_file) = payload.file {
                file = Some(payload_file);
            }
            if payload.line.is_some() || payload.start_line.is_some() {
                line = payload.line.or(payload.start_line);
            }
            if let Some(payload_end_line) = payload.end_line {
                end_line = Some(payload_end_line);
            }
            if let Some(payload_side) = payload.side {
                side = parse_line_side(&payload_side)?;
            }
        }
    }

    let content = content.ok_or_else(|| {
        TuicrError::InvalidInput(
            "comment text is required either as COMMENT or JSON field `content`".to_string(),
        )
    })?;
    let target = match target {
        Some(target) => target,
        None => build_comment_target(file, line, end_line, side)?,
    };

    Ok(AddRequestParts {
        target,
        comment_type,
        content,
        username,
    })
}

fn read_json_input(input: &str) -> Result<String> {
    if input == "-" {
        let mut contents = String::new();
        io::stdin().read_to_string(&mut contents)?;
        return Ok(contents);
    }
    if let Some(path) = input.strip_prefix('@') {
        return fs::read_to_string(path).map_err(TuicrError::Io);
    }
    Ok(input.to_string())
}

fn parse_add_payload(input: &str) -> Result<AddCommentPayload> {
    serde_json::from_str(input)
        .map_err(|err| TuicrError::InvalidInput(format!("invalid JSON review payload: {err}")))
}

#[derive(Debug, Deserialize)]
struct AddCommentPayload {
    #[serde(default, alias = "type")]
    comment_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    target: Option<JsonCommentTarget>,
    #[serde(default)]
    file: Option<PathBuf>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default, alias = "author")]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JsonCommentTarget {
    #[serde(default, rename = "type", alias = "kind")]
    target_type: Option<String>,
    #[serde(default)]
    file: Option<PathBuf>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
}

impl JsonCommentTarget {
    fn into_comment_target(self) -> Result<CommentTarget> {
        let side = match self.side {
            Some(side) => parse_line_side(&side)?,
            None => LineSideArg::New,
        };
        let inferred_type = if self.file.is_none() {
            "review"
        } else if self.line.is_some() || self.start_line.is_some() {
            if self.end_line.is_some() {
                "line_range"
            } else {
                "line"
            }
        } else {
            "file"
        };
        let target_type = self
            .target_type
            .unwrap_or_else(|| inferred_type.to_string())
            .replace('-', "_")
            .to_ascii_lowercase();

        match target_type.as_str() {
            "review" => Ok(CommentTarget::Review),
            "file" => Ok(CommentTarget::File {
                path: required_file(self.file, "target.file")?,
            }),
            "line" => Ok(CommentTarget::Line {
                path: required_file(self.file, "target.file")?,
                line: required_line(self.line.or(self.start_line), "target.line")?,
                side: line_side_arg_to_model(side),
            }),
            "line_range" | "range" => Ok(CommentTarget::LineRange {
                path: required_file(self.file, "target.file")?,
                range: LineRange::new(
                    required_line(self.line.or(self.start_line), "target.start_line")?,
                    required_line(self.end_line, "target.end_line")?,
                ),
                side: line_side_arg_to_model(side),
            }),
            other => Err(TuicrError::InvalidInput(format!(
                "unknown JSON target type '{other}'"
            ))),
        }
    }
}

fn required_file(path: Option<PathBuf>, name: &str) -> Result<PathBuf> {
    path.ok_or_else(|| TuicrError::InvalidInput(format!("{name} is required")))
}

fn required_line(line: Option<u32>, name: &str) -> Result<u32> {
    let line = line.ok_or_else(|| TuicrError::InvalidInput(format!("{name} is required")))?;
    validate_line(line, name)?;
    Ok(line)
}

fn parse_line_side(side: &str) -> Result<LineSideArg> {
    match side.to_ascii_lowercase().as_str() {
        "old" => Ok(LineSideArg::Old),
        "new" => Ok(LineSideArg::New),
        other => Err(TuicrError::InvalidInput(format!(
            "unknown side '{other}', expected 'old' or 'new'"
        ))),
    }
}

fn line_side_arg_to_model(side: LineSideArg) -> LineSide {
    match side {
        LineSideArg::Old => LineSide::Old,
        LineSideArg::New => LineSide::New,
    }
}

fn show_comments(session: &str, repo: &Path, out: &mut impl Write) -> Result<()> {
    let store = ReviewStore::new();
    let session_ref = resolve_session_ref(&store, repo, session)?;
    let session = store.get_review(&session_ref)?;
    let comments = collect_comments(&session);
    serde_json::to_writer_pretty(&mut *out, &comments)?;
    writeln!(out)?;
    Ok(())
}

/// The forge repository and pull number behind a `local:` PR slug, or
/// `None` for any other session argument.
fn parse_local_pr_slug(session: &str) -> Option<(ForgeRepository, u64)> {
    match session.parse::<Slug>() {
        Ok(Slug::Pr(pr)) if pr.forge == ForgeKind::Local => {
            Some((ForgeRepository::local(pr.owner, pr.repo), pr.number))
        }
        _ => None,
    }
}

/// Resolve a `local:` PR slug into the forge repository and PR number that
/// back its thread store. Forge threads only exist for local pull requests
/// today; other slugs are rejected with a pointer at the supported shape.
fn local_pr_target(session: &str) -> Result<(ForgeRepository, u64)> {
    let slug: Slug = session
        .parse()
        .map_err(|_| TuicrError::InvalidInput(format!("invalid session slug '{session}'")))?;
    let Slug::Pr(pr) = slug else {
        return Err(TuicrError::InvalidInput(
            "forge threads need a PR session slug like `local:owner/repo/pr/1`".to_string(),
        ));
    };
    if pr.forge != ForgeKind::Local {
        return Err(TuicrError::InvalidInput(
            "forge threads are only supported for `local:` pull requests".to_string(),
        ));
    }
    let repository = ForgeRepository::local(pr.owner, pr.repo);
    Ok((repository, pr.number))
}

/// The line a comment target anchors a thread to; `None` for review- and
/// file-level targets, which stay session drafts.
fn thread_anchor(target: &CommentTarget) -> Option<(PathBuf, u32, LineSide)> {
    match target {
        CommentTarget::Line { path, line, side } => Some((path.clone(), *line, *side)),
        CommentTarget::LineRange { path, range, side } => Some((path.clone(), range.end, *side)),
        CommentTarget::Review | CommentTarget::File { .. } => None,
    }
}

struct LocalThreadInput<'a> {
    start_line: Option<u32>,
    path: PathBuf,
    line: u32,
    side: LineSide,
    comment_type: &'a CommentType,
    content: &'a str,
    author: &'a str,
}

/// Open a thread on a local pull request straight in the forge store. The
/// checkout in `repo` supplies the diff the anchor is snapshotted against,
/// so it must be the repository the slug names.
fn add_local_thread(
    session: &str,
    repo: &Path,
    repository: ForgeRepository,
    number: u64,
    input: LocalThreadInput<'_>,
    ctx: SubmitContext<'_>,
) -> Result<LocalThread> {
    if !repo.is_dir() {
        return Err(TuicrError::InvalidInput(format!(
            "creating a thread on a local pull request needs the checkout path in --repo (got '{}')",
            repo.display()
        )));
    }
    let checkout_repository = local_repository(repo)?;
    if checkout_repository != repository {
        return Err(TuicrError::InvalidInput(format!(
            "'{session}' belongs to {}, but --repo {} is a checkout of {}; run from that repository or pass its path in --repo",
            repository.display_name(),
            repo.display(),
            checkout_repository.display_name()
        )));
    }
    let checkout = repo.canonicalize()?;
    let backend = LocalForgeBackend::new(repository.clone(), Some(checkout));
    let details = backend.get_pull_request(PullRequestTarget::with_repository(
        repository,
        number,
        number.to_string(),
    ))?;
    let body = format!(
        "{}{}",
        ctx.comment_type_prefix(input.comment_type),
        input.content
    );
    backend.create_local_thread(
        &details,
        &CreateThreadRequest {
            start_line: input.start_line,
            path: &input.path,
            line: input.line,
            side: GhSide::from(input.side),
            body: &body,
            author: Some(input.author),
            commit_id: &details.head_sha,
            diff_start_sha: None,
        },
    )
}

fn list_threads(session: &str, out: &mut impl Write) -> Result<()> {
    let (repository, number) = local_pr_target(session)?;
    let store = LocalForgeStore::new(&repository)?;
    let threads = store.threads(number)?;
    serde_json::to_writer_pretty(&mut *out, &threads)?;
    writeln!(out)?;
    Ok(())
}

fn reply_to_forge_thread(
    session: &str,
    thread: &str,
    username: Option<String>,
    content: &str,
    out: &mut impl Write,
) -> Result<()> {
    let (repository, number) = local_pr_target(session)?;
    let store = LocalForgeStore::new(&repository)?;
    let config = config::load_config()
        .ok()
        .and_then(|outcome| outcome.config);
    let author = resolve_cli_author(username, config.as_ref());
    let comment = store.reply_to_thread(number, thread, &author, content)?;
    serde_json::to_writer_pretty(&mut *out, &comment)?;
    writeln!(out)?;
    Ok(())
}

fn edit_forge_thread_comment(
    session: &str,
    thread: &str,
    comment: Option<&str>,
    username: Option<String>,
    content: &str,
    out: &mut impl Write,
) -> Result<()> {
    let (repository, number) = local_pr_target(session)?;
    let store = LocalForgeStore::new(&repository)?;
    let config = config::load_config()
        .ok()
        .and_then(|outcome| outcome.config);
    let author = resolve_cli_author(username, config.as_ref());
    let updated = store.update_thread_comment(number, thread, comment, &author, content)?;
    serde_json::to_writer_pretty(&mut *out, &updated)?;
    writeln!(out)?;
    Ok(())
}

fn delete_forge_thread_comment(
    session: &str,
    thread: &str,
    comment: Option<&str>,
    username: Option<String>,
    out: &mut impl Write,
) -> Result<()> {
    let (repository, number) = local_pr_target(session)?;
    let store = LocalForgeStore::new(&repository)?;
    let config = config::load_config()
        .ok()
        .and_then(|outcome| outcome.config);
    let author = resolve_cli_author(username, config.as_ref());
    let (comment_id, thread_deleted) =
        store.delete_thread_comment(number, thread, comment, &author)?;
    serde_json::to_writer_pretty(
        &mut *out,
        &serde_json::json!({
            "thread_id": thread,
            "comment_id": comment_id,
            "thread_deleted": thread_deleted,
        }),
    )?;
    writeln!(out)?;
    Ok(())
}

fn set_thread_resolution(
    session: &str,
    thread: &str,
    resolved: bool,
    out: &mut impl Write,
) -> Result<()> {
    let (repository, number) = local_pr_target(session)?;
    let store = LocalForgeStore::new(&repository)?;
    store.resolve_thread(number, thread, resolved)?;
    serde_json::to_writer_pretty(
        &mut *out,
        &serde_json::json!({ "thread_id": thread, "is_resolved": resolved }),
    )?;
    writeln!(out)?;
    Ok(())
}

fn resolve_session_ref(store: &ReviewStore, repo: &Path, session: &str) -> Result<SessionRef> {
    let direct_path = PathBuf::from(session);
    if direct_path.exists() || direct_path.is_absolute() || session.ends_with(".json") {
        return Ok(SessionRef::from_path(direct_path));
    }

    // PR sessions are keyed by forge coordinates, not a local checkout, so they
    // resolve from the manifest by slug rather than the per-repo listing.
    if matches!(session.parse::<Slug>(), Ok(Slug::Pr(_))) {
        return match store.resolve_pr_session(session)? {
            Some(session_ref) => Ok(session_ref),
            None => Err(TuicrError::InvalidInput(format!(
                "no PR session found for '{session}'. Run `tuicr review list --all` to see available sessions."
            ))),
        };
    }

    let matches: Vec<_> = store
        .list_sessions_for_repo(repo)?
        .into_iter()
        .filter(|summary| summary.slug == session)
        .collect();
    match matches.as_slice() {
        [summary] => Ok(summary.session_ref.clone()),
        [] => Err(TuicrError::InvalidInput(format!(
            "session '{session}' was not found for repo {}. Run `tuicr review list --repo {}` to see available sessions.",
            repo.display(),
            repo.display()
        ))),
        _ => Err(TuicrError::InvalidInput(format!(
            "session '{session}' is ambiguous for repo {}",
            repo.display()
        ))),
    }
}

fn build_comment_target(
    file: Option<PathBuf>,
    line: Option<u32>,
    end_line: Option<u32>,
    side: LineSideArg,
) -> Result<CommentTarget> {
    let side = match side {
        LineSideArg::Old => LineSide::Old,
        LineSideArg::New => LineSide::New,
    };

    match (file, line, end_line) {
        (None, None, None) => Ok(CommentTarget::Review),
        (Some(path), None, None) => Ok(CommentTarget::File { path }),
        (Some(path), Some(line), None) => {
            validate_line(line, "--line")?;
            Ok(CommentTarget::Line { path, line, side })
        }
        (Some(path), Some(start), Some(end)) => {
            validate_line(start, "--line")?;
            validate_line(end, "--end-line")?;
            Ok(CommentTarget::LineRange {
                path,
                range: LineRange::new(start, end),
                side,
            })
        }
        (None, Some(_), _) => Err(TuicrError::InvalidInput(
            "--line requires --target-file for review comments".to_string(),
        )),
        (None, None, Some(_)) => Err(TuicrError::InvalidInput(
            "--end-line requires --line and --target-file".to_string(),
        )),
        (Some(_), None, Some(_)) => Err(TuicrError::InvalidInput(
            "--end-line requires --line".to_string(),
        )),
    }
}

fn validate_line(line: u32, name: &str) -> Result<()> {
    if line == 0 {
        return Err(TuicrError::InvalidInput(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(())
}

fn collect_comments(session: &ReviewSession) -> Vec<CommentOutput> {
    let mut comments = Vec::new();
    for comment in &session.review_comments {
        comments.push(CommentOutput::from_parts(
            "review".to_string(),
            None,
            None,
            None,
            None,
            comment,
        ));
    }

    let mut files: Vec<_> = session.files.iter().collect();
    files.sort_by_key(|(path, _)| path.as_os_str().to_os_string());
    for (path, review) in files {
        let path_display = path.to_string_lossy().to_string();
        for comment in &review.file_comments {
            comments.push(CommentOutput::from_parts(
                path_display.clone(),
                Some(path_display.clone()),
                None,
                None,
                None,
                comment,
            ));
        }

        let mut line_comments: Vec<_> = review.line_comments.iter().collect();
        line_comments.sort_by_key(|(line, _)| *line);
        for (line, line_comments) in line_comments {
            for comment in line_comments {
                let (start_line, end_line) = comment
                    .line_range
                    .map(|range| (range.start, range.end))
                    .unwrap_or((*line, *line));
                let location = line_location(&path_display, start_line, end_line, comment.side);
                comments.push(CommentOutput::from_parts(
                    location,
                    Some(path_display.clone()),
                    Some(start_line),
                    Some(end_line),
                    comment.side,
                    comment,
                ));
            }
        }
    }

    comments
}

fn line_location(path: &str, start_line: u32, end_line: u32, side: Option<LineSide>) -> String {
    let line = if start_line == end_line {
        start_line.to_string()
    } else {
        format!("{start_line}-{end_line}")
    };
    match side {
        Some(LineSide::Old) => format!("{path}:{line} [old]"),
        _ => format!("{path}:{line}"),
    }
}

fn target_location(target: &CommentTarget) -> String {
    match target {
        CommentTarget::Review => "review".to_string(),
        CommentTarget::File { path } => path.display().to_string(),
        CommentTarget::Line { path, line, side } => {
            line_location(&path.to_string_lossy(), *line, *line, Some(*side))
        }
        CommentTarget::LineRange { path, range, side } => {
            line_location(&path.to_string_lossy(), range.start, range.end, Some(*side))
        }
    }
}

fn side_id(side: Option<LineSide>) -> Option<&'static str> {
    match side {
        Some(LineSide::Old) => Some("old"),
        Some(LineSide::New) => Some("new"),
        None => None,
    }
}

fn lifecycle_id(state: CommentLifecycleState) -> &'static str {
    match state {
        CommentLifecycleState::LocalDraft => "local_draft",
        CommentLifecycleState::PushedDraft => "pushed_draft",
        CommentLifecycleState::Submitted => "submitted",
    }
}

#[derive(Debug, Serialize)]
struct SessionSummaryOutput {
    slug: String,
    kind: &'static str,
    path: String,
    updated_at: String,
    comment_count: usize,
    reviewed_count: usize,
    file_count: usize,
    anchor: String,
    active: bool,
}

impl From<SessionSummary> for SessionSummaryOutput {
    fn from(summary: SessionSummary) -> Self {
        Self {
            slug: summary.slug,
            kind: summary.kind.id(),
            path: summary.session_ref.path().display().to_string(),
            updated_at: summary.updated_at.to_rfc3339(),
            comment_count: summary.comment_count,
            reviewed_count: summary.reviewed_count,
            file_count: summary.file_count,
            anchor: summary.anchor,
            active: summary.active,
        }
    }
}

#[derive(Debug, Serialize)]
struct CommentOutput {
    id: String,
    location: String,
    path: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    side: Option<&'static str>,
    comment_type: String,
    author: String,
    lifecycle_state: &'static str,
    created_at: String,
    content: String,
}

impl CommentOutput {
    fn from_target(target: &CommentTarget, comment: &Comment) -> Self {
        let (path, start_line, end_line, side) = match target {
            CommentTarget::Review => (None, None, None, None),
            CommentTarget::File { path } => (Some(path.display().to_string()), None, None, None),
            CommentTarget::Line { path, line, side } => (
                Some(path.display().to_string()),
                Some(*line),
                Some(*line),
                Some(*side),
            ),
            CommentTarget::LineRange { path, range, side } => (
                Some(path.display().to_string()),
                Some(range.start),
                Some(range.end),
                Some(*side),
            ),
        };
        Self::from_parts(
            target_location(target),
            path,
            start_line,
            end_line,
            side,
            comment,
        )
    }

    fn from_parts(
        location: String,
        path: Option<String>,
        start_line: Option<u32>,
        end_line: Option<u32>,
        side: Option<LineSide>,
        comment: &Comment,
    ) -> Self {
        Self {
            id: comment.id.clone(),
            location,
            path,
            start_line,
            end_line,
            side: side_id(side),
            comment_type: comment.comment_type.id().to_string(),
            author: comment.author.clone(),
            lifecycle_state: lifecycle_id(comment.lifecycle_state),
            created_at: comment.created_at.to_rfc3339(),
            content: comment.content.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    use crate::model::{FileStatus, SessionDiffSource};

    use crate::forge::local::store::{LocalForgeStore, LocalThread};

    #[test]
    fn should_target_local_pr_slug_for_forge_threads() {
        let (repository, number) = local_pr_target("local:owner/name/pr/7").unwrap();
        assert_eq!(number, 7);
        assert_eq!(repository.owner, "owner");
        assert_eq!(repository.name, "name");
        assert_eq!(repository.kind, ForgeKind::Local);
    }

    #[test]
    fn should_reject_non_local_slugs_for_forge_threads() {
        assert!(local_pr_target("gh:owner/name/pr/7").is_err());
        assert!(local_pr_target("owner/name@main/worktree/abc1234").is_err());
    }

    #[test]
    fn should_reply_and_resolve_local_threads() {
        let dir = tempdir().unwrap();
        let store = LocalForgeStore::at(dir.path());
        let thread = LocalThread {
            original_start_line: None,
            start_line_text: None,
            id: "t1".to_string(),
            path: "src/a.rs".to_string(),
            side: "new".to_string(),
            original_line: 3,
            original_commit: "abc".to_string(),
            base_commit: "base".to_string(),
            line_text: "let x = 1;".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: None,
            is_resolved: false,
            resolved_at: None,
            review_id: None,
            comments: Vec::new(),
        };
        store
            .add_review(1, "COMMENT", "", "abc", "user", vec![thread])
            .unwrap();

        let comment = store
            .reply_to_thread(1, "t1", "Claude Fable", "Done in abc1234.")
            .unwrap();
        assert_eq!(comment.author, "Claude Fable");

        store.resolve_thread(1, "t1", true).unwrap();
        let threads = store.threads(1).unwrap();
        assert!(threads[0].is_resolved);
        assert_eq!(threads[0].comments.len(), 1);
        assert_eq!(threads[0].comments[0].body, "Done in abc1234.");
    }

    struct ReviewsDirGuard;

    impl Drop for ReviewsDirGuard {
        fn drop(&mut self) {
            crate::persistence::storage::set_test_reviews_dir(None);
        }
    }

    /// A checkout with `develop` and a one-commit `feature` branch, plus a
    /// data directory under the same temp dir where `feature` is pull #1.
    fn local_pull_fixture() -> (tempfile::TempDir, PathBuf, ReviewsDirGuard) {
        let temp = tempdir().unwrap();
        crate::persistence::storage::set_test_reviews_dir(Some(temp.path().join("data/reviews")));
        let checkout = temp.path().join("repo");
        init_checkout(&checkout, "https://github.com/owner/project.git");
        crate::forge::local::target::resolve_local_target(&checkout, Some("feature"), None)
            .unwrap();
        (temp, checkout, ReviewsDirGuard)
    }

    fn init_checkout(checkout: &Path, origin: &str) {
        fs::create_dir_all(checkout).unwrap();
        let repository = git2::Repository::init(checkout).unwrap();
        repository.remote("origin", origin).unwrap();
        let base = commit(&repository, "develop", "alpha\nbeta\n", "base");
        repository
            .reference("refs/heads/feature", base, true, "test")
            .unwrap();
        commit(&repository, "feature", "alpha\nbeta changed\n", "feature");
        repository.set_head("refs/heads/feature").unwrap();
        repository
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
    }

    fn commit(
        repository: &git2::Repository,
        branch: &str,
        content: &str,
        message: &str,
    ) -> git2::Oid {
        fs::write(repository.workdir().unwrap().join("file.txt"), content).unwrap();
        let mut index = repository.index().unwrap();
        index
            .add_all(["file.txt"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Test User", "test@example.com").unwrap();
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

    fn add_command(
        session: &str,
        repo: &Path,
        file: Option<&str>,
        line: Option<u32>,
        username: Option<&str>,
    ) -> ReviewCommand {
        ReviewCommand::Add {
            session: session.to_string(),
            input: None,
            repo: repo.to_path_buf(),
            comment_type: "issue".to_string(),
            file: file.map(PathBuf::from),
            line,
            end_line: None,
            side: LineSideArg::New,
            username: username.map(str::to_string),
            content: Some("direct".to_string()),
        }
    }

    fn local_threads(temp: &tempfile::TempDir) -> Vec<LocalThread> {
        LocalForgeStore::at(temp.path().join("data/local-forge/owner__project"))
            .threads(1)
            .unwrap()
    }

    #[test]
    fn should_create_local_thread_from_add_on_local_pr_slug() {
        let (temp, checkout, _guard) = local_pull_fixture();
        let mut out = Vec::new();

        run_with_writer(
            add_command(
                "local:owner/project/pr/1",
                &checkout,
                Some("file.txt"),
                Some(2),
                Some("Claude Fable"),
            ),
            &mut out,
        )
        .unwrap();

        let printed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed["comments"][0]["author"], "Claude Fable");
        let body = printed["comments"][0]["body"].as_str().unwrap();
        assert!(body.ends_with("direct"), "body: {body}");
        assert_eq!(printed["line_text"], "beta changed");
        assert_eq!(printed["side"], "RIGHT");
        assert_eq!(printed["original_line"], 2);
        assert!(printed["review_id"].is_null());
        let threads = local_threads(&temp);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, printed["id"].as_str().unwrap());
        let store = LocalForgeStore::at(temp.path().join("data/local-forge/owner__project"));
        assert!(store.reviews(1).unwrap().is_empty());
        let sessions = temp.path().join("data/reviews/sessions");
        assert!(!sessions.exists() || fs::read_dir(&sessions).unwrap().next().is_none());
    }

    #[test]
    fn should_apply_configured_labels_and_author_to_direct_local_threads() {
        let (temp, checkout, _guard) = local_pull_fixture();
        for (comment_type, prefix_enabled, username, expected_body, expected_author) in [
            (
                "issue",
                true,
                None,
                "[⚠ NEEDS WORK] direct",
                "Config Reviewer",
            ),
            ("issue", false, None, "direct", "Config Reviewer"),
            ("none", true, None, "direct", "Config Reviewer"),
            (
                "unlisted",
                true,
                None,
                "[UNLISTED] direct",
                "Config Reviewer",
            ),
            (
                "issue",
                true,
                Some(" Agent Reviewer "),
                "[⚠ NEEDS WORK] direct",
                "Agent Reviewer",
            ),
        ] {
            let config = config::AppConfig {
                username: Some(" Config Reviewer ".to_string()),
                forge: Some(config::ForgeConfig {
                    comment_type_prefix: prefix_enabled,
                }),
                comment_types: Some(vec![config::CommentTypeConfig {
                    id: "issue".to_string(),
                    label: Some("⚠ needs work".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            };
            let mut out = Vec::new();
            add_comment_with_config(
                "local:owner/project/pr/1",
                &checkout,
                AddRequestParts {
                    target: CommentTarget::Line {
                        path: PathBuf::from("file.txt"),
                        line: 2,
                        side: LineSide::New,
                    },
                    comment_type: comment_type.to_string(),
                    content: "direct".to_string(),
                    username: username.map(str::to_string),
                },
                Some(&config),
                &mut out,
            )
            .unwrap();

            let printed: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(printed["comments"][0]["body"], expected_body);
            assert_eq!(printed["comments"][0]["author"], expected_author);
            let threads = local_threads(&temp);
            let stored = threads
                .iter()
                .find(|thread| thread.id == printed["id"].as_str().unwrap())
                .unwrap();
            assert_eq!(stored.comments[0].body, expected_body);
            assert_eq!(stored.comments[0].author, expected_author);
        }
    }

    #[test]
    fn should_create_local_thread_from_json_input_target() {
        let (temp, checkout, _guard) = local_pull_fixture();
        let mut command = add_command(
            "local:owner/project/pr/1",
            &checkout,
            None,
            None,
            Some("Claude Fable"),
        );
        if let ReviewCommand::Add { input, content, .. } = &mut command {
            *input = Some(
                r#"{"file":"file.txt","start_line":1,"end_line":2,"side":"new","content":"ranged"}"#
                    .to_string(),
            );
            *content = None;
        }
        let mut out = Vec::new();

        run_with_writer(command, &mut out).unwrap();

        let printed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed["original_line"], 2);
        assert_eq!(printed["original_start_line"], 1);
        assert!(
            printed["comments"][0]["body"]
                .as_str()
                .unwrap()
                .ends_with("ranged")
        );
        assert_eq!(local_threads(&temp).len(), 1);
    }

    #[test]
    fn should_edit_and_delete_thread_comments_through_the_cli() {
        let (temp, checkout, _guard) = local_pull_fixture();
        let slug = "local:owner/project/pr/1";
        let mut out = Vec::new();
        run_with_writer(
            add_command(slug, &checkout, Some("file.txt"), Some(2), Some("user")),
            &mut out,
        )
        .unwrap();
        let thread_id = serde_json::from_slice::<serde_json::Value>(&out).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut out = Vec::new();
        run_with_writer(
            ReviewCommand::Reply {
                session: slug.to_string(),
                thread: thread_id.clone(),
                username: Some("Claude Fable".to_string()),
                content: "on it".to_string(),
            },
            &mut out,
        )
        .unwrap();
        let reply = serde_json::from_slice::<serde_json::Value>(&out).unwrap();
        assert_eq!(reply["author"], "Claude Fable");
        let reply_id = reply["id"].as_str().unwrap().to_string();
        let edit = |comment: Option<&str>, username: &str, body: &str| ReviewCommand::Edit {
            session: slug.to_string(),
            thread: thread_id.clone(),
            comment: comment.map(str::to_string),
            username: Some(username.to_string()),
            content: body.to_string(),
        };
        let delete = |comment: Option<&str>, username: &str| ReviewCommand::Delete {
            session: slug.to_string(),
            thread: thread_id.clone(),
            comment: comment.map(str::to_string),
            username: Some(username.to_string()),
        };

        // The agent amends its own reply; the user's root stays foreign to it.
        let mut out = Vec::new();
        run_with_writer(
            edit(Some(&reply_id), "Claude Fable", "done in abc1234"),
            &mut out,
        )
        .unwrap();
        let edited: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(edited["body"], "done in abc1234");
        assert_eq!(edited["author"], "Claude Fable");
        assert!(!edited["updated_at"].is_null());
        let foreign =
            run_with_writer(edit(None, "Claude Fable", "rewrite"), &mut Vec::new()).unwrap_err();
        assert!(
            foreign
                .to_string()
                .contains("only its author can change it")
        );

        // A root with a reply is refused; the reply goes first, then the root takes the thread.
        let root_first = run_with_writer(delete(None, "user"), &mut Vec::new()).unwrap_err();
        assert!(root_first.to_string().contains("has replies"));
        let mut out = Vec::new();
        run_with_writer(delete(Some(&reply_id), "Claude Fable"), &mut out).unwrap();
        let deleted: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(deleted["comment_id"], reply_id);
        assert_eq!(deleted["thread_deleted"], false);
        let mut out = Vec::new();
        run_with_writer(delete(None, "user"), &mut out).unwrap();
        let deleted: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(deleted["thread_deleted"], true);
        assert!(local_threads(&temp).is_empty());
    }

    #[test]
    fn should_reject_thread_creation_when_repo_is_not_a_checkout() {
        let (temp, _checkout, _guard) = local_pull_fixture();
        let mut out = Vec::new();

        let error = run_with_writer(
            add_command(
                "local:owner/project/pr/1",
                Path::new("owner/project"),
                Some("file.txt"),
                Some(2),
                None,
            ),
            &mut out,
        )
        .unwrap_err();

        assert!(matches!(error, TuicrError::InvalidInput(_)));
        assert!(
            error
                .to_string()
                .contains("needs the checkout path in --repo")
        );
        assert!(local_threads(&temp).is_empty());
        assert!(out.is_empty());
    }

    #[test]
    fn should_reject_thread_creation_when_checkout_does_not_match_slug() {
        let (temp, _checkout, _guard) = local_pull_fixture();
        let other = temp.path().join("other");
        init_checkout(&other, "https://github.com/other/thing.git");
        let mut out = Vec::new();

        let error = run_with_writer(
            add_command(
                "local:owner/project/pr/1",
                &other,
                Some("file.txt"),
                Some(2),
                None,
            ),
            &mut out,
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(matches!(error, TuicrError::InvalidInput(_)));
        assert!(message.contains("belongs to owner/project"), "{message}");
        assert!(message.contains("other/thing"), "{message}");
        assert!(local_threads(&temp).is_empty());
    }

    #[test]
    fn should_keep_drafting_review_level_add_on_local_pr_slug() {
        let (temp, checkout, _guard) = local_pull_fixture();
        let mut out = Vec::new();

        let error = run_with_writer(
            add_command("local:owner/project/pr/1", &checkout, None, None, None),
            &mut out,
        )
        .unwrap_err();

        assert!(error.to_string().contains("no PR session found"));
        assert!(local_threads(&temp).is_empty());
    }

    #[test]
    fn should_keep_drafting_line_add_on_github_slug() {
        let (temp, checkout, _guard) = local_pull_fixture();
        let mut out = Vec::new();

        let error = run_with_writer(
            add_command(
                "gh:owner/project/pr/1",
                &checkout,
                Some("file.txt"),
                Some(2),
                None,
            ),
            &mut out,
        )
        .unwrap_err();

        assert!(error.to_string().contains("no PR session found"));
        assert!(local_threads(&temp).is_empty());
    }

    fn test_session(repo_path: PathBuf) -> ReviewSession {
        let mut session = ReviewSession::new(
            repo_path,
            "abc1234".to_string(),
            Some("main".to_string()),
            SessionDiffSource::WorkingTree,
        );
        session.add_file(PathBuf::from("src/main.rs"), FileStatus::Modified, 0);
        session
    }

    #[test]
    fn should_build_review_comment_target_by_default() {
        let target = build_comment_target(None, None, None, LineSideArg::New).unwrap();
        assert!(matches!(target, CommentTarget::Review));
    }

    #[test]
    fn should_build_line_range_comment_target() {
        let target = build_comment_target(
            Some(PathBuf::from("src/main.rs")),
            Some(12),
            Some(10),
            LineSideArg::Old,
        )
        .unwrap();

        assert!(matches!(
            target,
            CommentTarget::LineRange {
                range: LineRange { start: 10, end: 12 },
                side: LineSide::Old,
                ..
            }
        ));
    }

    #[test]
    fn should_reject_zero_line() {
        let err = build_comment_target(
            Some(PathBuf::from("src/main.rs")),
            Some(0),
            None,
            LineSideArg::New,
        )
        .unwrap_err();
        assert!(matches!(err, TuicrError::InvalidInput(_)));
    }

    #[test]
    fn should_build_add_request_from_flat_json_payload() {
        let parts = build_add_request_parts(AddCommentOptions {
            input: Some(
                r#"{"file":"src/main.rs","line":42,"side":"old","type":"issue","content":"fix it"}"#
                    .to_string(),
            ),
            comment_type: "note".to_string(),
            file: None,
            line: None,
            end_line: None,
            side: LineSideArg::New,
            username: None,
            content: None,
        })
        .unwrap();

        assert_eq!(parts.comment_type, "issue");
        assert_eq!(parts.content, "fix it");
        assert!(matches!(
            parts.target,
            CommentTarget::Line {
                path,
                line: 42,
                side: LineSide::Old,
            } if path.as_path() == Path::new("src/main.rs")
        ));
    }

    #[test]
    fn should_build_add_request_from_nested_json_payload() {
        let parts = build_add_request_parts(AddCommentOptions {
            input: Some(
                r#"{"comment_type":"suggestion","content":"collapse this","target":{"type":"line_range","file":"src/main.rs","start_line":5,"end_line":7}}"#
                    .to_string(),
            ),
            comment_type: "note".to_string(),
            file: None,
            line: None,
            end_line: None,
            side: LineSideArg::New,
            username: None,
            content: None,
        })
        .unwrap();

        assert_eq!(parts.comment_type, "suggestion");
        assert!(matches!(
            parts.target,
            CommentTarget::LineRange {
                range: LineRange { start: 5, end: 7 },
                side: LineSide::New,
                ..
            }
        ));
    }

    fn save_pr_session(store: &ReviewStore) -> SessionRef {
        use crate::forge::traits::{ForgeRepository, PrSessionKey};

        let key = PrSessionKey::new(
            ForgeRepository::github("github.com", "slatedb", "slatedb"),
            1745,
            "43e3566924690c06a45b2177b4dd2df59a0f09c6".to_string(),
        );
        let mut session = ReviewSession::new(
            PathBuf::from("forge:github.com/slatedb/slatedb"),
            key.head_sha.clone(),
            Some("reviews".to_string()),
            SessionDiffSource::PullRequest,
        );
        session.pr_session_key = Some(key);
        store.save_review(&session).unwrap()
    }

    fn save_local_pr_session(store: &ReviewStore) -> SessionRef {
        use crate::forge::traits::{ForgeRepository, PrSessionKey};

        let key = PrSessionKey::new(
            ForgeRepository::local("slatedb", "slatedb"),
            7,
            "43e3566924690c06a45b2177b4dd2df59a0f09c6".to_string(),
        );
        let mut session = ReviewSession::new(
            PathBuf::from("forge:local/slatedb/slatedb"),
            key.head_sha.clone(),
            Some("feature".to_string()),
            SessionDiffSource::PullRequest,
        );
        session.pr_session_key = Some(key);
        store.save_review(&session).unwrap()
    }

    #[test]
    fn should_find_pr_session_by_repo_coordinate() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        let session_ref = save_pr_session(&store);

        // A bare repo coordinate surfaces the PR session and emits its slug.
        let listed = store
            .list_sessions_for_repo(Path::new("slatedb/slatedb"))
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].slug, "gh:slatedb/slatedb/pr/1745");
        assert_eq!(listed[0].kind, crate::review_store::SessionKind::Pr);

        // The emitted slug resolves the same way regardless of --repo.
        let resolved =
            resolve_session_ref(&store, Path::new("slatedb/slatedb"), &listed[0].slug).unwrap();
        assert_eq!(resolved, session_ref);
    }

    #[test]
    fn should_list_and_resolve_local_pr_session_as_pr_kind() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        let expected = save_local_pr_session(&store);
        let checkout = temp.path().join("checkout");
        let repository = git2::Repository::init(&checkout).unwrap();
        repository
            .remote("origin", "https://github.com/slatedb/slatedb.git")
            .unwrap();

        let listed = store
            .list_sessions_for_repo(Path::new("slatedb/slatedb"))
            .unwrap();
        let listed_from_checkout = store.list_sessions_for_repo(&checkout).unwrap();
        let resolved =
            resolve_session_ref(&store, Path::new("ignored"), "local:slatedb/slatedb/pr/7")
                .unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].slug, "local:slatedb/slatedb/pr/7");
        assert_eq!(listed[0].kind, crate::review_store::SessionKind::Pr);
        assert_eq!(listed_from_checkout[0].slug, listed[0].slug);
        assert_eq!(resolved, expected);
    }

    #[test]
    fn should_match_pr_session_via_forge_repo_path_coordinate() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        save_pr_session(&store);

        // The `forge:host/owner/repo` form (as stored on disk) also resolves.
        let listed = store
            .list_sessions_for_repo(Path::new("forge:github.com/slatedb/slatedb"))
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].slug, "gh:slatedb/slatedb/pr/1745");
    }

    #[test]
    fn should_not_match_pr_session_for_unrelated_repo() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        save_pr_session(&store);

        assert!(
            store
                .list_sessions_for_repo(Path::new("other/project"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn should_list_pr_session_in_list_all() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        save_pr_session(&store);

        let all = store.list_all_sessions().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].slug, "gh:slatedb/slatedb/pr/1745");
    }

    #[test]
    fn should_resolve_pr_session_by_slug_without_repo() {
        let temp = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(temp.path().join("reviews"));
        let session_ref = save_pr_session(&store);

        // PR slugs are self-contained: `--repo` is irrelevant.
        let resolved =
            resolve_session_ref(&store, Path::new("."), "gh:slatedb/slatedb/pr/1745").unwrap();
        assert_eq!(resolved, session_ref);
    }

    #[test]
    fn should_error_for_unknown_pr_slug() {
        let temp = tempdir().unwrap();
        let reviews = temp.path().join("reviews");
        let store = ReviewStore::with_reviews_dir(&reviews);
        let err = resolve_session_ref(&store, Path::new("."), "gh:nope/nope/pr/9999").unwrap_err();
        assert!(matches!(err, TuicrError::InvalidInput(_)));
    }

    #[test]
    fn should_list_add_and_show_comments() {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let reviews = temp.path().join("reviews");
        let store = ReviewStore::with_reviews_dir(&reviews);
        let session = test_session(repo.clone());
        let session_ref = store.save_review(&session).unwrap();

        let mut out = Vec::new();
        let sessions = store.list_sessions_for_repo(&repo).unwrap();
        assert_eq!(sessions.len(), 1);
        let slug = sessions[0].slug.clone();

        let resolved = resolve_session_ref(&store, &repo, &slug).unwrap();
        assert_eq!(resolved, session_ref);

        let comment = store
            .add_comment(
                &resolved,
                AddCommentRequest {
                    target: CommentTarget::Line {
                        path: PathBuf::from("src/main.rs"),
                        line: 42,
                        side: LineSide::New,
                    },
                    content: "check this".to_string(),
                    comment_type: CommentType::from_id("issue"),
                    author: "review-agent".to_string(),
                    commit_id: None,
                },
            )
            .unwrap();

        let loaded = store.get_review(&session_ref).unwrap();
        let comments = collect_comments(&loaded);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, comment.id);
        assert_eq!(comments[0].location, "src/main.rs:42");
        assert_eq!(comments[0].comment_type, "issue");
        assert_eq!(comments[0].author, "review-agent");

        show_comments(&session_ref.path().display().to_string(), &repo, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value[0]["comment_type"], "issue");
        assert_eq!(value[0]["author"], "review-agent");
        assert_eq!(value[0]["location"], "src/main.rs:42");
        assert_eq!(value[0]["content"], "check this");
    }

    fn config_with_types(ids: &[&str]) -> config::AppConfig {
        config::AppConfig {
            comment_types: Some(
                ids.iter()
                    .map(|id| config::CommentTypeConfig {
                        id: (*id).to_string(),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn should_warn_when_type_is_not_configured() {
        // given a config declaring note and issue
        let config = config_with_types(&["note", "issue"]);

        // when a typo slips through
        let warning = unknown_comment_type_warning(&CommentType::from_id("isue"), Some(&config));

        // then the id and the valid ids are both named
        let warning = warning.expect("an unconfigured type should warn");
        assert!(warning.contains("'isue'"), "got {warning}");
        assert!(warning.contains("note, issue"), "got {warning}");
    }

    #[test]
    fn should_not_warn_for_a_configured_type() {
        let config = config_with_types(&["note", "issue"]);
        assert!(
            unknown_comment_type_warning(&CommentType::from_id("issue"), Some(&config)).is_none()
        );
    }

    #[test]
    fn should_not_warn_when_comment_types_are_unconfigured() {
        // `comment_types` is unset by default, which leaves every id but
        // `none` undefined. Warning there would fire on every typed comment
        // the agent skill documents, so an absent list means no opinion.
        assert!(unknown_comment_type_warning(&CommentType::from_id("issue"), None).is_none());
        assert!(
            unknown_comment_type_warning(
                &CommentType::from_id("issue"),
                Some(&config::AppConfig::default())
            )
            .is_none()
        );
    }

    #[test]
    fn should_not_warn_for_the_untyped_default() {
        // `--type none` is always valid and never carries a badge.
        let config = config_with_types(&["issue"]);
        assert!(
            unknown_comment_type_warning(&CommentType::from_id("none"), Some(&config)).is_none()
        );
    }

    #[test]
    fn should_still_store_a_comment_whose_type_is_unconfigured() {
        // given a session and a type no config declares
        let dir = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(dir.path());
        let session = test_session(PathBuf::from("/tmp/repo"));
        let session_ref = store.save_review(&session).unwrap();

        // when
        let comment = store
            .add_comment(
                &session_ref,
                AddCommentRequest {
                    target: CommentTarget::File {
                        path: PathBuf::from("src/main.rs"),
                    },
                    content: "body".to_string(),
                    comment_type: CommentType::from_id("isue"),
                    author: "Codex".to_string(),
                    commit_id: None,
                },
            )
            .expect("an unconfigured type must not block the write");

        // then the warning is advisory only — the comment is still stored
        assert_eq!(comment.comment_type.id(), "isue");
    }

    #[test]
    fn should_include_author_in_add_comment_output() {
        let comment = Comment::new(
            "check this".to_string(),
            CommentType::from_id("issue"),
            Some(LineSide::New),
        )
        .with_author("review-agent");
        let value = serde_json::to_value(CommentOutput::from_target(
            &CommentTarget::Line {
                path: PathBuf::from("src/main.rs"),
                line: 42,
                side: LineSide::New,
            },
            &comment,
        ))
        .unwrap();
        assert_eq!(value["author"], "review-agent");
    }
}
