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
the pre-built binary from GitHub Releases, and verifies its SHA-256 checksum.
If no binary exists for your platform it falls back to `cargo install --git`.

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
```

## Commands

```
nexus init [path]                       Initialize a Nexus project workspace
nexus login                             Authenticate with the Nexus platform
nexus logout                            Remove stored credentials
nexus status                            Show auth, project, and workspace status
nexus link [--project-id <id>]          Bind a project to the current workspace
nexus unlink                            Remove project binding from the workspace
nexus project link [--runtime-id <n>]   Issue a project inference token (nxs_proj_*) for gateway auth
nexus project rotate [--finalize]       Rotate the project inference token (zero-downtime overlap)
nexus project unlink                    Revoke the project inference token and clear local state
nexus project status                    List issued project inference tokens
nexus pull [--project-id <id>]          Pull skills and config from the Nexus platform
nexus push [--name "..."] [--dry-run]   Push workspace changes as a new fork
nexus stash save                        Save modified workspace files to a stash
nexus stash pop                         Restore the most recent stash
nexus stash list                        List all available stashes
nexus skills export [--project-id <id>] Export enabled skills as JSON
nexus preflight                         Run environment readiness checks
nexus deinit [--force]                  Remove all AI scaffold files from the workspace
nexus shadow on|off|status              Manage Git exclusion of workspace agentic files
nexus config show                       Display configuration
nexus config set K=V                    Update a configuration value
nexus config path                       Show the config file path
nexus upgrade                           Upgrade CLI to latest release version
```

### Global Flags

| Flag         | Short | Description                                     |
| ------------ | ----- | ----------------------------------------------- |
| `--yes`      | `-y`  | Non-interactive mode (auto-confirm all prompts) |
| `--verbose`  | `-v`  | Enable verbose output                           |
| `--help`     | `-h`  | Show help                                       |
| `--version`  | `-V`  | Show version                                    |

### Notable Flags

| Command         | Flag             | Description                                    |
| --------------- | ---------------- | ---------------------------------------------- |
| `nexus init`    | `--yes`          | Skip interactive project selection             |
| `nexus deinit`  | `--force`        | Skip confirmation prompt                       |
| `nexus deinit`  | `--yes`          | Auto-confirm removal                           |
| `nexus link`    | `--project-id`   | Specify project UUID directly (skip picker)    |
| `nexus project link` | `--runtime-id` | Logical runtime name for the token (default: hostname) |
| `nexus project link` | `--expires`  | Token lifetime: 30d, 12h, 2w, or an ISO 8601 timestamp |
| `nexus project link` | `--restrict-profiles` | Restrict the token to profile slugs (comma-separated) |
| `nexus project rotate` | `--finalize` | Revoke the previously superseded token after overlap |
| `nexus pull`    | `--project-id`   | Pull from a specific project (skip picker)     |
| `nexus push`    | `--name`         | Custom fork name (default: auto-generated)     |
| `nexus push`    | `--dry-run`      | Show what would be pushed without sending      |
| `nexus push`    | `--workspace`    | Only push workspace files (default for now)    |

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
nexus project unlink                        # revoke and clear the local token
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

### Workspace Sync (Push / Stash)

`nexus push` detects local changes to workspace files (`devbox.json`,
`scripts/devbox/`) and uploads them as a new workspace fork to the linked
Nexus project.

```bash
nexus push                          # detect changes + push
nexus push --name "v3.1 ansible"    # push with custom fork name
nexus push --dry-run                # show what would be pushed
```

Each push **archives the current active fork** and creates a new one. The
fork name is either auto-generated (`push-2026-08-23T10-30-00`) or set
via `--name`. Previous forks remain accessible in the dashboard.

`nexus stash` provides temporary local backup before a `nexus pull --force`:

```bash
nexus stash save    # save modified workspace files to .nexus/stash/
nexus stash pop     # restore the most recent stash
nexus stash list    # show all available stashes
```

Stashes are stored locally in `.nexus/stash/<timestamp>/` with metadata.
Multiple stashes can coexist; `pop` always restores the most recent one.

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

`nexus run` embeds the same checks and adds a **launch countdown** after
they complete. The countdown gives you a moment to review the results before
the tool starts. Press `Ctrl+C` at any time to abort.

### Configuration

Global config is stored in `~/.config/nexus/config.toml`.

| Key | Default | Description |
| --- | ------- | ----------- |
| `api_url` | `https://nexus.gatewarden.eu` | Nexus API base URL |
| `default_output` | `table` | Output format: `table`, `json`, `plain` |
| `no_color` | `false` | Disable colored output |
| `mcp_source` | `npm` | MCP server source: `npm` or `local` |
| `check_updates` | `true` | Check for CLI updates on startup |
| `run.default_tool` | `opencode` | Tool binary launched by `nexus run` |
| `run.launch_countdown_secs` | `5` | Seconds to count down after pre-launch checks before starting the tool. Set to `0` to skip the countdown and launch immediately. |

The API URL can also be set via the `NEXUS_API_URL` environment variable.
Resolution order: `--api-url` flag > `NEXUS_API_URL` env var > config.toml > default.

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

## Project Structure

```
nexus-cli/
├── Cargo.toml          # Workspace root
├── install.sh          # Platform-aware binary installer
├── .github/workflows/
│   ├── ci.yml          # CI: test matrix (ubuntu + macos), clippy, rustfmt
│   └── release.yml     # Release: 4-target cross-compile, checksums, GH release
├── nexusctl/           # Binary crate (builds the `nexus` binary)
│   └── src/
│       ├── main.rs     # CLI entry point (clap derive)
│       └── cmd/        # Command implementations
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
```

## Development

Install the pre-commit hook to run `cargo fmt` and `cargo clippy` before each commit:

```bash
cp hooks/pre-commit .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit
```

## CI/CD

- **CI** (`ci.yml`): Runs on every push/PR to `main`. Tests on ubuntu-latest
  and macos-latest, clippy with `-D warnings`, and rustfmt check.
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
> **Pushing workspace changes back to the platform:**
> Use `nexus push` to upload modified workspace files (devbox.json, scripts/)
> as a new workspace fork. Agent file push (skills, AGENTS.md) is planned
> for a future release.

- Run `nexus pull` periodically (or after skill/agent file changes in the dashboard) to keep your workspace in sync.
- Use `/nexus-init` inside OpenCode or Claude CLI to bootstrap the agent after initialization.
- Each project is optimized for a specific tool flavor (OpenCode, Claude CLI, or both). Run `nexus link` to see which flavor is configured.

## Cost Control Tools

`nexus pull` automatically configures two token-saving tools when enabled for a project:

### RTK — Runtime Token Killer

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
     Install: Install RTK: https://www.rtk-ai.app/#install — then run: rtk init -g --opencode
```

### Headroom — Context Compression MCP

ML-based context compression via MCP server. Reduces input context by 60–95% for large tool outputs.

```bash
# Install headroom
pip install "headroom-ai[mcp]"
# or
pipx install "headroom-ai[mcp]"
```

The Headroom MCP server (`headroom mcp serve`) is configured automatically in `opencode.json` when the headroom plugin is active. It provides three tools: `headroom_compress`, `headroom_retrieve`, `headroom_stats`.

## Related

- [Gatewarden Nexus](https://nexus.gatewarden.eu) — Platform (Next.js/Supabase)
- [nexus-mcp](https://github.com/gwnexus/nexus-mcp) — MCP server (38 tools, 4 layers)

## License

[Apache-2.0](LICENSE) -- Copyright (c) 2026 RelicFrog Holding UG (haftungsbeschraenkt)
