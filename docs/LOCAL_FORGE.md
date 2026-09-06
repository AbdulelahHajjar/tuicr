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
  `viewer_login` for since-last-review scoping. Threads opened directly (see
  *Direct-to-thread comments*) name their own author instead: the TUI's
  configured `username`, or the CLI's `--username`.

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
`reviews/` session store. Both fields are sanitized into one path component:
`/`, `\`, `..`, and a leading `.` are replaced. Nothing is ever written inside a
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
  "created_at": "…", "updated_at": "…", "is_resolved": false, "resolved_at": null, "review_id": 1,
  "comments": [ { "id": "…uuid…", "author": "…", "body": "…", "created_at": "…", "updated_at": null } ] }
```

Review ids are numeric (`GhCreateReviewResponse.id` is `u64`); thread and
comment ids are UUID strings (`RemoteReviewThread.id` is an opaque string).
`review_id` is `null` for threads opened directly, outside any review; nothing
reads it back. A thread's `updated_at` moves on every mutation — reply, edit,
delete, resolve, unresolve — and is filled from `created_at` when a file
predates the field, so one field comparison tells a poller the thread changed.
A comment's `updated_at` is set when its body is amended.

## `LocalForgeBackend` — `ForgeBackend` method by method

`src/forge/local/{mod.rs, store.rs, target.rs, anchor.rs}`. The backend
holds the repository identity and an optional checkout path; checkout discovery,
store lookup, and author lookup happen on first use. All git
reads go through `git2` against the checkout (or the CLI adapter in
`vcs/git/raw.rs`, which already produces `FilePatch` values) — never through
a network. The checkout is the source of truth.

| Method | Local behaviour |
|---|---|
| `list_pull_requests(query)` | Every local branch except the base branch that has ≥ 1 commit not reachable from the base, newest tip first; allocates numbers for branches that have none; paged by `already_loaded`/`page_size`. `ReviewRequested` scope returns the same list (no reviewer concept). Summary: `title` = tip commit subject, `author` = tip author name, `head_ref_name`, `base_ref_name`, `updated_at` = tip commit time, `state: "OPEN"`, `is_draft: false`. |
| `get_pull_request(target)` | Pull by `target.number`. `head_sha` = tip of `head_ref` (or `last_head_sha` when the branch is gone → closed), `base_sha` = `merge-base(base_ref, head_sha)`, `title` = tip subject, `body` = commit list: with one commit its body; with several, one `- <short sha> <subject>` line per commit oldest-first followed by each non-empty commit body indented. `author` = tip author, `updated_at` = tip time, `url` as above, `diff_start_sha: None`. |
| `get_pull_request_info(target)` | `from_details` plus: `review_decision` = state of the newest non-pending review (`APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`), `None` when there is none; `latest_reviews` = newest non-pending review per author; `mergeable`/`merge_state`/`checks`/`requested_reviewers`/`issue_comments` empty. |
| `get_pull_request_diff(pr)` | Cumulative `FilePatch`es for `base_sha..head_sha`. |
| `review_threads_revision(pr)` — **new trait method** | `Some(ReviewThreadsRevision)` hashed from `pulls/<n>/threads.json`'s modification time and length (a distinct marker when the file is absent). Default on the trait: `Ok(None)`, which disables thread auto-refresh for that backend. |
| `head_status(pr)` | `Open(head_sha)` while `refs/heads/<head>` exists, otherwise `Closed`. Other backends use the trait default `Ok(None)`. |
| `get_pull_request_commit_range_diff(pr, start, end)` | `FilePatch`es for `start..end`. |
| `list_pull_request_commits(pr)` | Commits `base_sha..head_sha`, oldest first: `oid`, 7-char `short_oid`, `summary` = subject, `author` = author name, `timestamp`. |
| `list_pull_request_review_metadata(pr)` | `viewer_login` = Author; one record per non-pending review: `(author, submitted_at, Some(commit_id))`. |
| `list_review_summaries(pr)` | One `RemoteReviewSummary` per non-pending review with a non-empty body; `state` from the event. |
| `list_review_threads(pr)` | Every thread in `threads.json`, re-anchored against the current `base_sha..head_sha` diff (see *Anchoring*). Comments map to `RemoteReviewComment` (`in_reply_to` = root comment id for every comment after the first). Pending-review threads are included (the author sees their own pending comments, as on GitHub). |
| `fetch_file_lines(request)` / `file_line_count` | Read the blob at `request.sha()` for `request.path` from the checkout. |
| `local_checkout_path()` | `Some(checkout)`. |
| `create_review(pr, request)` | Allocate a review id; store the review (`Draft` → `PENDING`, others → their GitHub event name; `commit_id = request.commit_id`; `body`). For each `InlineComment`, create one thread anchored at `(path, line, side)` with `original_commit = request.commit_id`, `base_commit = pr.base_sha`, `line_text` = content of that diff line (looked up in the `start..end` patch the comment was mapped against; empty string when it cannot be found), one root comment (`body`, Author, `review_id`). Any event reuses the Author's outstanding `PENDING` review: another Draft updates its non-empty body and attaches new threads; a non-Draft submits that same review in place. Only when no pending review exists is a new review allocated. The range the inline comments were mapped against arrives as `CreateReviewRequest::diff_start_sha` (the parent SHA the displayed diff starts at, `None` for the full pull request); `line_text` is looked up in the `<that start>..<request.commit_id>` patch. Return `GhCreateReviewResponse { id, html_url, state }` where `state` is `PENDING`/`COMMENTED`/`APPROVED`/`CHANGES_REQUESTED`. |
| `resolve_thread(pr, thread_id, resolved)` — **new trait method** | Set `is_resolved`/`resolved_at` on the thread and persist. Default implementation on the trait: `Err(TuicrError::UnsupportedOperation("Resolving review threads is not supported on <Forge>"))`; no other backend implements it in this version. |
| `update_thread_comment(pr, thread_id, comment_id, author, body)` / `delete_thread_comment(pr, thread_id, comment_id, author)` — **new trait methods** | Amend or remove one comment (`comment_id = None` addresses the root). The comment must carry `author`; anyone else's is refused. Deleting the last comment removes the thread; deleting a root that has replies is refused so the replies keep their context. `delete` returns whether the thread went with the comment. Both reject a closed pull. Defaults on the trait: `UnsupportedOperation`; only Local implements them. |
| `create_thread(pr, request)` — **new trait method** | Open one thread at `(path, line, side)` outside any review: `original_commit = request.commit_id`, `base_commit = pr.base_sha`, `line_text` looked up in the `<diff_start_sha or base>..<commit_id>` patch exactly as `create_review` does, `review_id = null`, one root comment authored by `request.author` (falling back to Author). The path must be part of that diff (error otherwise); a line outside it stores an empty `line_text`. A closed pull is rejected. Returns the thread as a `RemoteReviewThread` anchored at `original_line`, not outdated. Default on the trait: `UnsupportedOperation`; only Local implements it. The inherent `create_local_thread` returns the stored record for the CLI. |

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

