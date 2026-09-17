# Review Session CLI

`tuicr review` exposes persisted review sessions without opening the TUI. It is
intended for scripts and coding agents that need to inspect or update tuicr's
saved review state.

Interactive TUI sessions create a persisted session file as soon as a review
target becomes active, so agents can resolve the announced slug immediately.
If that auto-created file still has no comments and no reviewed files when the
TUI exits, tuicr removes it.

While a TUI is open, tuicr records the active session in
`active_sessions.json` beside the storage manifest with the process id, slug,
session path, and last-seen timestamp. `tuicr review list` also includes an
`active` boolean so agents can select the live session without guessing from
timestamps.

Session arguments accept any of:

- a local slug from `tuicr review list`
- a PR slug, e.g. `gh:slatedb/slatedb/pr/1745` (PR slugs resolve without `--repo`)
- an absolute or relative path to a session JSON file (anything ending in
  `.json` or that exists on disk is treated as a direct path)

## Commands

```bash
tuicr review list --repo .                            # checkout + its repo's PR sessions
tuicr review list --repo slatedb/slatedb              # all sessions for a forge repo
tuicr review list --all                               # every session across all repos
tuicr review comments --session agavra/tuicr@main/worktree
tuicr review comments --session gh:slatedb/slatedb/pr/1745
```

All `tuicr review` commands emit JSON by default. Timestamps are RFC3339 strings
so callers can parse them without locale-specific handling.

## The `--repo` selector

`--repo` is a repo selector, not just a path. It accepts:

- a checkout path (default `.`) — matches that checkout's local sessions and,
  via its `origin` remote, any PR sessions for the same repo
- a forge coordinate: `owner/repo`, `host/owner/repo`, `forge:host/owner/repo`,
  or a repo / PR URL — matches local and PR sessions by `owner/repo`

This is how PR sessions become discoverable. PR review sessions are keyed by
forge coordinates rather than a local checkout, so naming the repo — either by
standing in its checkout or passing `--repo slatedb/slatedb` — surfaces them.
`list` emits a usable slug for each; pass a PR slug to `--session` to read or
annotate it (no `--repo` needed, since PR slugs are self-contained).

```bash
# from anywhere:
tuicr review list --repo slatedb/slatedb
#   -> [ ..., { "slug": "gh:slatedb/slatedb/pr/1745", "kind": "pr", ... } ]
tuicr review comments --session gh:slatedb/slatedb/pr/1745
```

Azure DevOps repos live under `organization/project/repository`, so their
`owner` is `org/project` and a bare `owner/repo` selector cannot name them. Use
a URL form, which is recognized by host:

```bash
tuicr review list --repo dev.azure.com/myorg/myproject/_git/myrepo
tuicr review list --repo git@ssh.dev.azure.com:v3/myorg/myproject/myrepo
#   -> [ ..., { "slug": "az:myorg/myproject/myrepo/pr/123", "kind": "pr", ... } ]
```

