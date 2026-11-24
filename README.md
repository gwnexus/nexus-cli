# Nexus CLI

[![CI](https://github.com/gwnexus/nexus-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/gwnexus/nexus-cli/actions/workflows/ci.yml)
[![Release](https://github.com/gwnexus/nexus-cli/actions/workflows/release.yml/badge.svg)](https://github.com/gwnexus/nexus-cli/releases)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)

CLI for the [Gatewarden Nexus](https://nexus.gatewarden.eu) platform. Provides
project scaffolding, agent file synchronization, workspace management,
environment preflight checks, and configuration management for multi-agent
engineering workflows.

## Install

### One-liner (recommended)

```bash
curl -fsSL https://nexus.gatewarden.eu/install.sh | bash
```

The installer detects your platform (macOS/Linux, x86_64/aarch64), downloads
the pre-built binary from GitHub Releases, and verifies its SHA-256 checksum
(mandatory: without a published checksum or a `sha256sum`/`shasum` tool it
does not install the binary). If no binary exists for your platform it falls
back to `cargo install --git`. `nexus upgrade` verifies the install script's
checksum before it runs it, from both sources.

The installer also writes the manual pages next to the binary
(`~/.local/bin` -> `~/.local/share/man/man1`), where `man` finds them through
`PATH`. Set `NEXUS_MAN_DIR` for another directory or `NEXUS_NO_MAN=1` to skip
them.

### From source

```bash
cargo install --git https://github.com/gwnexus/nexus-cli.git nexusctl
```

Requires Rust >= 1.85.

### Pin a version

```bash
NEXUS_VERSION=v0.6.12 curl -fsSL https://nexus.gatewarden.eu/install.sh | bash
```

## Quick Start

```bash
nexus login              # Authenticate with the Nexus platform
nexus init               # Initialize a Nexus project workspace
nexus preflight          # Verify environment readiness
nexus pull               # Sync skills and agent files from the platform
nexus reload             # Same, while a session runs; says what it picks up live
```

## Commands

```
nexus init [path]                       Initialize a Nexus project workspace
nexus login                             Authenticate with the Nexus platform
nexus logout                            Remove stored credentials
nexus status [--json]                   Auth, project, devbox shell, environment, and every non-clean file with its next action
nexus status --agents [--watch] [--json] Local agent fleet view (main agent, subagents, background, state, tool, runtime)
nexus doctor claude [--fix]             Check the host tools and clipboard of the Claude Code workspace, offer fixes
nexus diff [path]                       Unified diffs for non-clean files (content and projection)
nexus link [--project-id <id>]          Bind a project to the current workspace (picker: type to filter, arrow keys)
nexus unlink                            Remove project binding from the workspace
nexus code-report [--dry-run]           Send the repo's code signals (marker files, extension counts) to the hub
nexus project link [--runtime-id <n>]   Issue a project inference token (nxs_proj_*) for gateway auth
nexus project rotate [--finalize]       Rotate the project inference token (zero-downtime overlap)
nexus project unlink                    Revoke the project inference token and clear local state
nexus project status                    List issued project inference tokens
nexus pull [--project-id <id>]          Pull skills and config from the Nexus platform
nexus pull --dry-run                    Preview devbox workspace changes (incl. removals) without writing
nexus reload [--force] [--trust]        Pull and update plugins while the session runs; says what the session picks up live
nexus pull --trust                      Accept new or changed executable content without asking
nexus pull --untrack-shadowed           Untrack excluded paths git still tracks (kept on disk, not committed)
nexus push [path] [--name "..."]        Push content changes (agent files; devbox changes as a new fork)
nexus reset [path]                      Discard local changes (content: backend version; projection: desired)
nexus stash save                        Save modified content files to a stash
nexus stash pop                         Restore the most recent stash
nexus stash list                        List all available stashes
nexus env [get [key]]                   Show the project's backend settings (alias: nexus settings)
nexus env keys                          List settable keys, types and allowed values
nexus env set <key> <value> [--pull]    Change a backend setting (admin), optionally pull afterwards
nexus env set <key>=<value> [--pull]    Same, as one argument
nexus env set                           Pick key and value interactively (schema-driven)
nexus classification [show|levels]      Project, customer and effective confidentiality level and what it drives
nexus classification set <level>        Change the project level after an impact plan (--reason, --dry-run, --pull)
nexus run [-- <args>]                   Start the agent environment the backend selects
nexus run --actor-panes                 Add an actors tab with one pane per local-lane actor (zellij)
nexus run --dispatch-pane               Add a Dispatch status pane to the CLAUDE tab (zellij; default with dispatch automation)
nexus dispatch status [--watch]         Incoming and outgoing Dispatches of the project (--compact, --json)
nexus dispatch runner register          Register this machine's Dispatch runner for the project's hub (notify mode)
nexus dispatch runner status|pause|resume|revoke|probe   Manage the runners (one per hub); probe checks Claude Code
nexus dispatch listen                   Run the Dispatch runner in the foreground (notices into sessions)
nexus run --resume                      Resurrect the project's exited zellij session instead of starting fresh
nexus actors run <slug> [-p <prompt>]   Start a local-lane actor as its own Claude Code session on the gateway
nexus actors validate|import <path>     Check or import actor profiles (flat or hub export frontmatter)
nexus actors export --target claude     Print the Claude Code agents and lane actors as JSON
nexus skills export [--project-id <id>] Export enabled skills as JSON
nexus preflight                         Run environment readiness checks
nexus deinit [--force]                  Remove the files Nexus wrote (edited ones stay)
nexus shadow on|off [--target T,...]    Hide (or stop hiding) agentic files, the workspace and Nexus internals from Git
nexus shadow status                     Effective state per target, excluded paths, hook and check summary
nexus shadow check [--commits N]        Scan tracked files and recent commit messages with the shadow rules
nexus git verify                        Compare local git config and the GitHub account with the project
nexus git apply [--canonical-remotes]   Apply git identity, .nexus/gh.env and the account's transport identity
nexus git login [--account <login>]     Log the project's GitHub account into its own gh profile
nexus config show                       Display effective configuration (with provenance)
nexus config set K=V [--local|--global] Update a configuration value
nexus config path [--local|--global]    Show a config file path
nexus upgrade                           Upgrade CLI to latest release version
nexus man [command] [--out-dir DIR]     Print a manual page, or write all of them
```

### Help and manual pages

`-h` prints a short summary with one line per option; `--help` adds the
details. Global options (`--api-url`, `--output`, `--verbose`, `--yes`) are
listed in their own section, and long lines wrap at the terminal width.

The manual pages are generated from the same text, so they always match the
binary:

```bash
nexus man run | man -l -                      # read one page (Linux)
nexus man run | mandoc -a                     # read one page (macOS)
nexus man --out-dir ~/.local/share/man/man1   # install all pages: man nexus-run
```

### Global Flags

| Flag         | Short | Description                                     |
| ------------ | ----- | ----------------------------------------------- |
| `--yes`      | `-y`  | Non-interactive mode (auto-confirm all prompts) |
| `--verbose`  | `-v`  | Enable verbose output                           |
| `--help`     | `-h`  | Show help (`-h` short, `--help` with details)   |
| `--version`  | `-V`  | Show version                                    |

### Notable Flags

| Command         | Flag             | Description                                    |
| --------------- | ---------------- | ---------------------------------------------- |
| `nexus init`    | `--yes`          | Skip interactive project selection             |
| `nexus deinit`  | `--force`        | Skip confirmation prompt                       |
| `nexus deinit`  | `--yes`          | Auto-confirm removal                           |
| `nexus link`    | `--project-id`   | Specify project UUID directly (skip picker)    |
| `nexus link`    | (none)           | Type to filter, pick with ↑/↓ and Enter, Esc cancels; numbered list without a terminal |
| `nexus project link` | `--runtime-id` | Logical runtime name for the token (default: hostname) |
| `nexus project link` | `--expires`  | Token lifetime: 30d, 12h, 2w, or an ISO 8601 timestamp |
| `nexus project link` | `--restrict-profiles` | Restrict the token to profile slugs (comma-separated) |
| `nexus project rotate` | `--finalize` | Revoke the previously superseded token after overlap |
| `nexus pull`    | `--project-id`   | Pull from a specific project (skip picker)     |
| `nexus pull`    | `--dry-run`      | Preview the selected `--scope` without writing: devbox workspace changes (create, update, keep, remove, executable-bit repairs `MODE +x`), skill removals, the task snapshot, and the executable items pull would ask about |
| `nexus pull`    | `--scope`        | Limit the pull to `skills`, `directives`, `agents`, `tasks` or `workspace` (repeatable, comma-separated); see [Pull scopes](#pull-scopes) |
| `nexus pull`, `nexus init` | `--trust` | Accept new or changed executable content and activate the repository git hooks without asking (not implied by `--yes` / `--force`) |
| `nexus push`    | `--name`         | Custom fork name (default: auto-generated)     |
| `nexus push`    | `--dry-run`      | Show what would be pushed without sending      |
| `nexus push`    | `--workspace`    | Only push workspace files (default for now)    |
| `nexus push`    | `--adopt-local`  | Publish local workspace as a new fork without a prior pull |
| `nexus config set` | `--local`     | Write to project-local `.nexus/config.toml` instead of global |
| `nexus config path` | `--local`   | Print the project-local config path instead of global |

### Project Inference Tokens (`nexus project link`)

The Nexus Model Gateway authenticates client inference traffic (`POST /v1/chat/completions`)
with a project-scoped token (`nxs_proj_*`), separate from your user PAT
(`nxs_pat_*`). `nexus project link` bootstraps such a token from your PAT and
stores it for tools like OpenCode to use.

```bash
nexus project link                         # issue a token for the linked project
nexus project link --runtime-id ci-prod    # name the logical runtime
nexus project link --expires 30d           # set a lifetime (30d / 12h / 2w / ISO 8601)
nexus project rotate                        # rotate with zero-downtime overlap
nexus project rotate --finalize             # revoke the previous token after overlap
nexus project status                        # list issued tokens (never prints secrets)
nexus project unlink                        # revoke and clear the local token (run again to retry unconfirmed revocations)
```

**Existing links:** run `nexus project link` in any already-linked workspace.
It reuses the project from `.nexus/config.toml`, so no relink is needed.

**Storage and exposure.** The raw token is returned by the API exactly once. It
is stored in `~/.config/nexus/project-tokens.toml` (mode `0600`, never
committed) and written to the gitignored `.env.nexus.local` as
`NEXUS_PROJECT_TOKEN`, which `nexus run` and OpenCode load automatically. In
CI, set `NEXUS_PROJECT_TOKEN` directly in the environment; it always takes
precedence over the local store. The PAT remains the long-lived bootstrap
credential; project tokens are shorter-lived and rotate independently.

`.env.nexus.local` is added to `.git/info/exclude` before the token is
written (owner-only, in one step); when git tracks the file, the command
stops and names the fix instead. `rotate` and `unlink` change the
workspace file only when the workspace is linked to the selected project,
and `rotate --finalize` refuses while the workspace still holds the previous
token.

### Workspace Files: Status, Diff, Push, Reset, Stash

One command per activity; each classifies every file itself:

- **content**: authored files that may go back to the backend: assigned
  agent files (e.g. `.nexus/AGENTS.md`) and the devbox workspace
  (`devbox.json`, `scripts/devbox/**` except the generated scripts below).
  States: MODIFIED, NEW, DELETED, UPDATE (backend changed), CONFLICT.
- **projection**: generated from backend settings; a local edit is drift:
  Claude Code Experience files, generated agent files, skills and OpenCode
  commands, the `CLAUDE.md` managed block, managed `.claude/settings.json`
  keys and the devbox scripts the backend generates for every workspace
  (`scripts/devbox/dbx_init.sh`, `dbx_post_init.sh`, `lib/common.sh`,
  `ops/*.sh`; put local changes into `scripts/devbox/init/os_<os>/override.sh`
  or an own script). States: DRIFTED, CONFLICT, UPDATE, CREATE, ORPHANED,
  UNMANAGED.
- Leftovers of the runtime the project does not use are reported as STALE
  with "run nexus pull to remove it" (see below); unmanaged files are never
  touched.
- Workspace files the backend never delivers or accepts are reported as
  RETIRED with a `git rm` hint: the tmux launchers `scripts/devbox/dbx_tmux_2w.sh`
  and `dbx_tmux_3w.sh` that older workspaces shipped, and an
  `scripts/devbox/init/os_<os>/override.sh` whose lines are all empty or
  comments, as long as the workspace fork does not carry them. They are not
  pending changes, and `nexus push` skips them. An override with at least
  one command line is NEW content as usual. An override the fork carries
  and that you reverted to the template is MODIFIED: `nexus push` sends it,
  and the new fork no longer carries it.
- `devbox.json` and scripts that differ only in trailing newlines (or, for
  JSON, in key order) count as unchanged (the backend sends some without a final newline, end-of-file
  fixers commit them with one); pull writes them with exactly one.

```bash
nexus status                        # overview with the next action per file (exit 1 if pending)
nexus diff .claude/rules            # diffs for a file or directory
nexus push .nexus/AGENTS.md         # push one content file
nexus push                          # push all modified content
nexus reset .claude/rules/10.md     # restore one file (content: backend, projection: desired)
nexus env set claude.hud minimal --pull   # change the setting a projection comes from
nexus env set                             # or pick the setting and value interactively
```

`nexus push` refuses projection files and names the setting to change
instead. Devbox changes become a new workspace fork; push compares the local
files with the same fork export `nexus status` uses, so a freshly pulled
workspace has nothing to push. New local scripts are pushed when named
(`nexus push scripts/devbox/<file>`). A fork script you deleted locally
(`DELETED`) is pushed as a removal (`D`): the new fork is built from the
pushed files, so it no longer carries the script. A named path pushes only
the devbox files it selects (`nexus push scripts/devbox/a.sh`): every other
file keeps its version from the current fork, so local edits elsewhere stay
local; when the fork cannot be read, such a push stops. A full `nexus push`
stops too, with nothing published, when the current fork cannot be read
(only "no workspace fork yet" goes on), or when a workspace file it would
send (tracked, named or in the fork) cannot be read; generated scripts are
never pushed, and files the fork dropped are skipped unless named. A file
unchanged locally whose fork version is newer goes into the new fork with
the fork version. If the local record of a pushed file changed while the
push ran (another pull or push), push says so and suggests `nexus pull`.
`nexus reset` of a
deleted executable script writes it back executable, and a leftover Claude
Code file that cannot be removed keeps its ownership record:

```bash
nexus push --name "v3.1 ansible"    # push with custom fork name
nexus push --dry-run                # show what would be pushed
```

`nexus pull` removes devbox workspace files it wrote earlier that the fork
no longer contains, if they are unchanged locally (`REMOVE`); locally
modified ones are kept and reported (`KEEP`), and files Nexus never wrote are
not touched. `nexus status` lists them as `ORPHANED` or `DROPPED`, and
`nexus pull --dry-run` previews the workspace changes (with `--scope
workspace` or no scope), also when the legacy workspace export is used.
A file is removed only in the state the plan approved: one edited or
re-recorded in the meantime is kept, and an unreadable one is named with
its error and checked again on the next pull. `nexus push` skips
files the current fork no longer contains, so a new fork does not bring them
back by accident; `nexus push <path>` pushes one back on purpose.

Skills follow the same rules: files of a skill that is no longer exported
(renamed, unassigned, or disabled in Nexus; the line then says
`disabled in Nexus`) are removed on `nexus pull` if unchanged
(`REMOVE`), kept and reported if edited (`KEEP`), listed by `nexus status`
and previewed by `nexus pull --dry-run`. Only the selected runtime's
directories are pruned (`<agentic_root>/skills/`, plus `.claude/skills/` or
`.opencode/commands/`). The `name` in each `SKILL.md` frontmatter is the
skill directory id (e.g. `nexus-code-review`).

The project's plugin variables (e.g. `HEADROOM_*`) are written to
`.nexus/env` (used by `nexus run`) and, as a shell-sourceable file, to
`.nexus/plugin.env`, which the devbox init hook sources (`set -a; . .nexus/plugin.env`).
Values there are single-quoted shell words and keys are valid variable names, so sourcing
the file never runs anything.

Each push **archives the current active fork** and creates a new one. The
fork name is either auto-generated (`push-2026-08-23T10-30-00`) or set
via `--name`. The push plan (also with `--dry-run`) names the new fork and
the fork that gets archived. Previous forks remain accessible in the
dashboard and can be activated again.

`nexus push` requires a prior `nexus pull --scope workspace` (it tracks
which files came from Nexus via `.nexus/sync-manifest.json`, and refuses to
push untracked files). The sync manifest and the pull manifest are updated
under a lock (a `<file>.lock` file next to them), so concurrent `nexus`
commands keep each other's records; a manifest locked for more than about
five seconds stops the command with "locked by another nexus command; try
again", and a manifest that cannot be parsed is reported, never reset.
Only one `nexus pull` runs per workspace at a time (a lock on
`.nexus/pull.lock`); a second one stops with "another nexus pull is running
in this workspace". If the local workspace was never pulled, or you need
to publish local edits without risking an older server-side fork
overwriting them, use `nexus push --adopt-local` to bootstrap a new fork
directly from the current local files, bypassing that check:

```bash
nexus push --adopt-local --dry-run   # preview what would be adopted
nexus push --adopt-local             # publish current local workspace as a new fork
```

`nexus push` also prints the effective backend (`Backend: <api_url>`)
alongside the project ID, so a wrong `NEXUS_API_URL` / `--api-url` (project
not on this environment) is diagnosable in one step, including under
`--dry-run`.

`nexus stash` provides temporary local backup of modified content files
(devbox workspace and agent files; projection files are never stashed)
before a `nexus pull --force`:

```bash
nexus stash save    # save modified content files to .nexus/stash/
nexus stash pop     # restore the most recent stash
nexus stash list    # show all available stashes
```

Stashes are stored locally in `.nexus/stash/<timestamp>/`: the saved files
under `files/`, the metadata in `stash.json` next to them. Two saves in the
same second get separate directories (`<timestamp>-001`, ...). Multiple
stashes can coexist; `pop` always restores the most recent one, makes a
saved executable script executable again, and removes the stash only once
every file is back. When a file cannot be restored (it is missing from the
stash or its path is unsafe), `pop` changes nothing, keeps the stash and
names the files.

### Pull scopes

`nexus pull --scope <scope>` (repeatable, comma-separated) limits a pull to
some parts; without it, every part is pulled.

| Scope | What it writes |
|-------|----------------|
| `skills` | Skill files under the agentic root, OpenCode slash commands, Claude Code's `.claude/skills/` copies, and the removal of skills no longer exported |
| `directives` | `<agentic_root>/directives.md` |
| `agents` | Agent files, actors, runtime configs (`opencode.json`, `.mcp.json`), the Claude Code projection, the shadow guard, plugins, the git identity and GitHub account, and the cleanup of the unused projection |
| `tasks` | The `<agentic_root>/TASKS.md` snapshot (cleared when no task is open, only if Nexus wrote it) |
| `workspace` | devbox workspace files and machine-local scripts |

The server's git excludes are applied with `agents` and with `workspace`.
`--dry-run` previews the same scopes.

### Generated `opencode.json`

`nexus pull` / `nexus init` write the Nexus MCP server config into
`opencode.json`'s `mcp.nexus` block:

```json
{
  "mcp": {
    "nexus": {
      "type": "local",
      "command": ["npx", "--yes", "@gwdn/nexus-mcp@latest"],
      "environment": {
        "NEXUS_API_URL": "https://nexus.gatewarden.eu",
        "NEXUS_PRIVATE_TOKEN": "nxs_pat_...",
        "NEXUS_PROJECT_ID": "11111111-2222-4333-8444-555555555555"
      }
    }
  },
  "instructions": [".nexus/AGENTS.md"]
}
```

- **`NEXUS_PROJECT_ID`** gives the MCP server (and agents reading its
  environment) an unambiguous project binding, so a session can't silently
  land in the wrong project by guessing from a project-listing tool and
  name/slug matching.
- **`instructions`** is merged additively from the platform (typically
  `<agentic_root>/AGENTS.md`), so OpenCode loads the project's agent policy
  deterministically at session start: no dependency on OpenCode's own
  upward `AGENTS.md` auto-discovery, which has no awareness of the
  `agentic_root` convention (default `.nexus/`). Pre-existing custom entries
  in `instructions[]` are preserved; re-running `pull` does not duplicate
  entries already present.

Without `--force`, pull updates only the keys Nexus writes and keeps the
rest of the file (theme and other settings). A changed default model or
agent is reconciled on the next pull. An `opencode.json` that cannot be
parsed is left alone with a warning (fix it or pull with `--force`); the
Claude Code projection is then not cleaned up, so the workspace keeps a
working configuration. `-y` does not rebuild the file; `--force` or
`--force-unmanaged` does.

#### Model-routing warnings

If the backend detects a divergence between the project's model routing and
what the generated `opencode.json` can actually serve (e.g. an agent's model
uses a provider Nexus can't verify, or a route-alias migration hasn't landed
on this environment), `nexus pull` renders it before writing `opencode.json`:

```
   ! Nexus detected 3 model-routing warning(s) for this project:

   • unverifiable_provider
     Agent nexus-plan uses provider "github-copilot", which Nexus does not
     declare in this opencode.json and cannot verify.
     hint: Ensure "github-copilot" is configured in your own OpenCode
     config, or retarget this agent to a Nexus-managed provider.

   ? Continue anyway? [y/N]
```

- Interactive sessions get a single-keypress `[y/N]` prompt (default `N`).
  Declining skips only the `opencode.json` / `<agentic_root>/mcp.json`
  write; the rest of the pull (skills, directives, actors, etc.) still
  completes.
- `--yes` (or `--force`) bypasses the prompt and proceeds.
- Non-interactive sessions (no TTY, e.g. CI) never block on this: warnings
  are printed and the pull proceeds automatically.

### Status

`nexus status` shows auth, workspace, and project state (with real
server-side validation, not just a local echo), then the agent environment
(what `nexus run` starts), the Claude Code runtime (version vs. the CCX
compatibility range, enabled plugins with `[managed]` marker and install
scope, last headroom summary), git identity, and the
workspace file list described above. The `Workspace:` line ends with the
devbox shell state: `[devbox shell: active]` inside this workspace's devbox
shell, `[devbox shell: active for <root>]` inside another project's shell,
or `[devbox shell: inactive, start with devbox shell]` when the workspace
has a `devbox.json` but no shell is active (JSON: `devbox_shell`). `--output json` is supported; the exit
code is 1 when changes are pending.

The file list compares against what `nexus pull` would write, with the
same choices: the runtime from the agent-file export, then the project
detail, then the cached owner; the same agentic root; for Claude Code the
native skill files under `.claude/skills/` as well; and the legacy
workspace export when the fork export fails (no fork: only local changes
against the last pull). When the backend version of a workspace file
cannot be fetched, `nexus diff` prints "backend version unavailable" for a
locally changed file instead of a diff against an empty file.

```bash
nexus status
```

```
>> Nexus Status

General

  API URL:         https://nexus.gatewarden.eu (global)
  Workspace:       /path/to/project  [devbox shell: active]

  Auth:            OK you@example.com (member)
                   Token: nxs_pat_****9f3c (global)

  Project:         OK My Project (my-project)
                   ID: 11111111-2222-4333-8444-555555555555

  Environment:     Claude Code (workspace zellij) (start with nexus run)

Claude Code runtime

  Version:         2.1.296 (supported: >=2.1.257 <3.0.0)
  Plugin:          code-review@claude-plugins-official [managed]: installed (project scope, by nexus)

Git Identity Verification

  user.name        local=dev                         [OK] expected=dev
  user.email       local=dev@example.com             [OK] expected=dev@example.com
  user.signingkey  local=<signing key fingerprint>   [OK] expected=<signing key fingerprint>
  commit.gpgsign   local=true                        [OK] expected=true

All git identity settings match.

OK Workspace is clean.
```

All sections share one key column. In the git identity rows the status sits
between the local and the expected value, aligned across rows, so it stays
visible next to long values such as signing keys (`nexus git verify` uses
the same layout).

- **API URL** reports which layer supplied it (`flag`, `env`, `local`,
  `global`, `default`): see [Configuration](#configuration) below.
- **Auth** verifies the token against the API (unchanged).
- **Project** now makes a real API call to confirm the linked project
  exists and is reachable at the effective `api_url`, instead of just
  echoing back `.nexus/config.toml`. This catches a silent env-mismatch
  (e.g. `api_url` resolved to production while the project only exists on
  staging) that previously reported a false "OK":
  - `ERR Not found at <api_url>`: the project isn't there; check
    `NEXUS_API_URL` / `--api-url` / `nexus config show`, or re-link.
  - `ERR Access denied at <api_url>`: reachable, but no permission.
  - `-- ... unverified`: authentication already failed or no token is
    configured, so the project binding cannot be confirmed.

### GitHub Account per Project Path (`nexus git`)

A project can use its own GitHub account (for example a dedicated account
for a customer organization), separate from your global `gh` login. The
account is configured in Nexus; the CLI keeps its token in a local `gh`
profile (`~/.config/nexus/gh-profiles/<profile>`). Tokens never leave your
machine, never appear on a command line and are never logged; your global
`gh` login, `~/.ssh/config` and global git config stay unchanged.

```bash
nexus git login --account octocat                  # browser login into the profile
nexus git login --account octocat --with-token < token.txt
```

`nexus git login` checks that the login is the expected account. On a
mismatch the profile is logged out again and nothing is written. For a
fine-grained token see `nexus git login --help`.

Once the profile is logged in as the expected account, `nexus pull`,
`nexus git apply` and `nexus git login` write:

- **`.nexus/gh.env`**: `GH_CONFIG_DIR` (absolute path) and, for GitHub
  Enterprise Server, `GH_HOST`. No token. The devbox shell sources it, so
  `gh` in that repository uses the project's account. The file is removed
  when the project has no account, the profile is not logged in, or the
  login is another account. A `.nexus/gh.env` without the Nexus header line
  is never touched.
- **Transport identity** (local git config only): with an SSH key on the
  account, `core.sshCommand = ssh -i ~/.ssh/<key> -o IdentitiesOnly=yes`;
  for HTTPS remotes, a credential helper that runs
  `gh auth git-credential` with the profile and without `GH_TOKEN`,
  `GITHUB_TOKEN` and their Enterprise variants, so a token in the shell
  cannot stand in for the profile's account. `gh repo set-default` points
  at the configured repository, or at `origin`.
- **Commit identity**: `user.name` / `user.email` of the account, each only
  when the project does not set its own.

A different local `core.sshCommand`, `user.name` or `user.email` that the
CLI did not write is replaced only after confirmation (`--yes` replaces).
`nexus git apply --canonical-remotes` rewrites remotes that use an SSH host
alias to `git@<host>:owner/repo.git` (needs an SSH key on the account).

`nexus status`, `nexus pull`, `nexus run` and `nexus git verify` print one
line for the account:

```
GitHub: octocat@github.com via profile octocat (project binding), ssh key id_ed25519_work, gh.env written, commits as dev@example.com
```

`nexus git verify` also reports a missing or stale `gh.env`, a missing or
different `core.sshCommand`, remotes on an SSH host alias, and a local
`user.email` that differs from the account's.

### Preflight Checks

`nexus preflight` validates the local environment is ready for Nexus:

| Check | What it verifies |
| ----- | ---------------- |
| Git | `git` is installed and accessible |
| Node.js | `node` >= 18 available |
| npm | `npm` available |
| npx | `npx` available |
| Config | Nexus config file exists (`~/.config/nexus/`) |
| Credentials | Valid `nxs_pat_*` token stored |
| API | Nexus API reachable and token valid |
| Workspace | `.nexus/` workspace marker present |
| MCP | Agent MCP configurations reference nexus-mcp |
| Plugin env | Variables and settings the project's plugins need (hub 0.20.0 or later) |
| Sentry live | The local Sentry token reaches the configured organization (`--no-live` skips) |

**Plugin variables.** The hub lists the environment variables and settings
the project's plugins need. The CLI asks without sending any local variable
name and decides locally whether each variable is set: in the shell, in
`.nexus/env` or in `.env.nexus.local` (git-tracked copies are ignored). A
missing required variable fails with its name (for the Sentry token also
the token scopes it needs), a missing optional one warns, a missing or
invalid hub setting fails. `nexus run` shows the same lines in its
pre-launch check.

**Sentry live check.** With the Sentry developer tool selected,
`nexus preflight` calls `https://<host>/api/0/organizations/<org>/` with the
local `SENTRY_ACCESS_TOKEN` (5 seconds, no redirects). When the hub names
another variable, it is read only if its name starts with `SENTRY_`, so no
other secret can be sent this way. The token is never
sent to the Nexus API, and only to `sentry.io`, `*.sentry.io` or hosts you
list in `NEXUS_TRUSTED_SENTRY_HOSTS` (comma-separated, exact `host[:port]`
or `*.domain`); for any other host the check is skipped with a warning. An
MCP server from the hub that passes `--host` outside these hosts is
highlighted in the consent prompt ("sends your Sentry token to ...").

`nexus run` is the single start command. It starts whatever the backend
selects for the project (`run_target`): OpenCode, Claude Code, or the Claude
Code workspace (zellij with the CCX layout; if zellij or the layout is
missing, Claude Code starts directly with a hint). `--tool <bin>` overrides
this. Backends without `run_target` fall back to `run.default_tool` and the
project's `agent_owner`. From `run_target`, only the known tools (`claude`,
`opencode`) are started; any other program needs `--tool`. A
`run_target.layout` or `run_target.config` file is used only while it is on
disk as approved by `nexus pull` (consent or `--trust`); otherwise the team
layout and the default Zellij config are used, with a note.

The terminal workspace (multiplexer) can be chosen per start or locally,
without affecting the Claude Code extensions (HUD, rules, hooks, plugins):

```bash
nexus run --plain                 # start the tool directly (= --workspace none)
nexus run --workspace zellij      # force the zellij workspace for this start
```

```toml
# .nexus/config.toml: personal default, never overwritten by nexus pull
[run]
workspace = "none"   # or "zellij"
```

Precedence: flag, then `[run] workspace`, then the project default
(`run_target.workspace`). The pre-launch check shows the choice and its
source in a `Workspace` row; the session summary repeats it.

**Arguments for the tool.** `nexus run -- <args>` passes `<args>` to the
tool only in a plain start. In the zellij workspace the layout defines the
pane commands, so the arguments are ignored with a warning; use `--plain`
to pass them.

**Claude in Chrome (experimental).** Claude Code offers its browser tools
only to a session started with `claude --chrome`. The hub's per-user
workspace setting turns this on: in the zellij workspace your personal
layout starts the claude pane with `--chrome` (`nexus run` keeps the pane
command as written, also with `--actor-panes` and the dispatches pane); in
a plain start the backend sends `run_target.tool_args: ["--chrome"]` and
`nexus run` adds it to the `claude` command line, before your own
`-- <args>`. `tool_args` is allow-listed on the CLI side: only `--chrome`
is passed, any other entry is dropped with a `Tool args` warning, so the
backend cannot add other Claude Code flags. When the start uses
`--chrome`, the pre-launch check shows a `Chrome` info row.

The support is experimental: whether the browser tools appear depends on
Chrome, the Claude extension and the local setup, which the CLI does not
check. The backend sends `tool_args` for both workspaces, so a zellij
project started plain (`--plain`, `[run] workspace = "none"`, or the
fallback when zellij or the layout is missing) keeps `--chrome`; in the
zellij workspace `nexus run` ignores `tool_args` and uses the layout. An
older backend sends `tool_args` only for `workspace: "none"`; a zellij
project started plain then starts `claude` without `--chrome`.

**Devbox shell.** In a workspace with `devbox.json`, the pre-launch check
shows a `Devbox shell` row: active, active for another project (warning),
or inactive (warning: the devbox packages such as zellij or language
servers are missing). When no devbox shell is active, devbox is installed
and someone is at the terminal, `nexus run` first asks
`Start it and re-run nexus run inside it? [Y/n]`; Enter replaces
the process with `devbox run -c <workspace> -- nexus <same arguments>`, which
runs the devbox init hook and exports the shell's variables. `-y` answers
only when `devbox.json` is on disk as approved by `nexus pull`; otherwise
the question is asked, since its init hooks run in the new shell. After that run
ends you are back in your original shell. The restarted run never asks
again (`NEXUS_DEVBOX_RESTART=1`), and `--dry-run` never asks.

**Claude Code check.** For Claude Code projects (and `--tool claude`),
`nexus run` checks before anything starts that `claude` is installed and
runnable: first on `PATH`, then in `~/.local/bin/claude` and
`~/.claude/local/claude` (native installer), and `claude --version` must
answer within 10 seconds. Missing or broken, it stops with a non-zero exit,
the path it tried and the install command
(`curl -fsSL https://claude.ai/install.sh | bash`); no zellij session is
opened. Found only outside `PATH`, it warns and puts that directory on
`PATH` for the session, so the workspace panes find it. Otherwise it prints
one line with the version. The check also runs with `--skip-checks`; only
`--skip-claude-check` turns it off (CI or headless use).

**Plugin programs.** Some agent plugins the project enables need a program
on the machine, e.g. an LSP plugin its language server. The backend lists
them (`claude_plugin_requirements`); `nexus pull`, the `nexus run` preflight
(after the Claude Code check) and `nexus doctor claude` look each program up
on `PATH` and, in a devbox project, in the devbox profile
(`.devbox/nix/profile/default/bin`). Each missing one gets one warning with
the fix, e.g. `pyright-lsp needs pyright-langserver: devbox add pyright`, or
`kotlin-lsp needs kotlin-lsp on PATH (install it, or disable the plugin in
Nexus)` when no package exists. The start is never blocked, and nothing is
shown while every program is present. Entries with a malformed plugin or
program name are skipped; nothing is executed.

Start the zellij workspace from a plain terminal: inside a running zellij
session `nexus run` refuses, since zellij would add the layout as tabs of
that session (inside the project's own session it only says so). The
`Zellij socket` row checks that the session socket path fits the OS limit
(103 bytes on macOS, 107 on Linux); if not, start with a shorter socket
directory, e.g. `ZELLIJ_SOCKET_DIR=/tmp/zj nexus run`. If zellij exits
right after the start, `nexus run` shows the end of the zellij log.

The `Zellij session` row says how the layout's `session_name` is entered:
**attach to running session**, **fresh start**, or **resurrect**. A session
that ended without `zellij kill-session` (terminal closed, reboot) stays
listed as EXITED, and attaching would restore its old layout instead of the
current one. `nexus run` therefore removes it (`zellij delete-session`,
never a running session) and starts fresh; `--resume` resurrects it
instead.

**Personal layout.** When the backend renders a workspace layout
for you (`af_export.claude_workspace`, e.g. a different preset than the team
default), `nexus pull` writes it to `.nexus/claude/workspace.local.kdl`,
which is added to `.git/info/exclude` and never committed. `run_target.layout`
points to it. It is regenerated on every pull; a local edit is reported and
kept unless `nexus pull --force` (`nexus status` / `diff` / `reset` list it as
a projection). Until it has been pulled, `nexus run` falls back to the
committed team layout `.nexus/claude/nexus-claude.kdl` with a WARN. The
per-user Zellij config works the same way; when the backend stops sending
it, pull removes the file only while it is unchanged. A file that cannot be
read is kept together with its pull-manifest record and reported; only a
missing file is dropped from the record silently, and a path that now leads
through a link out of the workspace is never removed.

### Agent Observability (`nexus status --agents`)

The Nexus Claude Code hook appends `nexus.agent-observation.v1` events to
`.nexus/claude/observer/events.jsonl` (local, git-excluded). `nexus status
--agents` folds them per session and agent and shows the fleet: counts,
main agent, subagents as a tree, background processes, state, current tool,
runtime, model and repository/worktree. No network or login needed.

```bash
nexus status --agents            # one-shot view (finished sessions older than 30 min hidden; --all shows them)
nexus status --agents --watch    # follow the file, redraw every second (the workspace observer pane runs this)
nexus status --agents --json     # the folded agent list as JSON
```

It also works in a second terminal next to `nexus run --plain`. Without the
hook it reports "no observation data".

### Doctor (`nexus doctor claude`)

Checks `claude`, `zellij`, `lazygit`, `ccusage` and `delta`. Which ones are
required comes from your workspace layout (`claude_workspace.requires`),
else from the run target (zellij). `claude` is looked up like `nexus run`
does (also in `~/.local/bin` and `~/.claude/local`) and counts only when
`claude --version` succeeds. Missing required tools fail (exit 1),
missing optional ones are informational; the Claude Code version is checked
against the project's CCX compatibility range. For Zellij workspaces the
**zellij session** row flags the project's session when it is EXITED
(`--fix` deletes it after confirmation) and lists other exited sessions with
the delete command. `--fix` prints devbox (for
devbox projects), brew or npm install commands and runs them only after
confirmation (`-y`). `nexus pull` never installs tools (managed Claude Code
plugins are the one exception, see below).

For Zellij workspaces the **workspace clipboard** row checks that text
copied in a pane reaches the system clipboard. It reads the Zellij config
the session uses (`ZELLIJ_CONFIG_FILE`, the project-local config, else
`ZELLIJ_CONFIG_DIR` or `~/.config/zellij/config.kdl`). Without an active
`copy_command`, Zellij copies via OSC 52: iTerm2 drops that unless
"Applications in terminal may access clipboard" is enabled, Terminal.app
does not support it, Ghostty, WezTerm and kitty do. On Linux the check
suggests `wl-copy` (Wayland) or `xclip -selection clipboard` when
installed; over SSH OSC 52 is the right default. `--fix` writes the
matching `copy_command` line after confirmation: it uncomments the
template line when present, else appends it, creates a minimal config when
none exists, backs up an existing one to `config.kdl.bak-<date>` and
changes nothing else. The project-local config is never written. `nexus
run` prints a one-line warning while the check fails, at most once per day
and CLI version.

**MCP servers (package versions).** Servers in `.mcp.json` and
`opencode.json` that start a package (`npx`, `bunx`, `uvx`, `pnpm dlx`) are
listed with the package version; servers not pinned to an exact version are
flagged. `--output json` reports them as `mcp_servers`.

**Plugin programs.** A missing program an enabled plugin needs is a
`plugin program` warning with the fix (see "Plugin programs" under
`nexus run`); it does not change the exit code. `--output json` reports every
listed program as `plugin_programs` (`plugin`, `binary`, `found`, and the
`message` when missing).

**Claude in Chrome (experimental).** When the start `nexus run` would make
without flags uses `--chrome` (the claude pane of the layout, or
`tool_args` in a plain start; the local `[run] workspace` counts), the
claude row gets an info line. `--output json` reports it as `claude_chrome`. The
Chrome extension itself is not checked.

`nexus run` embeds the same checks and adds a **launch countdown** after
they complete. The countdown gives you a moment to review the results before
the tool starts. Press `Ctrl+C` at any time to abort.

### Classification (`nexus classification`)

A project's confidentiality level drives runtime behaviour. The effective
level is the higher of the project level and its customer's level, unless
the project level was set with `--force` (see below); the hub
maps it to a gateway class (`public`, `internal`, `confidential`,
`restricted`). That class decides fail-closed gateway routing and whether
lane delegates are rendered, and the level decides which external services
the confidentiality gate blocks. Everything is read from the hub on each
call, never cached.

```bash
nexus classification                     # project, customer, effective level, gateway class, consequences
nexus classification --output json
nexus classification levels              # every level with label, group and gateway class
nexus classification set confidential --dry-run
nexus classification set confidential --reason "lane test" --pull
```

`set` always prints an impact plan first (effective level, gateway class,
delegates, newly allowed or blocked services) and asks before applying it;
`-y` skips the question. A downgrade (the backend decides which changes
count as one and marks it in the plan, e.g. `vs-nfd` to `confidential`,
even when the gateway class stays the same) needs `--reason`, which goes
into the audit log, and you type the project slug to confirm; `-y` does
not skip that. When the
customer level dominates, the change would have no effect: `set` says so
and stops. `--force` makes the project level override the customer level,
also below it (`nexus classification set internal --force --reason "..."`):
the hub then takes the project level as the effective level and gateway
class, the plan shows `Override: off -> on`, and `nexus classification`
marks the project level as forced. Going below the customer level is a
downgrade, so `--reason` and the slug confirmation apply. Setting the same
level again with `--force` only switches the override on; any later `set`
without `--force` switches it off, so the customer level is the floor
again. Hubs without override support ignore `--force` and only store the
project level. The customer level itself is changed by customer admins in
the dashboard. If the classification changed since it was read
(409), `set` shows the current state and you re-run it. `--pull` runs
`nexus pull` afterwards so delegates appear or disappear right away.
`nexus status` and the `nexus run` pre-launch check show
`classification: <effective> (<gateway class>, from <project|customer>)`.

**Why are my delegates missing?** Lane delegates are not rendered when the
gateway class is `restricted` (e.g. strictly confidential, VS-Vertraulich
and above, EU CONFIDENTIAL and above). `nexus pull` and `nexus doctor
claude` say so; `nexus classification` shows which level is responsible.
Lane actors still run in their own session: `nexus actors run <slug>` or
`nexus run --actor-panes`.

### Claude Code local lane

Actors on a gateway route run as their **own Claude Code process** against
the Nexus gateway, while the main session stays on your Claude
subscription (Claude Code allows one base URL per process, so subagents
cannot switch). `nexus pull` writes:

- `.claude/agents/<slug>.md` for session-lane actors. When the backend sends
  ready Claude subagents (`claude_agents`), they replace the raw actor
  markdown.
- a thin delegate `.claude/agents/<slug>.md` for each gateway-lane actor:
  it hands the task to `nexus actors run <slug> -p ...` and returns the
  result, so task and result pass through the main session. Projects whose
  gateway classification is `restricted` get no delegates (an unmodified
  earlier one is removed); use the actor pane or `nexus actors run` there.
- `<agentic_root>/generated/claude-lane.json` with the lane actors.

`nexus actors run <slug>` starts `claude` with a temporary settings file
(mode 0600, removed on exit) that points `ANTHROPIC_BASE_URL` at the gateway
and authenticates with the project inference token (`nexus project link`);
every model tier is set to the actor's profile as gateway model id
`nexus/<profile>`, the request carries
`x-nexus-actor: <slug>`, and non-essential traffic is off. The token never
appears on the command line, and other credentials are removed from the
process environment. Without a project token it refuses to start, so a
claude.ai login is never used. `-p "<prompt>"` runs headless; arguments
after `--` go to `claude`. The gateway URL comes from `--gateway-url`,
`NEXUS_GATEWAY_URL` or the backend. When a Claude settings file sets a
main-thread `agent`, the lane session starts under a neutral agent instead,
so that agent's prompt and model do not replace the actor's (pass your own
`-- --agent <name>` to override).

When you have an open Nexus session for the project at launch (on a
`staging/release-X.Y.Z` branch: the session of that release line), the
requests also carry `x-nexus-session-id: <id>`, so the gateway attributes
their usage to that session. Without one the header is left out, the actor
starts normally and a one-line hint says so. The header is fixed when the
actor starts: a session opened later is picked up only after restarting
the actor.

Two lines Claude Code prints on every lane run are expected and harmless:
`[claude-code:unrecognized_model]` (the gateway model id `nexus/<profile>`
is not an Anthropic model name) and the note that claude.ai connectors are
disabled because another auth source is set (the lane authenticates with
the project token on purpose; your claude.ai login is not touched).

`nexus run --actor-panes` opens the Zellij workspace with an extra `actors`
tab, one pane per lane actor. `nexus doctor claude` checks the lane: the
project token, the gateway's `/health/ready` (a connection error points to
the WireGuard or LAN route) and whether `/v1/models` lists every actor
profile.

`nexus actors export --target claude` prints, as JSON, what the backend
renders for Claude Code: the subagent files, the lane actors and the
main-thread agent; backend warnings go to stderr. It writes no files
(`nexus pull` does that), so it suits checks and scripts.
`--target opencode` (the default) prints the OpenCode agent section.

### Dispatch status (`nexus dispatch status`)

A read-only view of the project's Dispatch traffic with other projects:

```bash
nexus dispatch status                    # counters, open Dispatches, per-peer summary
nexus dispatch status --watch            # redraw every 30 s (--interval, at least 10)
nexus dispatch status --compact          # for a narrow side pane (about 45 columns)
nexus dispatch status --json             # nexus.dispatch-status.v1
nexus run --dispatch-pane                # the compact view as a pane in the CLAUDE tab
```

- **Incoming** (this project is the target): counted as open, waiting on us (open, acknowledged or blocked), in progress and blocked.
- **Outgoing** (this project is the sender): counted as open, waiting on us (`waiting_for_requester`, `needs_review`) and waiting on the peer (every other open status).
- **Resolved and closed** counts cover the last `--window` days (default 7).
- **Rows:** one per open Dispatch, with direction, peer project, type, priority, a `!` for blocking, status, age and the time since the last activity. A `>` marks the ones where the next step is ours; they come first, then blocking ones, then the most recent activity. Closed and resolved Dispatches leave the list and only count in the period line. A Dispatch from the hub's simulation form (title starting with `[Simulation]`) is shown in yellow.
- **Options:** `--project <id>` shows another project you can access instead of the linked one. `--window <days>` sets the period for resolved and closed counts.

The compact view reads like this:

```text
NEXUS-CLI 03:52Z manager none
in  0 open 1 ours 0 prog 0 blkd
out 0 open 0 ours 0 peer
7d  0 resolved 28 closed
> in  NEXUS-APP  ack   6m nexus dispatch status: …
NEXUS-APP    1/0, 1 ours
```

| Mark | Meaning |
|---|---|
| `>` | the next step is ours |
| `!` | blocking Dispatch |
| `ours` / `peer` | waiting on us / waiting on the peer project |
| `prog` / `blkd` | in progress / blocked |
| `ack`, `wait`, `revw` | acknowledged, `waiting_for_requester`, `needs_review` |
| `1/0` | open Dispatches with that peer: incoming / outgoing |

The time on a row is the time since its last activity. When the pane is too short for all rows, the last visible line says how many more there are (`… +3 more`).

On a color terminal the counters and statuses are colored: what waits on us (and `>`) yellow and bold, blocked and `!` red, in progress blue, open cyan, resolved green, closed dimmed. A counter of 0 is dimmed. Colors do not change the column widths; `NO_COLOR`, a pipe and `--json` give plain text.

The backend returns at most 50 Dispatches per list, so a count from a full list is shown as a lower bound (`50+`). With `--watch`, a failed refresh keeps the last state on screen and adds an error line. The command uses your token only as the request's bearer header. It holds no lock, so `pull`, `push` and `run` work while it runs. The header calls this machine's Dispatch runner the dispatch manager (`manager` in the pane, `Dispatch manager` in the `nexus run` checks); the commands keep `nexus dispatch runner`. Its state is read from the listener of the workspace's hub (`~/.config/nexus/state/dispatch/hubs/<hub>/runner.json`) and stays `none` until a runner writes that file. A runner bound only to other projects shows as `elsewhere`, with a line naming the projects it serves; for a bound project the line shows the runner's last error, such as a failed heartbeat. When the project has no linked projects yet, an info line says that no Dispatches from other projects arrive until one is linked in the Hub; `--json` carries the count as `linked_projects` (left out when the hub does not report it).

`--json` prints one object (with `--watch`, one line per refresh):

```json
{
  "schema": "nexus.dispatch-status.v1",
  "project": {"id": "<uuid>", "name": "<name>"},
  "refreshed_at": "2026-11-07T03:52:07Z",
  "runner": {"state": "none"},
  "window_days": 7,
  "incoming": {"open": {"value": 0, "truncated": false}, "waiting_on_us": {...}, "in_progress": {...}, "blocked": {...}, "resolved": {...}, "closed": {...}},
  "outgoing": {"open": {...}, "waiting_on_us": {...}, "waiting_on_peer": {...}, "resolved": {...}, "closed": {...}},
  "peers": [{"project_id": "<uuid>", "name": "<name>", "incoming": 1, "outgoing": 0, "waiting_on_us": 1}],
  "dispatches": [{"direction": "in", "id": "<uuid>", "peer_project_id": "<uuid>", "peer": "<name>", "title": "...", "type": "bug_report", "priority": "high", "blocking": false, "status": "open", "created_at": "...", "updated_at": "...", "waiting_on_us": true}],
  "error": null
}
```

`truncated: true` means the count came from a full list of 50 and may be higher. `error` holds the last refresh error in watch mode. `runner.note` appears when a watching view cannot run the runner, with the reason.

`nexus run --dispatch-pane` adds a `dispatches` pane running `nexus dispatch status --watch --compact` directly below the `claude` pane of the CLAUDE tab, at 13% of the height, as the hub workspace presets do. A layout without a `claude` pane gets a separate `dispatches` tab instead. Run from inside the project's running zellij session, it opens the pane there below the focused pane, at about a fifth of its height, and returns the focus to the pane above, because a layout only applies to a new session. A layout or tab that already has a `dispatches` pane (the hub workspace presets include one) is left as it is. When the project's dispatch automation (`policy.dispatch_automation`) is not `off`, `nexus run` adds the pane without the flag and says so in its checks; `--no-dispatch-pane` leaves it out. The policy default applies only to the zellij workspace. If the project has no linked projects yet, the `Dispatch manager` check says so as information instead of warning about an unregistered or unbound runner, and names the register step for after the first link. When the runner is not registered on this machine, or not bound to this project (for example after setting up the machine again), `nexus run` offers to fix it: Enter (or `-y`) runs `nexus dispatch runner register` with its defaults (notify mode, the linked project added to earlier bindings) and shows the `Dispatch manager` check again. A failed registration is a warning and the start goes on; `--dry-run` and runs without a terminal never ask.

With `--watch`, the view also runs this machine's Dispatch runner in the same process (see below) when it is registered and bound to the project. Otherwise a yellow `runner:` line says why it is not listening; `--no-listen` turns this off. Only one runner runs per machine: a second view shows the state of the one that listens.

### Dispatch runner (`nexus dispatch runner`, `nexus dispatch listen`)

The runner tells running agent sessions about Dispatch traffic, so nobody has to relay it by hand. For now it is notify only: it reads the hub event feed and delivers short notices. It starts no agent, claims nothing and changes no status.

```bash
nexus dispatch runner register           # once per machine and hub; binds the linked project
nexus dispatch runner register --project <id>   # bind another project (earlier bindings stay)
nexus dispatch runner status             # every runner on this machine: mode, blocking switch, lag
nexus dispatch runner pause|resume       # this hub's runner; the hub dashboard can do the same
nexus dispatch runner revoke             # final; removes this hub's local token file
nexus dispatch runner probe              # can the installed Claude Code take notices?
nexus dispatch runner titles on|off      # quote the sender's title in notices (default off)
nexus dispatch listen                    # the runner in the foreground (Ctrl-C stops it)
```

A production and a staging runner on one machine: register in a workspace
of each hub (or pass `--api-url`); each workspace's dispatches pane then
runs its own hub's runner.

```bash
cd ~/work/app-on-production && nexus dispatch runner register --project <id> --project <id>
cd ~/work/app-on-staging && nexus dispatch runner register
nexus dispatch runner status             # lists both, marks the current workspace's hub
```

- **One runner per hub.** Each hub (API origin) has its own runner, so a production and a staging runner work side by side on one machine. The commands act on the hub of the effective API URL (`--api-url`, `NEXUS_API_URL`, the linked project's `api_url`, else the global one); `runner status` lists every runner and marks the one of the current hub; with `--json` it prints `{"schema": "nexus.dispatch-runners.v1", "claude_code": {...}, "runners": [...]}`.
- **Register** uses your personal access token once. The hub returns a runner token (`nxs_run_...`) a single time; it is stored in `~/.config/nexus/dispatch-runners/<hub>.json` with mode 0600 and never printed again. `<hub>` is the origin without `https://`, for example `nexus.gatewarden.eu.json`. A binding carries the sha256 of the repository path, never the path itself. A new register replaces the earlier runner of this machine on the same hub only; runners of other hubs are not touched. A listener that is running picks the new registration up: when the hub rejects its token, it re-reads the runner file and continues with the new token, and it only stops when the file still holds the rejected one. The single `~/.config/nexus/dispatch-runner.json` of versions before 0.40 is moved to its hub's file on first use, together with the listener's cursor, so nothing has to be registered again.
- **Listen** runs the runner of the workspace's hub: one listener per hub, each with its own lock. It polls the feed every 30 seconds and sends a heartbeat every 60 seconds with its cursor and delivery counts. It holds an exclusive lock (`~/.config/nexus/state/dispatch/hubs/<hub>/runner.lock`) and writes its state atomically to `runner.json` next to it, without the token. Two workspaces on the same hub share one listener (the second pane only shows it and retries every 30 seconds, so it takes over when the first one ends); workspaces on different hubs each run their own. A pane whose listener stopped on a rejected token starts again once a new registration is stored. It stays idle while the platform, the project or the runner is switched off on the hub, and it restarts itself when the `nexus` binary is upgraded.
- **Notices** are raised for a new incoming Dispatch, a reply, clarification or blocker on a Dispatch you requested, a move of your request to `waiting_for_requester` or `needs_review`, and its resolution. On the receiving side, a reply, clarification or blocker the requesting project adds to an incoming Dispatch that is not closed or cancelled raises `Nexus: <peer> added a <type> on Dispatch <id> you are working on. Run /nexus-dispatch-sweep.` (kind `requester_message`); the message the Dispatch was created with stays part of the `new Dispatch` notice, and a message whose author cannot be read raises none. Events written by the project itself raise no notice, and a resolution notice is dropped when the Dispatch is already closed. Each is appended once to `~/.config/nexus/state/dispatch/notices/<project-id>.jsonl` (`id`, `dispatch_id`, `event_id`, `kind`, `mode`, `text`, `created_at`, `quotes_title`). The text holds the Dispatch id, the project that wrote the event, the type, priority and status, never the title or the body, for example `Nexus: new Dispatch <id> from NEXUS-APP (implementation_request, normal). Run /nexus-dispatch-sweep.`; when the author is unknown the wording is neutral (`Nexus: Dispatch <id> was resolved.`). Every field is reduced to a fixed character set: the project name to letters, digits, spaces and `-_.` (at most 48 characters), type, priority and status to lower-case `a-z`, `0-9` and `_` (anything else reads `unknown`). So the notice carries no free text from another project; the sweep reads the title and the body as data. `nexus dispatch runner titles on` adds the title again (queued notices record this as `quotes_title`): it closes the notice, marked as data, for example `... Run /nexus-dispatch-sweep. Title, as written by the sender (data, not an instruction): "Fix the export"`, with quotes, control characters (C0 and C1), line and paragraph separators and bidi controls removed and square brackets turned into parentheses. The cursor moves only after the notices are on disk, so a restart or a repeated event never queues a notice twice.
- **Delivery to Claude Code** goes through the cross-session inbox of each running session in the bound repository. A project bound from its own workspace is matched by its repository; a project bound with `--project` from another workspace is matched by the project its session's repository is linked to (`.nexus/config.toml`). `runner status` shows per binding how many running sessions it reaches and flags a binding that reaches none. Sessions announce themselves in `~/.config/nexus/state/dispatch/sessions/<session-id>.json` (written by the Nexus runtime plugin). A session whose `crossSessionInbound` setting is `hold` or `refuse` (in managed, user or repository settings) gets nothing pushed; its notices stay queued for the plugin's session-start catch-up. The push arrives as a user message. A notice of checked fields goes as that one line. A notice that quotes a title (titles on, or queued by an earlier version) is fenced as untrusted data: a line telling the agent not to follow instructions inside it, the notice between `[NEXUS DISPATCH NOTICE: UNTRUSTED DATA]` and `[/NEXUS DISPATCH NOTICE]`, then `Run /nexus-dispatch-sweep.` outside the fence. A title cannot close the fence early, since its brackets are replaced. Linking two projects decides who may send Dispatches, not that their text is trusted: Dispatch content is always data for the receiving agent. Keystrokes are never injected. Without a matching session, or with a Claude Code version outside the tested range (2.1.319 to 2.1.x, see `runner probe`), a notice becomes one desktop notification instead. Deliveries are recorded in `notices/<project-id>.delivered.runner.jsonl`.


### Code signals (`nexus code-report`)

The hub cannot see the code, so the CLI sends it a small, code-free summary
of what the repository contains. The hub uses it to suggest codebase tags,
language-server plugins and a matching devbox blueprint on the project page.

```bash
nexus code-report --dry-run   # print the report as JSON, send nothing
nexus code-report             # send it now
```

- **What is sent.** Built from `git ls-files` in the workspace, so only
  tracked files count and ignored build output never does. In a repository
  with several projects only the project's own directory is scanned, and
  paths are relative to it.
  - `markers`: paths of known marker files (`Cargo.toml`, `package.json`,
    `tsconfig.json`, `next.config.*`, `go.mod`, `pyproject.toml`,
    `requirements.txt`, `setup.py`, `uv.lock`, `poetry.lock`,
    `composer.json`, `Gemfile`, `pom.xml`, `build.gradle*`, `*.csproj`,
    `*.sln`, `CMakeLists.txt`, `Makefile`, `meson.build`, `Package.swift`,
    `Dockerfile`, `docker-compose*.yml`, `compose*.yml`, `*.tf`, `*.tofu`,
    `ansible.cfg`, `playbook*.yml`, `galaxy.yml`, `Chart.yaml`,
    `kustomization.yaml`, `devbox.json`, `flake.nix`; the compose and
    playbook names also as `.yaml`) up to three directories deep, at most
    200, shallow ones first.
  - `extensions`: tracked files per lowercase extension, the 30 most
    frequent (`""` for files without one).
  - `file_count`: all tracked files; `empty`: true when nothing is tracked
    besides Nexus-managed files (`.nexus/`, `.claude/`, `CLAUDE.md`,
    `AGENTS.md`). Managed files never count as markers or extensions.
  - `source`, `cli_version`, `scanned_at`.

  Never sent: file contents, other file names, the repository path,
  remotes.
- **When.** After `nexus link` (also a re-link), and on `nexus run` at most
  once per 24 hours per project. `nexus run` sends it in the background
  while it prepares the launch and waits at most 3 seconds for it; the
  last report time is kept in
  `~/.config/nexus/state/code-signals/<project-id>.json`. `nexus run
  --dry-run` and `--no-db` send nothing.
- **Answer.** When the hub has suggestions, one line says so: `Nexus: <n>
  project improvements suggested, see the project page.` A hub without the
  endpoint (404) is skipped silently; any other error is one warning line
  and never stops `link` or `run`.
- **Turn it off.** `nexus config set code_signals=false` or
  `NEXUS_NO_CODE_SIGNALS=1` stops the reports of `link` and `run`;
  `nexus code-report` always sends when you call it.

### Directives

Directives are short binding rules: a platform
catalog plus project rules. `nexus pull` renders them into
`.nexus/directives.md` as a compact list, grouped by category in the order
the server sends them (`nexus init` uses the same writer and records the
file the same way, so `nexus status` tracks it; a target linked out of the
workspace or into `.git` is not written, and an existing `directives.md`
without the Nexus marker is kept unless `--force` is given):

```markdown
## Security

- MUST: Never put secrets, credentials, tokens or private keys into output, logs, commits ...

## Workflow

- MUST: Track every working session in Nexus ...
- SHOULD: ...
```

MUST/SHOULD comes from the directive's `level` (older servers: `priority`
high/urgent = MUST). No rationale is sent to agents. Claude Code projects
with CCX also get `.claude/rules/15-nexus-directives.md`, which Claude Code
always loads; `.nexus/directives.md` is then the readable copy.

**Screenshots stay in the local vault.** `nexus link` and `nexus init`
create `.vault/` with `.vault/screenshots/`, the default output for browser
screenshots, which can show customer data. When git does not ignore
`.vault/` yet, they append `.vault/` to the repository's `.gitignore` (the
existing bytes stay as they are, in any encoding; a `.gitignore` that
cannot be read is left unchanged and the vault is reported as not ignored); in a
shadowed project (or when the shadow level is unknown, e.g. offline) to
`.git/info/exclude` instead, so nothing visible changes. Existing vault
content is never moved or deleted. When git already tracks files under
`.vault/`, nothing is changed: a warning names the count and how to untrack
them (`git rm -r --cached .vault`).

**Tokens are never shown in clear text.** `nexus login` reads the token
without echo: each typed or pasted character shows as `*`, and once you press
Enter only the masked form remains on screen (piped input, for example
`printf '%s' "$TOKEN" | nexus login`, still works). `nexus status`,
`nexus preflight` and the env table of `nexus run` show tokens as their
prefix plus the last four characters (`nxs_pat_****9f3c`), and mask every
injected env value whose name contains `TOKEN`, `SECRET`, `PASSWORD`, `KEY`,
`AUTH` or `CREDENTIAL`.

**Credential files are private and written in one step.** Credentials,
project tokens, the global config and the Dispatch runner files are written
to a new owner-only (0600) temporary file that then replaces the old one, so
an interrupted save keeps the previous file and older, wider permissions do
not carry over. `nexus login` without `--global` refuses to store the token
when `.nexus/credentials.toml` is tracked by git or `.nexus` is a symlink.

**Tokens only go to API hosts you chose.** When the API URL comes from a
`.nexus/config.toml` that the repository tracks in git, and its host is not
approved by you, every command asks before sending your token there and
refuses without a terminal or with `--yes` (see "API URL from a tracked
project config" under Configuration).

**What `nexus pull` and `nexus init` accept from the backend.** Every path the
server sends (agent files, skills and their resources, OpenCode commands,
actor profiles and avatars, Claude hook adapters, devbox scripts) must be
relative, stay inside the workspace, not point into a `.git` directory and
not resolve outside the workspace through a symlink; anything else is skipped
with a warning. Skill ids, command slugs, actor slugs and hook event names
are single segments (`[A-Za-z0-9._-]`), and an unsafe `agentic_root` falls
back to `.nexus`. Prerequisite checks never run through a shell: only
`<tool>`, `<tool> --version`, `-v`, `-V` or `version` is executed, otherwise
the tool is looked up on `PATH`. The token-bearing `.mcp.json` and
`opencode.json` are written owner-only (`0600`), listed in `.git/info/exclude`,
and reported when git already tracks them. Responses larger than 8 MiB are
refused.

**Executable content needs your consent.** Some of what the backend sends
runs on your machine: Claude Code hook scripts and their registrations,
`statusLine` and other command settings (`hooks`, `apiKeyHelper`,
`awsAuthRefresh`, `awsCredentialExport`, `otelHeadersHelper`), enabled
plugins and plugin marketplaces, MCP servers in `.mcp.json` / `opencode.json`,
Zellij layouts, OpenCode plugins, agent files in runnable locations, skill
scripts and skills with inline shell (`!` followed by a command in
backticks), `devbox.json` and devbox scripts. `nexus pull` and `nexus init`
fingerprint each item as it would be written (MCP servers including their
env values) and ask before a new or changed item lands on disk. An
OpenCode plugin download that fails or arrives empty (in `nexus pull` and
`nexus init`) leaves the installed plugin as it is:

```
   ? 2 executable item(s) from the server need your consent:
      new      mcp_server task-master  ->  .mcp.json
      changed  claude_hook session-guard  ->  .claude/hooks/nexus-session-guard.mjs
? Install them? › Review each / Accept all / Skip all
```

"Review each" shows the command and a diff against the local file (env values
only as a short hash). A diff longer than 80 lines is shortened; the prompt
then offers Skip, Install or "Show the full diff" before you decide. A refused item keeps its local version, or is left out
when there is none; the rest of the pull continues. Without a terminal, new
or changed items are skipped and listed; `--trust` accepts them (for CI).
Items that are already on disk exactly as they would be written are accepted
without asking, so the first pull after an upgrade only asks about real
changes. Accepted fingerprints are stored per workspace in
`~/.config/nexus/consent/` (owner-only), outside the repository, so a commit
cannot pre-approve anything; delete the file to be asked again. The backend
also declares its executables with a hash; the CLI compares it with its own
fingerprint, reports a difference and then asks, and points out items the
backend did not declare.

### Shadow Mode (`nexus shadow`)

`nexus shadow on` hides three targets from Git; `nexus shadow off` is the
reverse step. Without `--target` (or with `--all`) both switch every target:

| Target | Hides | Stored in |
| ------ | ----- | --------- |
| `agentic` (aliases `ai`, `agentic-files`) | Agentic files: `.claude/`, `.nexus/`, `AGENTS.md`, `CLAUDE.md`, `opencode.json`, `.opencode/`, ... | `policy.shadow` `ai` |
| `nexus` | Nexus internals in commit messages and tracked files (shadow guard); includes `agentic` | `policy.shadow` `nexus` |
| `workspace` | Devbox files: `devbox.json`, `devbox.lock`, `.devbox/`, `scripts/devbox/` | `policy.shadow_workspace` (older backends: local exclude block) |

```bash
nexus shadow on                                # all targets (same as --all)
nexus shadow on --target agentic               # only the agentic files
nexus shadow on --target agentic,workspace     # repeat --target or separate with commas
nexus shadow off --target nexus                # keep agentic, drop the Nexus content rules
nexus shadow off --target workspace --dry-run  # show what would change
nexus shadow on --target workspace --untrack   # also stop tracking committed devbox files
nexus shadow status                            # one line per target, then the details
nexus shadow check                             # full report; exit 1 when anything is found
```

`nexus` includes `agentic`: switching `nexus` on also hides the agentic
files, switching `agentic` off also switches `nexus` off, and switching
`nexus` off keeps `agentic`. The workspace target sets the project setting
`policy.shadow_workspace`; while it is unset, the workspace fork's shadow
mode decides. The server then sends the workspace paths in its exclude list,
so `nexus pull` adds or removes them for everyone on the project and drops an
older local workspace block. On backends without the setting,
`nexus shadow off --target workspace` is recorded locally
(`# nexus workspace shadow: off` in `.git/info/exclude`), so `nexus pull`
does not switch it on again for a workspace fork in shadow mode;
`nexus shadow on --target workspace` removes the line. Like `on`
never untracks committed files on its own, `off` never stages them: it prints
`git add <path>` for each previously excluded path that exists but is not
tracked.

An exclude only hides files git does not track. Files git tracks, committed
or only staged (for example `devbox.json` and `scripts/devbox/` in an
existing repository), keep showing up in `git status`; `nexus shadow on`
never takes them out of the index on its own and lists them in one hint
with both ways out:

```
  ! 14 file(s) are tracked by git (committed or staged), so git status keeps showing their changes; an exclude only hides untracked files:
      devbox.json
      devbox.lock
      scripts/devbox/ (12 file(s))
    Stop tracking them (they stay on disk; your next commit removes them from the repository, ...):
      nexus shadow on --target workspace --untrack
    Or hide their changes in this clone only:
      git ls-files -z -- devbox.json devbox.lock scripts/devbox | xargs -0 git update-index --skip-worktree
```

`--untrack` takes them out of the git index (`git rm --cached`); review
with `git status` and commit. It selects files the way git applies the
exclude rules, so `.claude/` also covers a nested `sub/.claude/`, and it
only touches the targets being switched. A file whose staged content
differs from both the last commit and the work tree is not forced out: git
refuses it, the command names it and exits non-zero, and the files untracked
before stay untracked. A teammate's `git pull` then deletes their
copies, and `nexus pull` writes them again. `nexus shadow
ai|nexus` and `nexus workspace shadow on|off|status` still work and print
the new command.

The agentic and nexus targets are the project setting `policy.shadow`:

| Level   | Effect |
| ------- | ------ |
| `off`   | Only personal files are git-excluded. |
| `ai`    | Agentic files stay out of git: `.claude/`, `.nexus/`, `AGENTS.md`, `CLAUDE.md`, `opencode.json`, `.opencode/`. |
| `nexus` | As `ai`, plus no Nexus references in repository content and no AI attribution (enforced by the directives). |

**Machine-local scripts.** A shadowed project that commits its devbox files
gets them in a shadow-safe form without the AI tooling. The backend then
also sends machine-local scripts (today `scripts/devbox/init/local.sh`,
sourced by the committed `devbox.json` init hook when the file exists) that
bring the tooling back on your machine only. `nexus pull` lists them in
their own block in `.git/info/exclude` (`# >>> nexus local scripts`) before
writing them, asks for consent like for any other script, and keeps a local
edit unless you pass `--force`. `nexus push`, `nexus status`, `nexus diff`
and `nexus stash` ignore them. When the backend stops sending one (the
project is no longer shadowed, or its devbox files are excluded instead),
the next pull deletes it with its exclude line; an edited copy stays and is
reported. Backends without the field leave them alone.

When the server sets a level, `nexus shadow status` labels the old local
agentic block as `Local shadow block (legacy)` and the devbox block as
`Workspace shadow (devbox files)`, so neither reads as a contradiction to the
server level.

Every `nexus pull` writes the server's `git_exclude` list into a managed
block in `.git/info/exclude` (`# >>> nexus managed exclude (server)`); paths
the server stops sending are removed from it, your own lines are never
touched, and unsafe entries (absolute, `..`, `!`, `#`) are ignored. `nexus
shadow <level>` sets the level and applies the block at once. An exclude
does not untrack files already in git: Nexus never untracks them itself
and prints a hint per path instead:

```
   ! .claude/ is excluded but still tracked by git; untrack it with: git rm -r --cached .claude
```

Workspaces not linked to a project, or backends without `policy.shadow`,
keep the old local mode (`on` adds a fixed pattern block, `off` removes it).

**Shadow guard.** For a level other than `off` the backend sends content
rules (portable regexes, each for commit messages and/or tracked files);
the CLI applies them as sent:

- `nexus shadow check [--commits N] [--json]` scans tracked files (excluded
  paths and binaries skipped) and the last N commit messages of the current
  branch (default 50), and lists excluded paths git still tracks with the
  one-time fix (`git rm --cached -r -- <path>`, never run for you). Read-only;
  exit code 1 when anything is found, so it works in CI. When git cannot read
  the index or the commit history the check fails with an error instead of
  reporting a clean result; an empty index or a branch without commits is
  clean.
- `nexus pull` caches the rules in `.nexus/generated/shadow-guard.json` (for every agentic root)
  and installs a `commit-msg` hook that checks each message offline
  (`nexus shadow check-msg`). A rejected message names the rule; `git commit
  --no-verify` bypasses the check once. The hook checks the message git will
  store: comment lines are skipped only when git strips them (editor flow
  with `commit.cleanup` `strip` or `default`), the `scissors` mode cuts at the
  scissors line, and with any other setting (or `-m`/`-F`) every line is
  checked. An existing untracked hook is kept (as `commit-msg.pre-nexus`) and
  run first; a hook tracked in the repository or owned by a hook manager
  (husky, lefthook, pre-commit) is left alone, also when the tracked hook was
  deleted locally, and pull prints the one line to add to it. A Nexus hook
  without the executable bit is not reported as installed and is repaired to
  mode 0755 by the next pull. The hook is removed again when the level is
  `off` or the project switches the check off; a hook inside the work tree
  (`core.hooksPath`), and the chained `commit-msg.pre-nexus`, are
  git-excluded. A `core.hooksPath` that git cannot reach (a `..` after a
  missing directory or a file) is reported as an error.
- `nexus shadow status` and `nexus shadow <level>` always print the effective
  state (level, excluded paths and how many are still tracked, hook) and a
  check summary, also when the level did not change; when the check cannot
  run they print a `Check: failed: <reason>` line. Switching shadow on does
  not clean content that is already committed.

### User-Invoked Skills

A skill with `invocation: user` (e.g. `nexus-grill-me`) gets
`disable-model-invocation: true` in its `SKILL.md` frontmatter
(`.claude/skills/<id>/` and `.nexus/skills/<id>/`), so Claude Code runs it
only when you call `/<skill>`, never on its own. Skills without the field
(older servers) stay model-invoked. OpenCode has no equivalent; its command
stub is unchanged.

### Live updates (`nexus reload`)

`nexus reload` brings backend changes into a workspace whose Claude Code
session and terminal layout keep running, for example from a second pane.
It runs `nexus pull` (same `--force`, `--force-unmanaged` and `--trust`)
and the managed plugin step of `nexus run`, compares the watched files and
plugin versions before and after, and prints per area what the running
session does with the change:

| Area | Running session |
| --- | --- |
| Skills, commands, subagents | active now |
| `.claude/settings.json`, statusline | active now (Claude Code watches the files) |
| Hook scripts (`.claude/hooks/`) | active from the next hook call (registration lives in `settings.json`) |
| Plugins (installed or updated) | `/reload-plugins` in the session |
| `CLAUDE.md`, `AGENTS.md`, rules, directives | read at session start: the session re-reads them, or `/compact`, or a restart |
| `.mcp.json` | restart: Ctrl-C, then `claude --continue` (the conversation stays) |
| `devbox.json`, `scripts/devbox/**` | next devbox shell (`nexus run`) |
| Terminal layout | next `nexus run` |
| `.cursorrules`, `.github/copilot-instructions.md` | read by those tools on their next start |

Each line counts the files of its area and splits the count into added,
changed and removed when more than one kind occurs, e.g.
`devbox workspace (9: 5 changed, 4 removed)`.

Running Claude Code sessions of this repository get the same summary
through their cross-session inbox (the channel the Dispatch notices use),
so the agent re-reads changed instruction files right away and names the
steps left to you. Sessions whose `crossSessionInbound` is `hold` or
`refuse` get nothing; `--no-notify` sends nothing. The message holds area
names, file paths and plugin ids, never file contents. Credentials and the
project config are not watched.

A session registers for this summary when it starts, through the
`nexus-dispatch-inbox` hook. A session started before that hook arrived is
not registered; reload says so, and a restart with `claude --continue`
registers it without losing the conversation.

`-y` answers questions as in `nexus pull`; edited Claude Code files
(`.claude/` rules, skills, agents and other locked files) are replaced and
modified leftovers removed only with `--force` or `--force-unmanaged`. Other files follow pull's
`-y` handling (for example `.mcp.json` keeps locally added MCP servers; only
`--force` or `--force-unmanaged` rebuilds it from the backend). When
`claude plugin list` is unavailable before or after the pull, reload says
that plugin changes could not be checked instead of reporting every plugin
as new.

```bash
nexus reload                 # pull, update plugins, report, tell running sessions
nexus reload --trust         # also accept changed hooks, MCP servers and plugins
```

### Claude Code Plugins

Projects with executioner `claude-cli` can select Claude Code plugins in
Nexus (e.g. `frontend-design@claude-plugins-official`). They arrive in
`af_export.claude_settings` as `enabledPlugins` (and
`extraKnownMarketplaces` for non-default marketplaces such as
`gatewarden-nexus`) and are merged into `.claude/settings.json` by every
pull. Enabling a plugin there does not install it, so `nexus pull --force`
and `nexus run` (before every launch) also install and update them:

1. `claude plugin list --json` decides what is present: installed at user
   scope, or at project/local scope **for this workspace**.
2. A marketplace a missing plugin needs is added from
   `extraKnownMarketplaces` (`claude plugin marketplace add <repo>#<ref>
   --scope project`) or, if already registered, refreshed.
3. Each missing plugin: `claude plugin install <id> --scope project --json`.
   Never with `--yes` / `--accept-command`: a plugin that declares an
   install command needs a person to confirm it (`/plugin` in Claude Code).
4. A managed plugin installed at project scope for this workspace is
   updated (`claude plugin update <id> --scope project --json`) when the
   registered marketplace checkout offers a newer version and that version
   is approved: each new version is asked once on a terminal and shows as
   `DEFERRED` without a terminal. A marketplace registered from another
   source than the project declares is never refreshed, installed from or
   updated from (`DEFERRED`, with the command to remove it). A `github` or
   `git` marketplace declared with a custom catalog `path` is verified
   against the stored registration (`plugins/known_marketplaces.json` in
   the Claude Code config directory), since the marketplace listing does
   not report that path; when that record is missing, unreadable or names
   another catalog, the marketplace is not used. The marketplace
   registration is shared by every project on the machine, so the step
   never re-points it: when it is registered at another ref than the
   project declares, the plugin shows as `PENDING`. Claude Code moves the
   registration to the declared ref when a session starts; the next
   `nexus run` then updates the plugin before the launch.

```
Claude Code plugins:
  MARKETPLACE gatewarden-nexus   added (project scope)
  INSTALLED   frontend-design@claude-plugins-official   project scope
  PRESENT     nexus-core@gatewarden-nexus
  UPDATED     nexus-core@gatewarden-nexus   1.4.1 -> 1.5.0 (project scope; sessions started from now on use it)
  PENDING     nexus-core@gatewarden-nexus   marketplace registered at nexus-core--v1.4.1, the project declares nexus-core--v1.5.0; ...
  FAILED      foo@claude-plugins-official   Plugin "foo" not found in marketplace ...
```

Nothing is printed when every managed plugin is already present. A failure
never blocks the pull or the launch. `nexus run` installs into the selected
`--account` (`CLAUDE_CONFIG_DIR`). Set `NEXUS_SKIP_PLUGIN_SYNC=1` to skip
the step (CI, offline).

Rules:

- User- and local-scope plugins are never touched; `enabledPlugins` and
  `extraKnownMarketplaces` are merged **per entry**, so plugins you enabled
  at project scope yourself survive a pull.
- What Nexus installed is recorded in the CCX lock
  (`.nexus/claude/manifest.lock.json`, `plugins`). When a plugin drops out
  of the managed list, pull removes its `enabledPlugins` entry only if
  Nexus wrote it and you have not changed it since (shown as `RELEASED`).
  Nothing is uninstalled.
- `nexus status` lists each enabled plugin with `[managed]`, its install
  scope and whether Nexus installed it (`--output json`: `managed`,
  `installed`, `scope`, `installed_by_nexus`).

### Configuration

Global config is stored in `~/.config/nexus/config.toml`.

| Key | Default | Description |
| --- | ------- | ----------- |
| `api_url` | `https://nexus.gatewarden.eu` | Nexus API base URL |
| `default_output` | `table` | Output format: `table`, `json`, `plain` |
| `no_color` | `false` | Disable colored output |
| `mcp_source` | `npm` | MCP server source: `npm` or `local` |
| `check_updates` | `true` | Check for CLI updates on startup |
| `code_signals` | `true` | Send code signals on `nexus link` and `nexus run` (see [Code signals](#code-signals-nexus-code-report)); `NEXUS_NO_CODE_SIGNALS=1` turns them off as well |
| `run.default_tool` | `opencode` | Tool binary launched by `nexus run` when the backend supplies no `run_target` |
| `run.launch_countdown_secs` | `5` | Seconds to count down after pre-launch checks before starting the tool. Set to `0` to skip the countdown and launch immediately. |

The API URL can also be set via the `NEXUS_API_URL` environment variable.
Resolution order: `--api-url` flag > `NEXUS_API_URL` env var > project-local
config > global config.toml > default.

**API URL from a tracked project config.** A project-local
`.nexus/config.toml` that is tracked by git belongs to the
repository, so its `api_url` was chosen by whoever wrote the repository.
Before any command sends your token to such a URL, the CLI checks that you
approved its origin (scheme, host, port): the global `api_url`, the default
above, or an entry of `NEXUS_TRUSTED_API_URLS` (comma-separated).
`--api-url` and `NEXUS_API_URL` are your own choice and always count as
approved. Otherwise the command asks once (`Send your Nexus token to …?`,
default no) and refuses without a terminal or with `--yes`; `nexus login`
included. An approval also covers the commands it starts (for example the
`nexus run` panes). `nexus status` shows such a URL as `(local, tracked by
git)`. Untracked project configs, as written by `nexus link` and
`nexus config set --local`, work as before. When git cannot tell whether
the file is tracked (git missing, a broken repository, refused ownership),
it counts as tracked. A `.nexus/config.toml` or `.nexus` directory that is
a symlink out of the workspace or into `.git` is refused.

```bash
# Temporary staging session (no config change needed)
export NEXUS_API_URL=https://staging.example.com
nexus status
nexus push --dry-run
```

Use `nexus config set K=V` to update a value, e.g.:

```bash
nexus config set run.launch_countdown_secs=3   # shorter countdown
nexus config set run.launch_countdown_secs=0   # launch immediately
```

#### Project-local config (`--local`)

`nexus config` supports two layers, mirroring git's own `--local` /
`--global` config precedence:

- **Global**: `~/.config/nexus/config.toml` (machine-wide default, unchanged
  from previous versions).
- **Local**: `.nexus/config.toml` `[config]` section (this project only).
  Supports `api_url`, `default_output`, and `no_color`.

Any command run inside a directory with a `.nexus/config.toml` prefers the
project-local value over the global one, falling back to global (then the
compiled-in default) for any key the local file doesn't set:

```bash
# Scope a staging API URL to this project only: every other project on
# this machine keeps using the global (e.g. production) api_url.
nexus config set api_url=https://staging.example.com --local

# Explicit global write (same as omitting the flag; symmetry with --local)
nexus config set api_url=https://nexus.gatewarden.eu --global

# See which layer is in effect for each key
nexus config show
#   api_url        = https://staging.example.com (local)
#   default_output = table (global)
#   no_color       = false (default)

# Print a specific layer's file path (unflagged = global, unchanged)
nexus config path            # ~/.config/nexus/config.toml
nexus config path --local    # ./.nexus/config.toml
```

This is purely additive: existing global-only setups are unaffected, and
`nexus config set K=V` without `--local`/`--global` behaves exactly as
before. Use `--local` whenever you regularly juggle multiple projects
across environments (e.g. a staging-only alpha-test repo alongside
production-bound projects) so a wrong `api_url` can't leak between them.


## Project Structure

```
nexus-cli/
├── Cargo.toml          # Workspace root
├── install.sh          # Platform-aware binary installer
├── .github/workflows/
│   ├── ci.yml          # CI: test matrix (ubuntu + macos), clippy, rustfmt
│   └── release.yml     # Release: 4-target cross-compile, checksums, GH release
├── nexusctl/           # Binary crate (builds the `nexus` binary)
│   ├── src/
│   │   ├── main.rs     # CLI entry point (clap derive)
│   │   └── cmd/        # Command implementations
│   └── tests/          # Functional tests: the real binary against a fake backend
│       ├── common/     # Harness: mock API, temporary git workspace, fixtures
│       ├── pull_functional.rs
│       ├── workspace_functional.rs  # push, status, reset, stash, deinit
│       ├── shadow_functional.rs
│       └── init_functional.rs
├── nexus-core/         # Library crate (shared modules)
│   └── src/
│       ├── api/        # HTTP client + API types
│       ├── auth/       # Credential storage (nxs_pat_*)
│       ├── config/     # CLI configuration (~/.config/nexus/)
│       ├── error/      # Unified error types
│       └── hash/       # Shared hashing utilities (sha256_hex)
└── tests/              # Dedicated test crate
    └── src/
        ├── auth_tests.rs
        ├── config_tests.rs
        ├── deinit_tests.rs
        ├── error_tests.rs
        ├── link_tests.rs
        └── types_tests.rs
```

## Build

```bash
cargo build --release
```

> **Note:** On macOS with devbox/nix, set `RUSTFLAGS=""` to avoid linker issues
> with mold.

## Test

```bash
cargo test --workspace
cargo test -p nexusctl --test pull_functional   # one functional suite
```

The functional tests in `nexusctl/tests/` build and run the `nexus` binary.
Each test gets a fresh git repository, its own `HOME` (no real
configuration or credentials are read) and a fake Nexus API on
`127.0.0.1`, so they need no network and no login. They check what a command
writes, keeps, removes and sends, and that dry runs and refused steps leave
the workspace byte for byte unchanged.

## Development

Install the pre-commit hook to run `cargo fmt` and `cargo clippy` before each commit:

```bash
cp hooks/pre-commit .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit
```

## CI/CD

- **CI** (`ci.yml`): Runs on every push/PR to `main`. Tests on ubuntu-latest
  and macos-latest, clippy with `-D warnings`, rustfmt check, and a gitleaks
  self-test (`scripts/ci/gitleaks_selftest.sh`): the repo's `.gitleaks.toml`
  must detect a synthetic token (so a config that drops the built-in rules
  fails the build) and find nothing in the source tree.
- **Release** (`release.yml`): Triggered by `v*` tags. Builds release binaries
  for 4 targets (aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu,
  aarch64-unknown-linux-gnu), generates SHA-256 checksums, and creates a GitHub
  Release with all assets attached.

## Tips

> **Local changes are protected by default.**
>
> `nexus pull` checks if local files were modified since the last pull.
> Modified files are **skipped** with a warning. Use `nexus stash` to save
> changes before pulling, or `nexus pull --force` to overwrite.
>
> **Pushing changes back to the platform:**
> `nexus push` uploads modified content: agent files (e.g. AGENTS.md) and
> workspace files (devbox.json, scripts/, as a new workspace fork). Generated
> files are changed through their setting (`nexus env set`) instead.

- Run `nexus pull` periodically (or after skill/agent file changes in the dashboard) to keep your workspace in sync. While a Claude Code session runs, `nexus reload` in a second pane does the same and tells the session what changed; leaving the session or the terminal workspace is only needed for `.mcp.json`, devbox and layout changes, and `claude --continue` keeps the conversation across such a restart.
- Start the agent environment with `nexus run`, then use `/nexus-init` inside OpenCode or Claude Code to bootstrap the agent.
- Each project uses exactly one runtime (`agent_owner`: OpenCode or Claude Code); `nexus pull` only writes that runtime's files. Run `nexus link` to see which runtime is configured. When the backend cannot report the owner (project detail unreachable, or no owner while Claude Code is cached), pull keeps the owner cached in `.nexus/config.toml` and removes nothing of the other runtime.
- Switching `agent_owner` is safe at any time: the next `nexus pull` writes the new runtime's projection and then removes what Nexus had put on disk for the old one (OpenCode: `.opencode/` commands, plugins, install artifacts, the Nexus parts of `opencode.json`; Claude Code: CCX files and hooks, rendered skills and agents, the Nexus keys in `.claude/settings.json`, the Nexus servers in `.mcp.json`, the `CLAUDE.md` managed block). Locally modified Nexus files are kept and listed; `nexus pull --force` removes them too (for OpenCode the whole `.opencode/`). Files Nexus never wrote under `.claude/` and `.claude/settings.local.json` are never touched. Switching back recreates everything.
- Committed workspace files are protected: when `devbox.json` or a script equals `HEAD`, differs from what Nexus last delivered and the workspace fork has a different version, `nexus pull` keeps it and suggests `nexus diff`, then `nexus push` (publish the committed version) or `nexus pull --force` (take the fork version, with a warning to review `git diff`).
- If the repo ships `.githooks/pre-commit`, `nexus pull` offers to set `git config core.hooksPath .githooks` when it is unset (a value set on purpose is never overridden, only hinted) and warns when the hook needs `gitleaks` but it is not installed. Hooks run code on every commit, so they are activated only after a confirmation on a terminal (default no) or with `--trust`; a non-interactive pull leaves them inactive and prints how to enable them.

## Cost Control Tools

`nexus pull` automatically configures two token-saving tools when enabled for a project:

### RTK: Runtime Token Killer

Filters and compresses CLI output before it reaches the agent context (60–90% savings on build/test/lint output).

```bash
# Install the RTK binary
# See: https://www.rtk-ai.app/#install

# Install the OpenCode plugin (once per machine)
rtk init -g --opencode

# After nexus pull, trust project filters
rtk trust
```

The `.rtk/filters.toml` file is auto-generated from the project's **codebase character** presets (configured in the Nexus dashboard under Project → Plugins) and synced on every `nexus pull`. An OS/shell baseline (git, find, grep, curl, ssh) is always included.

If RTK is not installed when you run `nexus pull`, the CLI prints a warning:

```
   ! rtk not found in PATH
     Required by: RTK output filtering (codebase: rust, docker, make)
     Install: Install RTK: https://www.rtk-ai.app/#install, then run: rtk init -g --opencode
```

### Headroom: Context Compression MCP

ML-based context compression via MCP server. Reduces input context by 60–95% for large tool outputs.

```bash
# Install headroom
pip install "headroom-ai[mcp]"
# or
pipx install "headroom-ai[mcp]"
```

The Headroom MCP server (`headroom mcp serve`) is configured automatically in `opencode.json` when the headroom plugin is active. It provides three tools: `headroom_compress`, `headroom_retrieve`, `headroom_stats`.

## Related

- [Gatewarden Nexus](https://nexus.gatewarden.eu): Platform (Next.js/Supabase)
- [nexus-mcp](https://github.com/gwnexus/nexus-mcp): MCP server (38 tools, 4 layers)

## License

[Apache-2.0](LICENSE) -- Copyright (c) 2026 RelicFrog Holding UG (haftungsbeschraenkt)
