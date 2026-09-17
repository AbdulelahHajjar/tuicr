# Command Line Reference

Every flag and subcommand `tuicr` accepts, plus the environment variables it
reads and the output it produces for scripts and agents. `tuicr --help` is the
same list in condensed form.

Related references: [CONFIG.md](CONFIG.md) for the config file and themes,
[KEYBINDINGS.md](KEYBINDINGS.md) for keys and `:` commands inside the TUI, and
[REVIEW_CLI.md](REVIEW_CLI.md) for the `tuicr review` subcommands in full.

## Commands

| Form | What it does |
|---|---|
| `tuicr` | Open the TUI on the review target selector |
| `tuicr tui` | Same TUI, explicit subcommand |
| `tuicr pr [<target>] [--base <ref>]` | Review the current branch, another local branch, or a forge pull request |
| `tuicr mr <target>` | Review a forge merge request; requires a target |
| `tuicr tui pr [<target>]`, `tuicr tui mr <target>` | Same, under the explicit TUI subcommand |
| `tuicr review list` | List persisted review sessions |
| `tuicr review add` | Add a session draft, or open a Local pull request thread for a line/range target |
| `tuicr review comments` (`tuicr review get`) | Print a session's comments |
| `tuicr review threads` | Print a Local pull request's stored threads |
| `tuicr review reply` | Reply to a Local thread |
| `tuicr review resolve` | Resolve a Local thread; `--unresolve` reopens it |
| `tuicr review edit`, `tuicr review delete` | Edit or delete your own Local thread comment |
| `tuicr update [VERSION]` | Exit with fork-specific update guidance |

`get` is an alias of `comments`. Both `pr` and `mr` accept a bare `<number>`,
`<owner/repo#N>`, or a forge PR URL. `pr` also accepts a local branch name;
omitting its target reviews the current branch as a Local pull request.
`mr` requires a forge target and does not accept `--base`.

For a Local pull request, `--base <ref>` overrides the default branch and
records that base for the pull request. It must name a reference, such as
`main` or `origin/main`; moving revision expressions such as `HEAD~2` are
rejected. Combining `--base` with a forge target is an error. See
[LOCAL_FORGE.md](LOCAL_FORGE.md) for default-base selection and persistence.