`--repo` for `add` / `comments` is only consulted when resolving a *local*
slug; PR slugs and JSON paths ignore it. The one exception is a thread on a
`local:` pull request, where `--repo` must be that repository's checkout (see
[Forge Threads](#forge-threads-local-pull-requests)).

## Add Comments

Use flags for quick manual comments:

```bash
tuicr review add --session agavra/tuicr@main/worktree \
  --target-file src/main.rs \
  --line 42 \
  --side new \
  --type issue \
  "Handle the empty case here."
```

Target flags:

- omit `--target-file` for a review-level comment
- pass `--target-file <path>` for a file-level comment
- add `--line <n>` for a line comment
- add `--end-line <n>` for a range comment
- use `--side old|new` for inline comments
- use `--username <name>` to identify the comment author; otherwise tuicr uses
  the configured `username` or `"user"`

On a `local:` pull request slug, a line or range target opens a forge thread
instead of a draft (see [Forge Threads](#forge-threads-local-pull-requests));
review- and file-level targets still become drafts.

## JSON Input

For machine input, pass a JSON payload with `--input`. The value can be literal
JSON, `@path/to/payload.json`, or `-` to read stdin.

```bash
tuicr review add --session agavra/tuicr@main/worktree --input - <<'JSON'
{
  "type": "issue",
  "content": "Handle the empty case here.",
  "file": "src/main.rs",
  "line": 42,
  "side": "new"
}
JSON
```

Flat JSON fields:

- `content`: required comment text
- `type` or `comment_type`: comment classification, defaults to `none` (untyped, no `[TYPE]` tag)
- `file`: file path; omit for a review-level comment
- `line`: line number for a line comment
- `start_line` and `end_line`: range bounds
- `side`: `old` or `new`, defaults to `new`
- `username` or `author`: comment author; uses the same fallback as
  `--username`

Nested targets are also accepted:

```json
{
  "comment_type": "suggestion",
  "content": "This range can be simplified.",
  "target": {
    "type": "line_range",
    "file": "src/main.rs",
    "start_line": 10,
    "end_line": 14,
    "side": "old"
  }
}
```

Target types:

- `review`
- `file`
- `line`
- `line_range` or `range`

## Output

`list` returns a JSON array:

```json
[
  {
    "slug": "agavra/tuicr@main/worktree",
    "kind": "local",
    "path": "/Users/alice/Library/Application Support/tuicr/reviews/sessions/9f6c1b3e09a54e2a.json",
    "updated_at": "2026-05-22T17:20:00Z",
    "comment_count": 1,
    "reviewed_count": 0,
    "file_count": 3,
    "anchor": "main",
    "active": true
  }
]
```

With `--all`, PR sessions appear alongside local ones with `"kind": "pr"` and a
PR slug:

```json
[
  {
    "slug": "gh:slatedb/slatedb/pr/1745",
    "kind": "pr",
    "path": "/Users/alice/Library/Application Support/tuicr/reviews/sessions/172e168db0d525e5.json",
    "updated_at": "2026-05-22T17:20:00Z",
    "comment_count": 0,
    "reviewed_count": 0,
    "file_count": 12,
    "anchor": "pr/1745",
    "active": false
  }
]
```

`comments` returns a JSON array:

```json
[
  {
    "id": "79c9b3e1-0a7a-4efe-9d43-f7085d7c1a82",
    "location": "src/main.rs:42",
    "path": "src/main.rs",
    "start_line": 42,
    "end_line": 42,
    "side": "new",
    "comment_type": "issue",
    "author": "alice",
    "lifecycle_state": "local_draft",
    "created_at": "2026-05-22T17:20:00Z",
    "content": "Handle the empty case here."
  }
]
```

The `author` field is the username stored with the comment. It is present in
the JSON emitted by both `review add` and `review comments`. Local thread
output stores each comment's author in `comments[].author`; `reply` and `edit`
return the affected comment with its `author`.

## Forge Threads (local pull requests)

Line and range comments on a `local:` pull request create forge threads
immediately, and submitted inline drafts also become threads. These threads
survive head changes and re-anchor to the current diff. The following commands
let scripts and agents work with them directly:

```bash
tuicr review threads --session local:owner/repo/pr/1
tuicr review reply   --session local:owner/repo/pr/1 --thread <id> \
  --username "Claude Fable" "Fixed in abc1234."
tuicr review resolve --session local:owner/repo/pr/1 --thread <id>
tuicr review resolve --session local:owner/repo/pr/1 --thread <id> --unresolve
tuicr review edit    --session local:owner/repo/pr/1 --thread <id> [--comment <id>] \
  --username "Claude Fable" "Done in def5678 instead."
tuicr review delete  --session local:owner/repo/pr/1 --thread <id> [--comment <id>] \
  --username "Claude Fable"
```

`threads` prints every thread with its id, anchor, resolution state, and
comments. Each thread's `updated_at` changes on every mutation — reply, edit,
delete, resolve, unresolve — so a poller can compare that one field per thread. `reply` appends a comment to a thread and prints it. `resolve`
flips `is_resolved` (`--unresolve` reopens). These thread commands accept only `local:`
PR slugs; other forges return an error.

`edit` replaces the body of a comment and prints it with `updated_at` set;
`delete` removes one and prints `{ thread_id, comment_id, thread_deleted }`.
Both address the root comment unless `--comment` names another, and both
require the comment to carry the `--username` author (config `username`, then
`"user"`, when omitted): amending or deleting someone else's comment is an
error. Deleting the last comment removes the thread; deleting a root that has
replies is refused, so resolve the thread or delete the replies first.

New threads come from `add`:

```bash
tuicr review add --session local:owner/repo/pr/1 --repo /path/to/checkout \
  --target-file src/main.rs --line 42 --side new --type issue \
  --username "Claude Fable" "Handle the empty case here."
```

With a line or range target on a `local:` slug, `add` opens a thread anchored
at that line (a range anchors at its last line) and prints it in the same
shape as a `threads` element, so `.id` is the thread id. It needs the checkout
in `--repo` (default `.`) to snapshot the line's text against the pull request
diff: a `--repo` that is not a directory, or a checkout of a different
repository, is an error, and the file must be part of the diff. Threads
created this way have `"review_id": null`; threads created by `:submit` carry
their review's id. No session file is touched.

The `[TYPE]` prefix in a new thread's body uses the configured comment type
`label`, uppercased, matching upstream's submitted reviews. `--type` still
takes the type ID. With no configured label, the ID is uppercased; type `none` and
`[forge] comment_type_prefix = false` omit the prefix. Existing thread bodies
keep their stored text when the configuration changes.