The `mr` command remains forge-only with a required target and no `--base`
option. `tuicr mr 125` and `tuicr tui mr 125` keep their existing routing,
while a missing target or `--base` is rejected by clap.

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
a `Local` arm. Construction is infallible; a missing checkout is reported by
the first backend method that needs it (the `local_checkout` argument;
threads/reload/submit spawns obtain it from `backend.local_checkout_path()`
exactly as they do for GitHub). Opening a local pull request does not call
`resolve_canonical_repository` or any `gh` command.

The target selector's Pull Requests tab has Forge and Local sources. `l`
switches sources and reloads page one; the Local source lists branches ahead
of the base through `LocalForgeBackend::list_pull_requests`, supports the same
paging and open flow, and is the default when no forge remote is detected.
The `r` review-requested filter applies only to the Forge source.

## Auto-follow

A backend opts into pull-request following through `head_status`. Local returns
`Open(head_sha)` or `Closed`; the default `Ok(None)` disables following. When an
open head moves, or a previously open head closes, the tick calls
`spawn_pr_reload()`. It defers while the input mode is not `Normal`, or while a
submit, range reload, or PR reload is in flight, and never follows a PR that is
already closed. Everything after
that is the existing reload path (`finish_pr_reload` → head changed →
`opened_pr_with_new_head_session` → "Reloaded PR at new head"). `:e` keeps
working as well, and a fresh launch carries state from the previous head.

New config key `local_pr_follow_interval_ms` (default `1000`; `0` disables).
`diff_watch_interval_ms` keeps its current meaning and stays ignored for
pull requests of other forges.

The same tick also watches the thread store. `review_threads_revision` (new
trait method, default `Ok(None)`; Local hashes `threads.json`'s modification
time and length, with a marker of its own for an absent file) is sampled once
per tick, and when it differs from the last sample the threads are re-fetched
**in place**: the rows already on screen stay until the new list lands, so a
reply written by another process — `tuicr review reply` from an agent — shows
up within an interval and without `:e`. The first sample after opening only
records the marker (the open fetched those threads). A head move takes
precedence, since its reload re-fetches threads anyway; the tick defers while
a thread fetch is in flight, while the input mode is not `Normal`, or while a
submit or reload runs. This process's own writes move the marker too and cost
one background re-read. `local_pr_follow_interval_ms = 0` disables following
and refreshing alike; `:e` remains the manual path.

