# Local forge — pull-request review of local branches

The Local forge is a `ForgeBackend` whose "pull request" is a local branch
compared against the repository's default branch. Everything tuicr does for a
GitHub pull request — the description panel, the Pull Requests tab, cumulative
and per-commit diffs, review threads with resolved/outdated state,
`:submit` with a review history, "commits since your last review", `:e` at a
new head with carried review state — works on a branch that was never pushed.
Two things exist only here: threads can be resolved from the TUI
(`:resolve` / `:unresolve`), and a local pull request follows its branch
automatically as commits land.

This document is the contract for the implementation. Anything not stated
here follows the conventions of the existing forge modules (see `AGENTS.md`,
"Forge integration").

## Vocabulary

- **Checkout** — the git working copy tuicr was started in (`std::env::current_dir()`
  resolved to its repository root). Always a real git repository.
- **Base branch** — the branch a local pull request is compared against.
  Resolution order: `--base <ref>` on the command line (must name a branch or
  other reference, stored by its shorthand — a moving revision such as
  `HEAD~2` is rejected); the branch that `refs/remotes/origin/HEAD` points at
  (its local branch when one exists, else the remote-tracking branch
  `origin/<name>`); the first of `develop`, `main`, `master` that exists as a
  local branch. Otherwise opening fails with an error that names all three
  sources. `--base` combined with a forge target (a number, `owner/repo#N` or
  a URL) is an error rather than silently ignored.
- **Head branch** — the local branch under review (`refs/heads/<name>`).
- **Local pull request** (LPR) — a numbered record `(number, head_ref, base_ref)`
  in the local forge store. One per head branch name per repository; the
  number never changes for that branch name.
- **Repository identity** — `ForgeRepository { kind: Local, host: "local", owner, name }`.
  `owner`/`name` come from the same remote parsing the local session slugs
  use (`slug.rs`, `parse_remote_owner_repo` over `origin`), so a checkout of
  `github.com/HudHud-Maps/hudhud-ios` is `HudHud-Maps/hudhud-ios` in both
  local sessions and local pull requests. Without a parseable `origin`:
  `owner = "local"`, `name` = the checkout's directory name.
- **Author** — the reviewer's display name: `git config user.name` of the
  checkout, else `$USER`, else `you`. Used as comment/review author and as
  `viewer_login` for since-last-review scoping.

## Identity and persistence surfaces

| Surface | Value |
|---|---|
| `ForgeKind` | new variant `Local` (serde `local`), `display_name()` → `"Local"` |
| `ForgeRepository::local(owner, name)` | host `"local"`; `display_name()` returns `owner/name` for this host, like the known public hosts |
| Slug | `local:<owner>/<name>/pr/<number>` — `Slug` parses and prints the `local` forge prefix; everything else about `PrSlug` is unchanged |
| PR session key | `PrSessionKey { repository: <Local repo>, number, head_sha }` — unchanged type; persistence, manifest (`Pr { number, head_sha }`), carry-forward and `tuicr review …` need no new concepts beyond the slug prefix |
| Session `repo_path` | `pr_session_repo_path(key)` unchanged → `forge:local/<owner>/<name>` |
| PR URL | `local:<owner>/<name>/pull/<number>` (`PullRequestDetails.url`, `PullRequestSummary.url`, review/thread/comment `url`s use `local:<owner>/<name>/pull/<number>#review-<id>`, `#thread-<id>`, `#comment-<id>`) |

`tuicr review list --repo <checkout>` and `--repo owner/name` must list local
pull-request sessions of that repository alongside GitHub ones; `tuicr review
comments --session local:…` resolves them. Export headers print
`Local`, `URL: local:…`, `Head: <sha8>` through the existing PR export path.

## The store

Location: `<tuicr data dir>/local-forge/<owner>__<name>/` — a sibling of the
`reviews/` session store (same `ProjectDirs` lookup; any `/` inside `owner`
is replaced by `-` in the directory name). Nothing is ever written inside a
repository working tree or `.git`.

```
local-forge/HudHud-Maps__hudhud-ios/
├── .lock                       # same protocol as reviews/.tuicr.lock
├── pulls.json
└── pulls/
    └── 1/
        ├── reviews.json
        └── threads.json
```