This fork blocks `tuicr update`, including an optional version argument, so
an upstream release cannot replace the Local workflow. It directs you to
`tuicr-fork-update`, a separately managed helper that this repository does not
bundle. Install from the fork's `local-forge` branch as described in
[README.md](../README.md#install).

There is no `tuicr help` subcommand. Use `-h` / `--help`, which also works per
command (`tuicr review add --help`).

## Flags

These apply to the TUI and can be given on the bare command, on `tui`, or on
`pr` / `mr`. When the same option appears at more than one level, string values
take the last one given and boolean flags stay on once set, so
`tuicr --stdout tui pr 125 --theme nord-dark` is valid.

| Flag | Short | Value | Description |
|---|---|---|---|
| `--revisions` | `-r` | `REVSET` | Commit range / revset to review. Syntax depends on the VCS backend (`main..HEAD` for git, a jj revset for jj) |
| `--theme` | | `THEME` | Color theme. Bundled names resolve first, then `<config>/themes/<name>.toml` |
| `--appearance` | | `light`, `dark`, `system` | Appearance mode, used when no explicit theme is set |
| `--path` | `-p` | `PATH` | Filter the diff to a file or directory |
| `--working-tree` | `-w` | | Include uncommitted changes and skip the target selector |
| `--file` | | `PATH` | Open a file or directory for annotation with no VCS required |
| `--all-files` | `-A` | | Review every tracked file in the current repo |
| `--stdout` | | | Print the export to stdout instead of copying to the clipboard |
| `--no-update-check` | | | Skip the startup update check (same as `no_update_check` in the config) |
| `--repo-url` | | `URL` | Override the forge repo for PR operations |
| `--remote` | | `NAME` | Use a named VCS remote's fetch/pull URL for PR operations; conflicts with `--repo-url` |
| `--version` | `-V` | | Print the version and exit |
| `--help` | `-h` | | Print help and exit |

`--repo-url` accepts GitHub, GitLab, Gitea, Bitbucket, Azure DevOps, and Gerrit repos in HTTPS,
SCP-style SSH, or `ssh://` form — for example
`https://github.com/owner/repo`, `git@gitlab.com:owner/repo`, or
`https://dev.azure.com/org/project/_git/repo`. It is what to reach for when the
checkout's `origin` remote does not point at the forge repo you want to review
against.

If the repository is already configured as a remote, use its name instead:

```bash
tuicr pr 382 --remote staging-upstream
```

`--remote` discovers a Git repository from the current directory, including
subdirectories and colocated Jujutsu workspaces. Remote lookup does not run `jj`.
Non-colocated Jujutsu and Mercurial workspaces can use `--repo-url` instead.

- Git uses `git remote get-url <name>` and honors `url.<base>.insteadOf` rewrites.

The fetch/pull URL is used, not a push URL. A missing remote or unrecognized
forge URL is an error, not a fallback to another remote. The URL must identify
a repository on one of the supported forges.

Both `--remote` and `--repo-url` select the repository directly, without a fork-parent
lookup. They cannot be combined. When `--remote` is supplied, its lookup must
succeed before a full PR URL or `owner/repo#N` target takes precedence. Omit
`--remote` when using an explicit target outside a Git checkout.

These options select a remote forge. Local pull requests keep their checkout's
repository identity and use `--base` to select the comparison reference.

### Scope selection

`--path`, `--working-tree`, `--file`, and `--all-files` pick what gets reviewed,
so they cannot be combined — passing two is a usage error. `--file` also
conflicts with `--revisions`, since annotating an arbitrary file has no commit
range.

`--path` on its own implies `--working-tree`. Pair it with `-r` to filter a
commit range instead:

```bash
tuicr -p src/cli.rs                 # uncommitted changes to one file
tuicr -r main..HEAD -p src/cli.rs   # that file's changes across a range
```

`--revisions` accepts leading hyphens, so revsets like `-r -3` reach the VCS
rather than being read as another flag.

TUI flags cannot be mixed with `review` or `update`, which do not open a TUI:

```bash
tuicr --stdout review list
# error: TUI options cannot be used with `tuicr review`; run `tuicr review --help` for command options
```

## Environment variables

No flag reads an environment variable as a fallback — anything below either
configures something with no flag equivalent, or is read by a tool tuicr shells
out to.

| Variable | Effect |
|---|---|
| `EDITOR` | Editor used by `e` and `:edit`. The config `editor` setting wins; `vi` is the last resort |
| `XDG_CONFIG_HOME`, `HOME`, `APPDATA` | Locate the config directory. See [CONFIG.md](CONFIG.md) |
| `TUICR_PROFILE` | Write a startup/render profile log. See below |
| `TUICR_PROFILE_FILE` | Path for that log, overriding the default location |
| `AZURE_DEVOPS_EXT_PAT`, `AZURE_DEVOPS_PAT` | Azure DevOps PAT. Checked in that order; without one tuicr falls back to `az rest`. See [AZURE.md](AZURE.md) |
| `GERRIT_URL` | Gerrit server root; also identifies Gerrit remotes on otherwise neutral hostnames. See [GERRIT.md](GERRIT.md) |
| `GERRIT_USERNAME`, `GERRIT_PASSWORD` | Gerrit REST API credentials. `GERRIT_PASSWORD` must be an HTTP password, not the account password |
| `TUICR_GLAB_DEBUG` | Log `glab` invocations for debugging. See [GITLAB.md](GITLAB.md) |
| `TMUX`, `ZELLIJ`, `SSH_TTY` | Detected to choose a clipboard path — OSC 52 over SSH, `tmux load-buffer` inside tmux |
| `XDG_SESSION_TYPE` | Picks `wl-copy` on Wayland or `xclip` on X11 |

Set `TUICR_PROFILE` to `1`, `true`, `on`, or `yes` to log to
`tuicr-profile.<timestamp>.log` in the system temp directory. Any other
non-empty value is treated as the log path itself; `0`, `false`, `off`, `no`,
and an empty value disable profiling. `TUICR_PROFILE_FILE` overrides the path
either way, but only when `TUICR_PROFILE` is enabled.

## Output for scripts and agents

Two markers go to **stderr**, so they survive on the user's scrollback and stay
out of piped output:

```
tuicr-session: agavra/tuicr@main/worktree
tuicr-summary: reviewed 3/3 files, 2 comments added
```

`tuicr-session:` is printed once at startup, before the alternate screen is
entered, and names the session slug. Pass it straight to
`tuicr review comments --session <slug>` to read comments while the TUI is
still open, or after it exits. `tuicr-summary:` is printed on exit, always —
including with zero comments, so a watcher can tell "reviewed everything, found
nothing" apart from "quit without looking".

`--stdout` prints the export markdown to stdout when the TUI exits, with the
TUI itself rendered to `/dev/tty`. Combined with the two stderr markers, that
keeps stdout clean for a pipe.

All `tuicr review` subcommands print pretty JSON to stdout and nothing
else. There is no `--json` flag because JSON is the only format. See
[REVIEW_CLI.md](REVIEW_CLI.md) for the schemas.

`tuicr update` prints the fork update guidance to stderr and exits with code 1.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success, including `--help` and `--version` |
| `1` | Startup failure (not a repository, forge auth, missing PR, bad `--file` path) or a failed `review` / `update` command |
| `2` | Usage error (unknown flag, conflicting flags, invalid `--appearance` value) or a theme that could not be resolved |

## Not provided

tuicr ships no man page and no shell completions. `tuicr --help` and this page
are the reference.
