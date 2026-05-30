# Nexus CLI

[![CI](https://github.com/gwnexus/nexus-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/gwnexus/nexus-cli/actions/workflows/ci.yml)
[![Release](https://github.com/gwnexus/nexus-cli/actions/workflows/release.yml/badge.svg)](https://github.com/gwnexus/nexus-cli/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
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
nexus pull [--project-id <id>]          Pull skills and config from the Nexus platform
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
| `nexus pull`    | `--project-id`   | Pull from a specific project (skip picker)     |

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
│       └── error/      # Unified error types
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

> **Local agent file changes are overwritten by `nexus pull`.**
>
> Files managed by the Nexus platform (AGENTS.md, CLAUDE.md, .cursorrules,
> copilot-instructions.md, plugin configs, etc.) are pulled from the server
> and will **overwrite** any local modifications. `nexus pull` warns before
> overwriting, but with `--force` changes are lost silently.
>
> **Pushing local changes back to the platform is not yet supported.**
> If you need custom content, edit the agent files in the Nexus dashboard
> and run `nexus pull` to sync them to your workspace.

- Run `nexus pull` periodically (or after skill/agent file changes in the dashboard) to keep your workspace in sync.
- Use `/nexus-init` inside OpenCode or Claude CLI to bootstrap the agent after initialization.
- Each project is optimized for a specific tool flavor (OpenCode, Claude CLI, or both). Run `nexus link` to see which flavor is configured.

## Related

- [Gatewarden Nexus](https://nexus.gatewarden.eu) — Platform (Next.js/Supabase)
- [nexus-mcp](https://github.com/gwnexus/nexus-mcp) — MCP server (38 tools, 4 layers)

## License

[MIT](LICENSE) -- Copyright (c) 2026 RelicFrog Holding UG (haftungsbeschraenkt)
