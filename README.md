# nexus-cli

Rust CLI for the [Nexus](https://nexus.gatewarden.eu) platform. Provides
project scaffolding, authentication, environment preflight checks, and
configuration management.

## Install

### One-liner (recommended)

Requires a GitHub PAT with `repo` scope for the private release artifacts:

```bash
curl -fsSL https://nexus.gatewarden.eu/install.sh | bash
```

The installer detects your platform (macOS/Linux, x86_64/aarch64), downloads
the pre-built binary from GitHub Releases, and verifies its SHA-256 checksum.
If no binary exists for your platform it falls back to `cargo install --git`.

> **Note:** Releases v0.1.3 and v0.1.4 contain installer URLs that point to
> `raw.githubusercontent.com`, which is not accessible for private repos. If you
> are on one of these versions, please upgrade to **v0.1.5+** where the
> installer is served via CDN.

### From source

```bash
cargo install --git https://github.com/gwnexus/nexus-cli.git nexusctl
```

## Commands

```
nexus init [path]                       Initialize a Nexus project workspace
nexus init [path] --shadowed-ai         Init and add all AI files to .gitignore
nexus login                             Authenticate with the Nexus platform
nexus logout                            Remove stored credentials
nexus status                            Show auth, project, and workspace status
nexus link [--project-id <id>]          Bind a project to the current workspace
nexus unlink                            Remove project binding from the workspace
nexus pull [--project-id <id>]          Pull skills and config from the Nexus platform
nexus skills export [--project-id <id>] Export enabled skills as JSON
nexus preflight                         Run environment readiness checks
nexus deinit [--force]                  Remove all AI scaffold files from the workspace
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
| `nexus init`    | `--shadowed-ai`  | Append all AI scaffold files to `.gitignore`   |
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

## Workspace Structure

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

## CI/CD

- **CI** (`ci.yml`): Runs on every push/PR to `main`. Tests on ubuntu-latest
  and macos-latest, clippy with `-D warnings`, and rustfmt check.
- **Release** (`release.yml`): Triggered by `v*` tags. Builds release binaries
  for 4 targets (aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu,
  aarch64-unknown-linux-gnu), generates SHA-256 checksums, and creates a GitHub
  Release with all assets attached.

## Build

```bash
cargo build --release
```

## Tips

> **:warning: Local agent file changes are overwritten by `nexus pull`.**
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

## Known Issues

- **No push support for agent files.** Local edits to managed files (AGENTS.md, CLAUDE.md, etc.) cannot be synced back to the platform. Edit in the dashboard instead.
- **`nexus pull` overwrites without merge.** There is no diff/merge strategy — the server version always wins. Back up local changes before pulling if needed.
- **macOS build requires `RUSTFLAGS=""`** when using devbox/nix (the default `RUSTFLAGS=-C link-arg=-fuse-ld=mold` does not work on macOS).

## Development

Install the pre-commit hook to run `cargo fmt` automatically before each commit:

```bash
cp hooks/pre-commit .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit
```

## Test

```bash
cargo test --workspace
```

## Related

- [nexus](https://github.com/gwnexus/nexus-hub) — Backend + Frontend (Next.js/Supabase/Netlify)
- [nexus-mcp](https://github.com/gwnexus/nexus-mcp) — MCP server (38 tools, 4 layers)

## License

Nexus is a product of the Nexus product group, owned by
RelicFrog Holding UG (haftungsbeschränkt).
