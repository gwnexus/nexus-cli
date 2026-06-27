# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-06-27

### Added

- **`nexus init`: no-workspace advisory prompt** — when `nexus init` is run
  against a project that has no workspace (devbox fork) configured in the
  backend, the CLI now displays a clear advisory block explaining that this is
  an unusual configuration, and prompts `Understood — continue without
  workspace? [y/N]`. Answering `N` (the default) aborts with `Aborted.` and
  gives actionable recovery instructions:
  1. Add a workspace in the Nexus backend project settings, then
     re-run `nexus init`; **or**
  2. Run `nexus pull --force` after the workspace has been added.
  The prompt is TTY-gated (non-interactive / CI contexts print the advisory
  and continue without prompting). The `--force` / `-y` flag bypasses the
  prompt entirely, consistent with all other advisory prompts in the CLI.
  (ADR-0012)

### Fixed

- **macOS build: mold linker flag removed from `devbox.json` env** — `mold`
  is a Linux-only linker. Previously `devbox.json` set
  `RUSTFLAGS="-C link-arg=-fuse-ld=mold"` globally, which leaked into macOS
  shell environments on devbox activation and caused all Cargo builds (including
  agent-triggered builds from OpenCode) to fail with
  `clang: error: invalid linker name in argument '-fuse-ld=mold'`.
  `RUSTFLAGS` is removed from `devbox.json`; mold is now configured via
  per-target `[target.*]` entries in `.cargo/config.toml` (Linux targets only).
  macOS uses the system linker. `RUSTC_WRAPPER=sccache` is retained in
  `devbox.json` (sccache is cross-platform). (ADR-0013)

## [0.7.5] - 2026-06-25

### Fixed

- **`NEXUS_SEC_OPENAI_API_KEY` resolved directly from `.env.nexus.local`**:
  `nexus pull` and `nexus init` now read `NEXUS_SEC_OPENAI_API_KEY` directly
  from `.env.nexus.local` (or `.env.local` as fallback) in the workspace root,
  without requiring the variable to be exported into the shell environment first.
  Resolution order: shell env → `.env.nexus.local` → `.env.local` → `{env:}`
  template fallback. No more `set -a && source .env.nexus.local` required.

## [0.7.4] - 2026-06-25

### Fixed

- **`NEXUS_SEC_OPENAI_API_KEY` environment resolution**: `nexus pull` now
  correctly writes the literal key value into `opencode.json` when the variable
  is present in the environment at pull time. Verified that the `{env:}`
  fallback path is only used when the variable is genuinely absent. No logic
  change — release tracks the confirmed behaviour and aligns the installed
  binary version with the `opencode.json` generation fix shipped in v0.7.3.

## [0.7.3] - 2026-06-25

### Fixed

- **`NEXUS_SEC_OPENAI_API_KEY` resolved at write time**: `nexus pull` and
  `nexus init` now read `NEXUS_SEC_OPENAI_API_KEY` from the current shell
  environment (e.g. sourced from `.env.nexus.local`) and write the **literal
  value** into `opencode.json`. Previously the `{env:}` template was written,
  which OpenCode cannot expand when the variable is not set at startup time.
  Falls back to `{env:NEXUS_SEC_OPENAI_API_KEY}` when the variable is absent.

## [0.7.2] - 2026-06-25

### Added

- **Semantic search support**: `nexus pull` and `nexus init` now write
  `NEXUS_SEC_OPENAI_API_KEY` as an `{env:NEXUS_SEC_OPENAI_API_KEY}` template
  reference into the nexus MCP server environment block in `opencode.json`.
  This enables pgvector-based semantic and hybrid knowledge search via the
  Nexus MCP (`kb_search` with `search_mode=semantic` or `hybrid`). The key
  is resolved at runtime from the shell environment — never stored as a
  literal value.

## [0.7.1] - 2026-06-22

### Added

- **Prerequisites check**: `nexus pull` now checks for required external binaries
  (e.g. `rtk`, `headroom`) after the plugin install step. For each missing
  prerequisite, the CLI prints a clear warning with the tool name, which plugin
  requires it, and an install hint. Pull succeeds regardless — the check is
  informational only.
