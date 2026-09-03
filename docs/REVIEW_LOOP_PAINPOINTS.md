# Review-loop requirements (agent handoff)

Context: a live collaborative review loop on a `local:` pull request — the user reviews in the
tuicr TUI, an agent addresses each comment, replies inline, and resolves threads. Requirements
distilled from a real session on `local:HudHud-Maps/hudhud-ios/pr/1` (2026-09-02/03). Each item
states an invariant; suggested directions are non-binding — satisfy the invariant any way that
fits the codebase. Keep in step: `docs/LOCAL_FORGE.md`, `docs/REVIEW_CLI.md`, `docs/CONFIG.md`,
`docs/KEYBINDINGS.md`, and the LOCAL_FORGE verification contract.

Stores involved:
- Sessions: `~/Library/Application Support/tuicr/reviews/sessions/*.json`
  (`pr_session_key` includes `head_sha`)
- Local forge: `~/Library/Application Support/tuicr/local-forge/<owner>__<name>/`
  (`pulls.json`, `pulls/<n>/reviews.json`, `pulls/<n>/threads.json`)

Design decision (see appendix for the incidents behind it): **the `local:` review flow is
draft-free.** Comments from either party go straight to forge threads. The draft/batch
machinery is an upstream feature for remote forges (network round-trips, one coherent
notification for a human recipient) — do not remove it; bypass it for `local:` PRs. This keeps
the fork's diff against upstream small.

## Must

### M1. Direct-to-thread comments on local PRs

Status (2026-09-03): implemented — see `docs/LOCAL_FORGE.md` § Direct-to-thread comments and
`docs/REVIEW_CLI.md` § Forge Threads.

Invariant: on a `local:` pull request, creating a comment produces a durable forge thread in a
single action, authored by whoever wrote it, with no session-draft intermediary.

- TUI: the user saves a comment and it is immediately a thread (author = configured
  `username`). No separate submit step for inline comments.
- CLI: an agent can create a **new** thread at `file:line` (today `review reply` handles only
  existing threads; `review add` creates a draft — the gap is thread creation), with
  `--username` carried onto the thread comment.
- Streaming N comments must not produce meaningfully heavier artifacts than a batch would
  (one logical review per pass is acceptable; N session files or N heavyweight review records
  are not; making `review_id` optional on threads is equally acceptable).
- Review verdict events (`:submit approve` / request-changes / comment-with-body) remain as
  they are — they are about the verdict, not inline comments.

## Good to have

### G1. Change-observation primitive

Status (2026-09-03): partly covered — every thread carries an `updated_at` that moves on any
mutation (reply, edit, delete, resolve, unresolve), so a poller compares one field per thread.
No blocking watch or cursor yet.

Invariant: an agent can ask "what changed since X?" for a PR slug and get an answer that is
stable across session rotations and cannot race a concurrent writer.

Suggested: `tuicr review watch --session <slug> [--since <cursor>]` blocking until
threads/comments/reviewed marks change, printing the new cursor; or a cursored aggregate query.
Mitigation today: ledger-diff polling (no baselines) solves the race client-side.

### G2. Consistent CLI flags, loud errors

Invariant: one consistent flag surface across `review` subcommands (accept `--repo` everywhere
or nowhere, documented); errors on stderr with nonzero exit; JSON only on stdout.

Observed: `list/add/comments` accept `--repo`, `threads/reply/resolve` do not; a uniform script
got a clap error that a lenient pipeline read as "zero threads", blinding a watcher for hours.

### G3. Threads output exposes current anchor state

Invariant: `review threads` shows each thread's current (reanchored) line against the present
head and whether it is outdated — today only `original_line`/`original_commit` are printed.

### G4. Edit or retract a thread comment

Status (2026-09-03): implemented — see `docs/LOCAL_FORGE.md` § Editing and deleting threads and
`docs/REVIEW_CLI.md` § Forge Threads.

Invariant: the author of a thread comment can amend or delete it on a local PR.

Why: drafts were the revise-before-send grace window; direct-to-thread (M1) removes it. A
correction-by-reply works but litters the thread.

## Appendix: incidents that motivated the draft-free decision

Kept for context only — these are NOT requirements; M1 makes the whole class impossible.

- Sweep + authorship flattening: the user's `:submit comment` consumed every session draft,
  including replies the agent had added with `review add --username "Claude Fable"`, and
  `create_review` stamped them all with the user's name — the permanent record showed the user
  answering their own comments (threads `a4152f22`, `ab4806a7` on pr/1).
- Rotation loss: `pr_session_key` includes `head_sha`; each agent commit plus a TUI reload
  minted a new session file that carried drafts only for files the commit did not touch. A real
  user comment vanished from the active view and was recovered only by sweeping old session
  JSONs.
- Identity discontinuity: submission minted fresh thread/comment UUIDs with no link to draft
  ids, forcing agents to re-triage their handled-work ledger after every submit.