All files are JSON written atomically (temp file + rename) under the
directory lock; the lock helper in `persistence/storage.rs` is generalized
over a directory rather than duplicated. Every file carries `"version": 1`.

`pulls.json`

```json
{
  "version": 1,
  "next_number": 3,
  "pulls": [
    { "number": 1, "head_ref": "feature/HHIOS-2563", "base_ref": "develop",
      "created_at": "…", "updated_at": "…", "last_head_sha": "…" }
  ]
}
```

- `number` is allocated on first open or first listing of a head branch and
  is stable for that branch name. If the branch is deleted the pull is
  **closed** (`state: "CLOSED"`, `closed: true`, review read-only, head =
  `last_head_sha`); recreating a branch with the same name reopens the same
  number. (Deliberate departure from GitHub, which would open a new PR — a
  local branch name is the user's identity for the work.) Known limitation: a
  closed pull's `last_head_sha` is kept alive only by the reflog, so once
  `git gc` prunes it the closed pull can no longer be opened; the contract
  forbids writing refs into the repository to pin it.
- `base_ref` is recorded on creation and updated when the user passes
  `--base`.
- `last_head_sha` is refreshed on every open/reload.

`pulls/<n>/reviews.json` — array, oldest first:

```json
{ "id": 1, "event": "COMMENT" | "APPROVE" | "REQUEST_CHANGES" | "PENDING",
  "body": "…", "commit_id": "<head sha reviewed>", "author": "…", "submitted_at": "…" }
```

`pulls/<n>/threads.json` — array, oldest first:

```json
{ "id": "…uuid…", "path": "Sources/A.swift", "side": "RIGHT" | "LEFT",
  "original_line": 171, "original_commit": "<head sha when created>",
  "base_commit": "<base sha when created>", "line_text": "<diff line content, no origin marker>",
  "created_at": "…", "is_resolved": false, "resolved_at": null, "review_id": 1,
  "comments": [ { "id": "…uuid…", "author": "…", "body": "…", "created_at": "…" } ] }
```

Review ids are numeric (`GhCreateReviewResponse.id` is `u64`); thread and
comment ids are UUID strings (`RemoteReviewThread.id` is an opaque string).

## `LocalForgeBackend` — `ForgeBackend` method by method

`src/forge/local/{mod.rs, store.rs, target.rs, anchor.rs}`. The backend
holds the repository identity, the checkout path and a store handle. All git
reads go through `git2` against the checkout (or the CLI adapter in
`vcs/git/raw.rs`, which already produces `FilePatch` values) — never through
a network. The checkout is the source of truth.

| Method | Local behaviour |
|---|---|
| `list_pull_requests(query)` | Every local branch except the base branch that has ≥ 1 commit not reachable from the base, newest tip first; allocates numbers for branches that have none; paged by `already_loaded`/`page_size`. `ReviewRequested` scope returns the same list (no reviewer concept). Summary: `title` = tip commit subject, `author` = tip author name, `head_ref_name`, `base_ref_name`, `updated_at` = tip commit time, `state: "OPEN"`, `is_draft: false`. |
| `get_pull_request(target)` | Pull by `target.number`. `head_sha` = tip of `head_ref` (or `last_head_sha` when the branch is gone → closed), `base_sha` = `merge-base(base_ref, head_sha)`, `title` = tip subject, `body` = commit list: with one commit its body; with several, one `- <short sha> <subject>` line per commit oldest-first followed by each non-empty commit body indented. `author` = tip author, `updated_at` = tip time, `url` as above, `diff_start_sha: None`. |
| `get_pull_request_info(target)` | `from_details` plus: `review_decision` = state of the newest non-pending review (`APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`), `None` when there is none; `latest_reviews` = newest non-pending review per author; `mergeable`/`merge_state`/`checks`/`requested_reviewers`/`issue_comments` empty. |
| `get_pull_request_diff(pr)` | Cumulative `FilePatch`es for `base_sha..head_sha`. |
| `get_pull_request_commit_range_diff(pr, start, end)` | `FilePatch`es for `start..end`. |
| `list_pull_request_commits(pr)` | Commits `base_sha..head_sha`, oldest first: `oid`, 7-char `short_oid`, `summary` = subject, `author` = author name, `timestamp`. |
| `list_pull_request_review_metadata(pr)` | `viewer_login` = Author; one record per non-pending review: `(author, submitted_at, Some(commit_id))`. |
| `list_review_summaries(pr)` | One `RemoteReviewSummary` per non-pending review with a non-empty body; `state` from the event. |
| `list_review_threads(pr)` | Every thread in `threads.json`, re-anchored against the current `base_sha..head_sha` diff (see *Anchoring*). Comments map to `RemoteReviewComment` (`in_reply_to` = root comment id for every comment after the first). Pending-review threads are included (the author sees their own pending comments, as on GitHub). |
| `fetch_file_lines(request)` / `file_line_count` | Read the blob at `request.sha()` for `request.path` from the checkout. |
| `local_checkout_path()` | `Some(checkout)`. |
| `create_review(pr, request)` | Allocate a review id; store the review (`Draft` → `PENDING`, others → their GitHub event name; `commit_id = request.commit_id`; `body`). For each `InlineComment`, create one thread anchored at `(path, line, side)` with `original_commit = request.commit_id`, `base_commit = pr.base_sha`, `line_text` = content of that diff line (looked up in the `start..end` patch the comment was mapped against; empty string when it cannot be found), one root comment (`body`, Author, `review_id`). A non-`Draft` event with a `PENDING` review of the Author outstanding **submits that pending review in place** (GitHub's "submit pending review"): its event becomes the new event, its body the request body when non-empty, its `submitted_at` now, and the new threads attach to it; the response carries *its* id. Only when no pending review exists is a new review allocated. The range the inline comments were mapped against arrives as `CreateReviewRequest::diff_start_sha` (the parent SHA the displayed diff starts at, `None` for the full pull request); `line_text` is looked up in the `<that start>..<request.commit_id>` patch. Return `GhCreateReviewResponse { id, html_url, state }` where `state` is `PENDING`/`COMMENTED`/`APPROVED`/`CHANGES_REQUESTED`. |
| `resolve_thread(pr, thread_id, resolved)` — **new trait method** | Set `is_resolved`/`resolved_at` on the thread and persist. Default implementation on the trait: `Err(TuicrError::UnsupportedOperation("Resolving review threads is not supported on <Forge>"))`; no other backend implements it in this version. |

### Anchoring (outdated detection)

GitHub keeps a thread's original position and reports `line` on the current
diff, marking the thread *outdated* when that line is no longer part of it.
Locally, for each thread:

1. `original_commit == pr.head_sha` **and** the cumulative diff has `line_text` at `(side, original_line)` (or `line_text` is empty) → `line = Some(original_line)`, not outdated (fast path). The content check matters for comments made against a commit-subset diff: a `LEFT` line number there is an old-line number of the subset's start tree, not of the base tree.
2. Otherwise take the current cumulative patch for `path` and collect the
   candidate lines on the thread's side — `RIGHT`: added and context lines
   with their *new* line numbers; `LEFT`: deleted and context lines with
   their *old* numbers — whose content equals `line_text`. If any exist,
   `line = Some(candidate nearest to original_line)`, not outdated.
3. No candidate (file absent from the diff, line removed, or `line_text`
   empty) → `line = None`, `is_outdated = true`.

Re-anchoring never rewrites the store; `original_*` fields are immutable.

## Command line

```
tuicr pr                         # current branch as a local pull request
tuicr pr <branch>                # another local branch
tuicr pr --base <ref> [<branch>] # override the base branch (recorded on the pull)
tuicr pr 125 | owner/repo#125 | <PR URL>   # unchanged: the origin forge's pull request
```

`PrCommand.target` becomes `Option<String>`; `--base <REF>` is added. `CliArgs`
carries the invocation as `pr: Option<PrInvocation { target: Option<String>, base: Option<String> }>`
(replacing `pr_target: Option<String>`), so "`tuicr pr` with no target" is
distinguishable from "no `pr` subcommand".

Target resolution in `App::new_from_pr_target…`:

1. No target → the checkout's current branch. Detached HEAD, or HEAD on the
   base branch itself, is an error ("nothing to review on `develop`; run
   `tuicr pr <branch>`").
2. A target the existing forge parsers accept (numeric, `owner/repo#N`, URL)
   → the existing forge path, unchanged. A numeric target is therefore never
   a branch name.
3. Otherwise, if `refs/heads/<target>` exists → local pull request for that
   branch. Else an error naming both interpretations.

The Local repository identity, the store and the pull number are resolved in
`forge/local/target.rs`; the result is a `PullRequestTarget::with_repository(local_repo, number, original)`
fed to the unchanged `open_pull_request` flow. `create_forge_backend` gains
a `Local` arm that requires the checkout path (the `local_checkout` argument;
threads/reload/submit spawns obtain it from `backend.local_checkout_path()`
exactly as they do for GitHub). Opening a local pull request does not call
`resolve_canonical_repository` or any `gh` command.

## Auto-follow

A local pull request follows its head branch: on the diff-watch tick, when the
diff source is a `PullRequest` whose repository kind is `Local`, compare the
current tip of `head_ref` in the checkout with `current_pr_head`; if it moved
and no PR reload is in flight, call `spawn_pr_reload()`. Everything after
that is the existing reload path (`finish_pr_reload` → head changed →
`opened_pr_with_new_head_session` → "Reloaded PR at new head"). `:e` keeps
working as well, and a fresh launch carries state from the previous head.

New config key `local_pr_follow_interval_ms` (default `1000`; `0` disables).
`diff_watch_interval_ms` keeps its current meaning and stays ignored for
pull requests of other forges.

## `:resolve` / `:unresolve`

Command-mode commands (`CommandKind::ResolveThread(bool)`) available in PR
mode. The target thread is:

1. the comment-navigator selection when the navigator is focused and the
   selected row is a remote thread; otherwise
2. the annotation under the diff cursor when it is a `RemoteThreadLine`; otherwise
3. the first thread anchored at the diff line under the cursor (same path,
   line and side).

With no thread at the cursor: error "No review thread at cursor". On success
the in-memory thread is updated, annotations rebuilt (under `:comments
unresolved` a resolved thread disappears; the cursor is clamped), and the
status bar shows "Thread resolved" / "Thread reopened". An
`UnsupportedOperation` error is shown verbatim. The call is synchronous
(local file I/O; the only implementation in this version).

## Fork build markers

- `tuicr --version` prints `tuicr <cargo version>+local-forge` (build metadata
  appended at compile time from `CARGO_PKG_VERSION`; `Cargo.toml` is not
  edited so upstream rebases stay clean).
- `tuicr update` exits non-zero with: `This is the local-forge fork build; update with tuicr-fork-update`.

## Documentation to keep in step

`README.md` (a "Local pull requests" section under forge review; commands
table), `docs/KEYBINDINGS.md` (`:resolve`, `:unresolve`), `docs/CONFIG.md`
(`local_pr_follow_interval_ms`), `AGENTS.md` (module tree, key types, forge
invariants — including that `Local` never shells out and that thread
resolution exists only for `Local`), `src/ui/help_popup.rs`,
`src/ui/status_bar.rs` (PR-mode hint), and this file.

## Verification contract

Every stage ends with `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`
and `cargo test` green. Unit tests follow upstream's `should_…` naming and
live in the module they cover; app-level tests build temporary git
repositories with `git2` + `tempfile` like the existing suites. Required
coverage (not exhaustive):

- slug round-trip `local:owner/name/pr/7`; unknown prefix still rejected
- pull-number stability across opens; closed-when-branch-gone; reopen keeps the number
- target resolution: no target → current branch; numeric → forge; branch name → local; detached/base-branch errors; `--base`
- diff/commit list/commit-range diff against a temp repo with two commits
- `create_review`: Comment/Approve/RequestChanges/Draft; pending promotion; thread creation with `line_text`
- anchoring: fast path; moved line; deleted line → outdated; duplicate lines pick the nearest
- review metadata drives since-last-review preselection through the existing selector code
- `:resolve`/`:unresolve` parse + app behaviour with a fake backend (navigator selection, cursor on thread row, cursor on anchored line, nothing at cursor, unsupported forge)
- auto-follow tick: moved head → reload spawned once; no double spawn while in flight; disabled at `0`
- `tuicr review list --repo <checkout>` lists a local PR session; `--session local:…` resolves
- regression: every existing test still passes; GitHub target parsing unchanged