- **`auth_token` echo**: `nexus pull` now uses the `auth_token` field from the
  `af_export` response (if present) to write the PAT directly into `opencode.json`,
  without a separate read of `~/.config/nexus/credentials.toml`. Falls back to
  the token already in memory when the server does not provide the field (older hub
  versions).
- **`Prerequisite` struct** in `nexus-core` API types — deserialised from the
  `prerequisites` array in `af_export` responses.

## [0.7.0] - 2026-06-15

### Added
- Provider support: `nexus pull` consumes provider configuration from `af_export`
  and writes a `providers` block to `opencode.json` — DGX Spark auto-mapped as
  `dgx-spark` provider with `@ai-sdk/openai-compatible` type
- Init prompt fix: `nexus init` no longer prompts for API URL when the global
  config file (`~/.config/nexus/config.toml`) already exists

### Fixed
- Security: updated `rustls-webpki` to 0.103.13, resolving RUSTSEC-2026-0098,
  RUSTSEC-2026-0099, and RUSTSEC-2026-0100 — `cargo audit` clean
- Suppressed update-check banner after successful `nexus upgrade`

### Changed
- 237 tests passing (10 new provider tests)

## [0.6.13] - 2026-05-30

### Added
- Built-in plugin registry (`resolve_platform_plugins`) mapping platform plugin
  slugs to GitHub raw download URLs for automatic installation
- `nexus init`: auto-downloads platform-selected plugins from `af_export` response
  (`nexus-compaction-plus`, `nexus-cost-control`) into `.opencode/plugins/`
- `nexus pull`: downloads missing platform plugins on every pull; skips existing
  files unless `--force` is set
- Unit tests for `resolve_platform_plugins` (known slugs, unknown slugs, partial
  match, empty input — 227 tests total passing)

### Fixed
- Update-check banner no longer shown after a successful `nexus upgrade` —
  the cache is stamped with the newly installed version so the banner is
  suppressed for the remainder of the process and the next 24 h cache window



## [0.6.12] - 2026-05-27

### Added
- Background update check via GitHub API (24h cache, 3s timeout, never blocks CLI)
- `check_updates` config option to enable/disable update notifications
- Clippy lint checks in pre-commit hook

### Fixed
- 223 tests passing

## [0.6.11] - 2026-05-21

### Added
- Machine registry support (MCP v0.8.8, migration 0094)
- Session metadata display (agent model, toolstack, machine info)

## [0.6.10] - 2026-05-18

### Added
- Git Identity Guard: `nexus git verify|apply` commands
- Per-project git identity storage (user.name, user.email, user.signingkey, commit.gpgsign)
- Auto-apply git identity on `nexus init` and `nexus pull`

## [0.6.9] - 2026-05-15

### Added
- Shadow mode: `nexus shadow on|off|status` manages .git/info/exclude for agentic files

## [0.6.8] - 2026-05-12

### Added
- Smart agent file generation and sync protocol

## [0.6.7] - 2026-05-10

### Fixed
- Workspace pull with PAT-authenticated export

## [0.6.6] - 2026-05-08

### Added
- Workspace 2.0 blueprint + fork architecture

## [0.6.5] - 2026-05-05

### Added
- `nexus import` command for onboarding existing configurations
- Session metadata support

### Changed
- Shadow mode deprecated in favor of automatic .git/info/exclude management (later un-deprecated in 0.6.9)

## [0.6.4] - 2026-05-02

### Added
- Project tasks support
- CLI machine-ID generation and tracking

### Changed
- Security hardening across API communication

## [0.6.3] - 2026-04-28

### Added
- Skill resources support
- Auto-generated frontmatter in exported skills

## [0.6.2] - 2026-04-25

### Changed
- License tier refactoring and per-user licensing support

## [0.6.1] - 2026-04-22

### Added
- PDF export support for ADRs and agent skill files

## [0.6.0] - 2026-04-18

### Added
- Security hardening (TLS, credential management)
- PDF export pipeline
- Classification UX improvements

### Changed
- Performance optimizations across API calls
