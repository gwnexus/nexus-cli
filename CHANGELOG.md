# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.15.0] - 2026-09-03

### Added
- **`nexus project link` — project inference tokens (`nxs_proj_*`)** - new command group that provisions project-scoped inference credentials for the Nexus Model Gateway (gateway ADR-0005), bootstrapped from the user PAT. Subcommands:
  - `nexus project link` issues a token for the linked project (or `--project-id`), defaulting the logical `--runtime-id` to the hostname. Supports `--expires` (relative `30d`/`12h`/`2w` or ISO 8601) and `--restrict-profiles` (tighten-only profile ceiling). Shortcuts: `--rotate`, `--status`.
  - `nexus project rotate` performs a zero-downtime rotation (previous token stays valid); `--finalize` revokes the superseded token after the overlap window.
  - `nexus project unlink` revokes the token(s) server-side and clears local state.
  - `nexus project status` lists issued tokens (runtime, prefix, created/last-used/expiry, status) and never prints secrets.
- **Project token storage** - tokens are stored in `~/.config/nexus/project-tokens.toml` (mode `0600`, never committed) and exposed to tools as `NEXUS_PROJECT_TOKEN` via the gitignored `.env.nexus.local`. The `NEXUS_PROJECT_TOKEN` environment variable overrides the store (CI-friendly). Existing linked workspaces need no relink: `nexus project link` reuses the project from `.nexus/config.toml`.
- **API client** - `issue_inference_token`, `rotate_inference_token`, `revoke_inference_token`, and `list_inference_tokens` methods against `/api/projects/:id/inference-tokens`, plus a generic authenticated `DELETE` helper.

## [0.14.5] - 2026-08-24