Every rebuild that can insert or remove rows above the cursor — threads
landing after a reload or a refresh, a thread hidden by `:comments`, a thread
saved, resolved, edited, or deleted, a range diff replacing the view — is
bracketed by a *view anchor* (`app/view_anchor.rs`), captured before the
thread list or the rows change and restored after the rebuild. The anchor
names the cursor's target (a diff line by path and line numbers, a thread
by id plus the row inside its block with the thread's own line as fallback, or
an overview row by index) together with the cursor's distance from the top of
the viewport, rebuilds, and lands back on the same thing at the same screen
row. A head-follow reload captures the view before it starts and hands it to
the thread landing that follows, keyed by the head it was captured at, so the
cursor returns to the thread row it was on rather than to the diff line under
it. `:e` gains the thread-row anchor as well.

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

## Direct-to-thread comments

The Local review flow is draft-free for inline comments. A new line or range
comment saved in the TUI on an open Local pull request goes straight to
`threads.json` through `ForgeBackend::create_thread`, authored as the
configured `username`, and appears in the diff and the comment navigator at
once. No session draft is written and no `:submit` is needed for it. The
anchor is the displayed diff: with the inline commit selector on a strict
subset, `commit_id` is the subset's newest commit and `diff_start_sha` its
parent, exactly as `:submit` maps drafts. A range comment anchors at its last
line. If the write fails, the error is shown and the comment box stays open
with its text; nothing falls back to a draft.

File-level and review-level comments, edits of existing drafts, and comments
on a closed pull keep the session-draft path; the verdict `:submit` folds them
into the review body as before. `:summary`, `:clear`, and `:clearc` act on
drafts only. `:submit approve` / `request-changes` / `comment` remain the way
to record a verdict.

The CLI mirrors this: `tuicr review add --session local:… --target-file <path>
--line <n>` opens a thread (see `docs/REVIEW_CLI.md`). `--repo` must point at
the checkout so `line_text` can be snapshotted against the pull request diff.

## Editing and deleting threads

`dd` on a thread row of an open Local pull request deletes the thread when
the viewer (config `username`) wrote its root comment; `i` / `A` open that
root comment in the comment box, and saving writes the new body through
`update_thread_comment` and mirrors it in the diff. The body is edited as
stored, `[TYPE] ` tag included. A thread whose root belongs to someone else
answers "Thread by X — only its author can edit/delete it"; a root that
already has replies cannot be deleted (resolve it, or delete the replies
first). On other forges thread rows stay read-only. Replies are edited and
deleted through the CLI (`tuicr review edit` / `tuicr review delete`), which
address any comment by id under the same author rule.

## Fork build markers

- `tuicr --version` prints `tuicr <cargo version>+local-forge` (build metadata
  appended at compile time from `CARGO_PKG_VERSION`; `Cargo.toml` is not
  edited so upstream rebases stay clean).
- `tuicr update` exits non-zero with: `This is the local-forge fork build; update with tuicr-fork-update`.

## Documentation to keep in step

`README.md` (a "Local pull requests" section under forge review; commands
table), `docs/KEYBINDINGS.md` (`:resolve`, `:unresolve`), `docs/CONFIG.md`
(`local_pr_follow_interval_ms`), `AGENTS.md` (module tree, key types, forge
invariants — including that `Local` never calls a forge CLI or the network and that thread
resolution exists only for `Local`), `src/ui/help_popup.rs`,
and this file.

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
- view anchoring: threads landing above the cursor keep it on its diff line and at its screen row; a refetch lands back on the same thread row; a vanished thread falls back to its line; a carried anchor is honoured only after the head moved and dropped otherwise; the follow tick carries the view; a reload started from a thread row anchors on the thread's line
- thread auto-refresh: store `threads_revision` differs across writes and for an absent file; the follow tick records the first sample without a fetch, re-fetches in place (rows kept) when the marker moves, reports unchanged when it matches, prefers a head move, and defers while a thread fetch is in flight
- direct threads: store `add_thread` keeps `review_id` null and older numeric files still load; backend `create_thread` snapshots `line_text` (full diff and commit subset), stamps the requested author, rejects a closed pull and a path outside the diff
- TUI save on a Local pull request: line and range comments call `create_thread` with the displayed diff's SHAs and leave no draft; file-level, GitHub, and closed-pull saves still draft; a failed write keeps the comment box open
- `tuicr review add` on a `local:` slug with a line target writes a thread (flags and `--input`), rejects a non-checkout `--repo` and a checkout of another repository, and still drafts for review-level targets and other forges
- thread edits: store `update_thread_comment` stamps `updated_at` and `delete_thread_comment` removes an emptied thread; both refuse another author's comment and a root with replies cannot be deleted; the backend rejects a closed pull; TUI `dd` / `i` act on the viewer's own thread and keep the editor open on a failed write; `tuicr review edit` / `delete` address comments by id
- regression: every existing test still passes; GitHub target parsing unchanged
