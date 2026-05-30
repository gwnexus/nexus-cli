# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