### Fixed
- **Workspace files now tracked in sync-manifest after pull** - `nexus pull` writes workspace files (devbox.json, scripts/devbox/*) but did not add them to `.nexus/sync-manifest.json`. This caused the `nexus push` origin guard to always block workspace pushes ("not tracked by Nexus"). Both v2 (ws_export) and v1 (wf_export) code paths now track pulled workspace files in the manifest with their content hashes.

## [0.14.4] - 2026-08-23

### Fixed
- **`nexus status` ignored `NEXUS_PRIVATE_TOKEN` env var** - the status command used `Credentials::load()` directly instead of `resolve_token()`, bypassing the env var override. Now uses the same resolution order as all other commands (env var > credentials.toml). Token source is shown as `(env)` when resolved from environment.
- **`nexus preflight` ignored `NEXUS_PRIVATE_TOKEN` env var** - same fix applied to the credentials check in preflight. Shows `(env)` indicator when token comes from environment.

## [0.14.3] - 2026-08-23

### Fixed
- **CI: `make_latest` on GitHub Release** - release workflow now explicitly marks non-prerelease builds as `latest`, fixing `nexus upgrade` and `install.sh` resolving stale versions when multiple releases are published in quick succession.

## [0.14.2] - 2026-08-23

### Added
- **Origin guard for `nexus push`** - push now requires a sync manifest (`.nexus/sync-manifest.json`) to exist, proving that `nexus pull` was run at least once. Only files tracked in the manifest are included in the push; untracked local files are skipped with a warning. Prevents pushing arbitrary local files to Nexus workspace forks.

## [0.14.1] - 2026-08-23

### Added
- **`NEXUS_API_URL` env var override** — API URL is now resolved as: `--api-url` flag > `NEXUS_API_URL` env var > config.toml > default. Enables temporary staging testing without permanent config changes.
- **`nexus pull` local modification warning** — before overwriting agent files or workspace files, pull now checks if the local file was modified since the last pull (via sync-manifest hash comparison). Modified files are skipped with a warning; use `--force` to overwrite or `nexus stash` to save changes first.
- **`nexus_core::hash` module** — shared `sha256_hex()` utility extracted from duplicate implementations across push, stash, and sync commands.
- **Integration tests** for `WorkspacePushResponse` and `FileStatusResponse` deserialization (roundtrip + edge cases).

### Fixed
- **`chrono_lite_timestamp()`** — correct date calculation with proper leap year handling and per-month day lengths (was using approximate `days / 365` and `remaining_days / 30`).
- **Hash key format** — script file hashes now use workspace-relative paths (`scripts/devbox/init.sh`) instead of the `script:` prefix, matching the `af_status` server endpoint format.
- **Silent file read failures** — push and stash commands now log `tracing::warn!()` when file reads fail instead of silently skipping.
- **`nexus stash` default subcommand** — running `nexus stash` without a subcommand now defaults to `nexus stash save`.
- Removed unused `_workspace_only` parameter from `push::run()`.
- Removed direct `sha2` dependency from `nexusctl` (uses `nexus-core` re-export).

## [0.14.0] - 2026-08-23

### Added
- **`nexus push`** — push local workspace changes (devbox.json, scripts/devbox/) to the linked Nexus project as a new workspace fork. Archives the current active fork automatically. Supports `--name` for custom fork naming, `--dry-run` for preview, `--workspace` flag (default scope for this release).
- **`nexus stash save`** — save modified workspace files to a local stash (`.nexus/stash/<timestamp>/`). Detects modifications by comparing SHA-256 hashes against the sync manifest from the last pull.
- **`nexus stash pop`** — restore the most recent stash and remove it from the stash directory.
- **`nexus stash list`** — list all available stashes with timestamps and file counts.
- **API: `workspace_push`** — new client method calling `ws_push` action on the MCP agent-files endpoint. Creates a new fork with pushed devbox.json and script_files.
- **API: `file_status`** — new client method calling `af_status` action. Compares local file hashes against server-side content hashes, returns categorized diff (modified, new_local, deleted_local, unchanged).
- **API types** — `WorkspacePushResponse`, `FileStatusResponse`, `StatusModifiedFile`, `StatusNewFile`, `StatusDeletedFile`, `StatusUnchangedFile` response structs for the new endpoints.

### Changed
- Version bumped to 0.14.0 — bidirectional workspace sync milestone.

## [0.13.5] - 2026-07-29

### Security

- **SEC-001: Path traversal protection in `sync.rs`** — added `ParentDir`
  component validation to `sync::status()` and `sync::reset()` functions.
  Server-supplied `target_path` values from the sync manifest are now rejected
  if they contain `..` traversal components, matching the existing guard in
  `write_agent_file`. (CWE-22, OWASP A03)

- **SEC-002: Token-bearing config files auto-excluded from git** — `nexus pull`
  and `nexus init` now automatically add `opencode.json`, `opencode.jsonc`, and
  `<agentic_root>/mcp.json` to `.git/info/exclude` after writing PAT tokens
  into these files. This prevents accidental credential commits without
  requiring manual `nexus shadow on`. (CWE-312, OWASP A02)

- **SEC-003: Install script integrity verification in `nexus upgrade`** — the
  upgrade command now downloads the install script to a temp file and verifies
  its SHA-256 checksum against a `.sha256` sidecar file before execution.
  If the checksum file is unavailable, a warning is shown but execution
  proceeds (graceful degradation). (CWE-494, OWASP A08)

- **SEC-004: Prerequisite check command sanitization** — server-supplied
  `check_command` strings from the agent-file export are now validated against
  an allowlist of safe characters before shell execution. Commands containing
  shell metacharacters are skipped. (CWE-78, OWASP A03)

- **SEC-005: Reduced token exposure in preflight output** — the credential
  check now shows only the first 8 and last 4 characters of the PAT token
  (e.g., `nxs_pat_...4567`) instead of the first 16 characters.
  (CWE-200, OWASP A02)

## [0.13.2] - 2026-07-14

### Fixed

- **Spurious update notification after `nexus upgrade`** — the update-available
  banner was displayed after a successful upgrade because the check ran against
  the old binary's cached version data. The notification is now suppressed when
  the active command is `nexus upgrade`.

## [0.13.1] - 2026-07-13

### Fixed

- **`nexus shadow on` no longer silently corrupts the git index** — previously,
  `git rm --cached` was called unconditionally for every matched file, including
  files that had commit history on the current branch. This staged deletions
  that would be committed on the next `git commit`, causing silent data loss.

  The fix introduces a per-file guard in `untrack_patterns`: before calling
  `git rm --cached`, each resolved path is checked via `git log --oneline -1 --
  <file>`. If the file has commit history, it is skipped with a warning:

  ```
  warning: skipping '<file>' — file has commit history on this branch
           (use 'git rm --cached <file>' manually if intended)
  ```

  Files that are staged-only (in the index but never committed) continue to be
  removed from the index as before.

  The same guard applies to `nexus shadow workspace on`.



### Added

- **Configurable launch countdown** — `nexus run` now displays a per-second
  countdown after the pre-launch checks complete before starting the tool.
  The default is 5 seconds; the user can abort at any time with `Ctrl+C`.
  Replaces the previous "Press Enter to launch" prompt for the all-pass and
  warnings-only cases.

  Configurable via `~/.config/nexus/config.toml`:

  ```toml
  [run]
  launch_countdown_secs = 5   # default; set to 0 to launch immediately
  ```

  Or via CLI:

  ```bash
  nexus config set run.launch_countdown_secs=3
  nexus config set run.launch_countdown_secs=0   # skip countdown
  ```

## [0.12.1] - 2026-07-10

### Fixed

- **headroom JSONL timestamp parsing** — `read_headroom_stats()` failed to parse
  ISO 8601 timestamps (`"2026-07-09T15:18:06.407Z"`) and fell back to
  `run_start_epoch`, causing all headroom entries to be treated as "too old".
  Introduced `iso8601_to_unix_secs()` — a dependency-free parser for the UTC
  subset used by headroom-intercept.

- **Token usage "unavailable" regression** — Session entries lookup used a
  chained `and_then(|_| ...)` that always returned `None` after the first failed
  path. Now uses defensive multi-path traversal: `.entries`, `.document.entries`,
  `.session.entries`. Error message improved:
  `unavailable (nexus-cost-control plugin not active for this project)`.

### Added

- **Pre-launch spinner** — `nexus run` now shows an animated `indicatif` spinner
  (`⠋ Running Nexus pre-launch checks...`) while collecting check results, then
  clears it before printing the result table. Improves perceived responsiveness
  on slow filesystems or API calls.

- **Activity Stats: ingested documents** — `research_added` session entry type
  is now counted and displayed as `Docs: N ingested` in the post-session
  Nexus Activity block.

## [0.12.0] - 2026-07-09

### Added

- **Pre-launch confirmation prompt** — after all pre-launch checks pass (or only
  produce warnings), `nexus run` now pauses with `Press Enter to launch <tool>,
  or Ctrl+C to abort...` before starting the tool. Gives the user a chance to
  review the check results.

- **`--force` / `-f` flag** — skip the confirmation prompt and launch immediately
  after checks (non-interactive/CI mode). Also skips pre-launch checks entirely
  (equivalent to `--skip-checks --force`).

- **Post-session token usage** — after the tool exits, `nexus run` queries the
  Nexus backend session API (`session_list` + `kb_get`) and displays the latest
  token cost snapshot: input/output/cache tokens, total, and estimated cost in USD.
  Shows `unavailable (no session data)` if no cost entry exists.

- **Post-session Nexus activity stats** — session entries are counted by type:
  ADRs created/accepted, tasks created/completed, dispatches sent/replied, notes.
  Displayed in the summary when any activity occurred during the session.

- **`NexusClient::list_sessions()`** — new API method for `POST /api/mcp/sessions`
  with `session_list` action.

- **`NexusClient::get_session()`** — new API method for `POST /api/mcp/kb` with
  `kb_get` action (entity_type=session).

### Changed

- **Pre-launch checks: warn/pass paths now prompt** — previously only failures
  prompted for confirmation. Now all outcomes (pass, warn) prompt unless `--force`
  is set.

## [0.11.1] - 2026-07-09

### Added

- **Post-session headroom stats** — `nexus run` now reads `.nexus/headroom-intercept.jsonl`
  after the tool exits and displays compression statistics: mode, compressions,
  local transforms, tokens saved, observations, skips, passthroughs, and cache
  integrity failures. Entries are filtered by session start time to show only
  stats from the current run.

- **Token usage hint** — post-session summary now shows `recorded in Nexus session
  (nexus pull to sync)` instead of a generic placeholder. Full cost readback from
  session entries will follow in v0.12.0 (dispatch 9ae65f3a, approach A2).

## [0.11.0] - 2026-07-09

### Added

- **`nexus run` — pre-launch checks** — before launching the tool, `nexus run` now
  verifies workspace state, authentication, MCP config, plugin env vars, tool binary
  availability, and headroom mode. Checks are displayed in a formatted table. On
  failure, the user is prompted to continue or abort. Skip with `--skip-checks`.

- **`nexus run` — post-session summary** — after the tool exits, prints a summary
  including session duration, exit code, git activity (commits, file changes), and
  newly created release tags. Token usage and headroom stats sections are placeholders
  pending nexus-app integration (dispatch 9ae65f3a).

- **`nexus run --exec` flag** — opt-in for the previous `exec()` behaviour (replaces
  the nexus process, no post-session summary). Default is now `spawn()+wait()` on all
  platforms, which enables the post-session summary and correct exit code propagation.

- **`nexus run --skip-checks` flag** — skip the pre-launch check suite for faster
  startup when the environment is known-good.

- **`nexus run --show-env` confirmation prompt** — `--show-env` now displays the
  resolved env block and waits for `Enter` before launching the tool (previously it
  launched immediately). `--dry-run` behaviour is unchanged (display only, no launch).

- **`nexus run` hint after `nexus init` and `nexus pull`** — both commands now print
  a contextual hint about `nexus run`:
  - With `devbox.json` present: optional tip (devbox shell already sets vars).
  - Without `devbox.json`: important notice that `nexus run` is required for plugin
    env var injection.

### Changed

- **`nexus run` default launch mode** — changed from `exec()` (Unix) to `spawn()+wait()`
  on all platforms. This enables the post-session summary and preserves the nexus
  process after the tool exits. Use `--exec` for the previous behaviour.

- **`preflight.rs` — check functions are now `pub(crate)`** — `CheckResult`, `print_check`,
  and `cmd_version` are reused by `nexus run` pre-launch checks.

## [0.10.2] - 2026-07-09

### Fixed

- **`McpServerConfig.command` — String vs. Array deserialization** — the backend
  delivers array commands (e.g. `["headroom", "mcp", "serve"]`) for plugin MCP
  servers. The previous `command: String` field caused a hard serde failure, making
  `nexus pull` silently fall back to the local template and skip `.nexus/env` entirely.
  `command` is now `Vec<String>` with a custom `StringOrVec` deserializer that
  accepts both forms. `opencode.json` writes the full array; `mcp.json` (Claude format)
  splits into `command` (first element) + `args` (remainder).

- **`McpServerConfig.environment` field missing** — inline env vars delivered by the
  platform (e.g. `HEADROOM_*` from `nexus-headroom` MCP config) were silently dropped.
  New `environment: HashMap<String, String>` field is now merged into the
  `opencode.json` and `mcp.json` environment blocks. Inline values take precedence
  over `env_keys` templates.

- **3 environment-dependent test failures** — `test_write_mcp_configs_npm_mode`,
  `test_write_mcp_configs_if_missing_creates_both`, and
  `test_write_mcp_configs_reads_key_from_env_file` failed when `NEXUS_SEC_OPENAI_API_KEY`
  was set in the shell (e.g. via devbox / `.env.nexus.local`). Each test now calls
  `std::env::remove_var` before the assertion to ensure deterministic results
  regardless of the shell environment.

### Tests

- **T1–T4** (`nexus-core/src/api/types.rs`): `McpServerConfig` string command,
  array command, `environment` map, and full `af_export` round-trip with
  `nexus-headroom` (array command + environment + `plugin_env`).

- **T5–T9** (`nexusctl/src/cmd/pull.rs`): `write_mcp_configs` plugin-server paths —
  array command in `opencode.json`, inline environment overlay, `env_keys` template
  rendering, inline-overrides-env_keys precedence, and `mcp.json` Claude format
  (`command[0]` → string, `command[1..]` + `args` → array).

## [0.10.1] - 2026-07-09

### Fixed

- **`nexus config set run.default_tool`** — `Config::set()` now recognises the
  `run.default_tool` key. Previously the key was silently absent from the setter,
  making it impossible to configure the default tool via `nexus config set`. The
  "valid keys" error hint is updated accordingly.

## [0.10.0] - 2026-07-09

### Added

- **`.nexus/env` — platform-managed plugin env vars** — `nexus pull` and `nexus init`
  now write `.nexus/env` from `af_export.plugin_env`. The file is the single source of
  truth for non-sensitive, platform-managed environment variables (e.g. `HEADROOM_*`).
  - Full overwrite on every pull/init (platform-owned file).
  - Automatically git-ignored via `.<agentic_root>/.gitignore` (entry `env` appended if missing).
  - Deleted gracefully when `plugin_env` is absent or empty in the API response.

- **`nexus run` — env-var injection before tool launch** — new command that launches a
  tool (default: `opencode`) with platform-managed plugin env vars injected into the
  process environment.
  - Env resolution order (low→high): `.nexus/env` → `.env.nexus.local` → shell (shell vars
    are never overwritten).
  - `--dry-run`: print resolved env block and exit without launching.
  - `--show-env`: print env block, then exec.
  - `--no-db`: offline mode — reads `.nexus/env` from disk only (no API call).
  - `--tool <name>`: override the default tool; configurable via `[run] default_tool` in
    `~/.config/nexus/config.toml`.
  - `-- <args...>`: extra args forwarded verbatim to the tool.
  - Unix: replaces the current process via `exec()` (same PID, signals work correctly).
  - Windows: spawns + waits; propagates exit code.

## [0.9.5] - 2026-06-30

### Security

- **SEC-001: quinn-proto upgraded to 0.11.15** — fixes RUSTSEC-2026-0185
  (remote memory exhaustion via unbounded out-of-order QUIC stream reassembly,
  CVSS 7.5 high). Transitive via `reqwest`.

- **SEC-002: anyhow upgraded to 1.0.103** — addresses RUSTSEC-2026-0190
  (unsoundness in `Error::downcast_mut()`). No direct call to `downcast_mut()`
  in nexus-cli source; upgrade is precautionary.

- **SEC-003: URL allowlist for plugin and avatar downloads** — `nexus pull` and
  `nexus init` now validate all remote download URLs (plugin registries, actor
  avatar assets) against a trusted-host allowlist before fetching:
  `nexus.gatewarden.eu`, `cdn.gatewarden.eu`, `raw.githubusercontent.com`,
  `github.com`, `objects.githubusercontent.com`. Non-HTTPS URLs and
  non-allowlisted hosts are rejected with a warning. Prevents SSRF if the
  API response is ever compromised. (CWE-918)

- **SEC-004: nexus upgrade — supply-chain risk documented** — module-level doc
  comment and runtime output now explicitly note that `nexus upgrade` runs
  `curl | bash` without checksum verification. Alternative GitHub Releases
  download URL shown on failure. (CWE-494)

`cargo audit` passes with 0 vulnerabilities and 0 warnings after these updates.

## [0.9.4] - 2026-06-29

### Fixed

- **`nexus init`: protected file refusal no longer exits with code 1** — when
  `nexus init` encounters an existing protected file (e.g. `.env.nexus.local`)
  during agent file delivery, it now prints a yellow warning and continues
  instead of calling `anyhow::bail!()`. The workspace is fully initialized;
  only the env file is intentionally skipped. Exit code is now 0 in all cases
  where the workspace itself was set up correctly.

  Path traversal attempts (`..` in `target_path`) remain a hard error — that
  indicates a malformed or malicious server response. (Dispatch 038595ee)

## [0.9.2] - 2026-06-28

### Added

- **Unit tests for `parse_frontmatter()`** — 8 tests covering: valid frontmatter
  extraction, missing frontmatter passthrough, empty values skipped, comments
  ignored, colons in values, unclosed frontmatter, route_alias parsing, leading
  whitespace handling.

- **Unit tests for `normalize()`** — 3 tests covering: creating frontmatter from
  plain markdown (infers slug from filename), preserving existing frontmatter
  fields during normalization, error on nonexistent file.

- **CLI parser tests for new actors subcommands** — 7 tests covering:
  `actors normalize`, `actors validate`, `actors validate --project-id`,
  `actors import`, `actors export` (default target), `actors export --target`,
  `pull --skip-actor-assets`.

Total test count: 149 → 167 (+18 new tests).

## [0.9.1] - 2026-06-28

### Added

- **`nexus actors normalize <path>`** — normalize actor markdown files to
  canonical YAML frontmatter format (ADR-0056). Extracts or infers slug, name,
  role, description, and route_alias fields, then rewrites the file with
  consistent frontmatter structure.

- **`nexus actors validate <path>`** — validate actor profile(s) against the
  expected schema. Checks required frontmatter fields (slug, name, role), slug
  format, and route_alias references against the model route catalog (ADR-0055).
  Reports errors and warnings.

- **`nexus actors import <path>`** — import validated actor profiles from local
  markdown files (or a directory of .md files) into the Actor Registry via
  `POST /api/mcp/actors` with `actor_import` action.

- **`nexus actors export --target opencode`** — export actor configuration for
  opencode.json format via `POST /api/mcp/actors` with `actor_export` action.
  Outputs the JSON agent block to stdout.

- **`opencode_agents` merge in `nexus pull`** — when the af_export response
  includes an `opencode_agents` field, it is merged into `opencode.json` as the
  `"agents"` top-level key alongside `"mcp"` and `"provider"`.

- **Model route deprecation warnings** — `nexus pull` now checks if any
  assigned actor references a deprecated model route (ADR-0055) and prints a
  warning with the deprecation message.

- **`nexus pull --skip-actor-assets`** — explicit flag to skip avatar asset
  downloads (complement to `--with-actor-assets`). `--skip-actor-assets` takes
  precedence when both are specified.

- **`ModelRoute` type** — `nexus-core` exports `ModelRoute` with alias,
  provider, model, deprecated flag, and deprecation message fields.

- **Actor import/export API methods** — `import_actors()` and
  `export_actors()` added to NexusClient.

## [0.9.0] - 2026-06-27

### Added

- **Actor profile delivery in `nexus pull`** — when the backend returns actor
  data in the `af_export` response, the CLI now writes:
  - `.nexus/actors/<slug>.md` for each assigned actor (Markdown profile)
  - `.nexus/generated/actors.json` with full actor metadata (JSON export)
  This aligns with ADR-0053/0054 (Nexus Actor System).

- **`nexus pull --with-actor-assets`** — new flag to download cached actor
  avatar SVGs from the platform into `.nexus/actors/assets/<slug>.svg`.
  Default: metadata only (no binary assets). Avatar URLs come from the
  `actors[].avatar.url` field in the af_export response.

- **`nexus actors` command group** — new top-level command with subcommands:
  - `nexus actors list` — list actors assigned to the linked project
  - `nexus actors show <slug>` — show full actor profile (role, model
    routing, permissions, profile body)
  - `nexus actors avatar generate <slug>` — trigger avatar regeneration
    via API
  - `nexus actors avatar reset <slug>` — reset avatar to DiceBear default

- **Actor API types** — `nexus-core` now exports `ActorSummary`,
  `ActorProfile`, `ActorAvatar`, `ActorListResponse`, `ActorGetResponse`,
  `ActorAvatarResponse`, and `ExportedActorFile` types.

- **NexusClient actor methods** — `list_actors()`, `get_actor()`,
  `generate_actor_avatar()`, `reset_actor_avatar()`, and
  `download_actor_avatar()` added to the HTTP client.

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
