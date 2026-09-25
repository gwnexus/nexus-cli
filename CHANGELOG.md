# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.28.1] - 2026-09-25

Staging acceptance findings for v0.28.0 (NEXUS-APP dispatch b5f7bfb0).

### Fixed
- `AGENTS.md` and `CLAUDE.md` agent files are classified as **projection** (the backend regenerates them on every export and `af_sync` push rejects them): a local edit is DRIFTED with a `nexus reset` hint, `nexus push` refuses them, and `nexus stash` no longer stashes them.
- `<agentic_root>/directives.md` is now visible to `nexus status` / `nexus diff` / `nexus reset` as a projection generated from the project directives. `nexus pull` records its hash, so a local edit (DRIFTED) is distinguishable from a new backend version (UPDATE).
- `nexus diff` for content now uses the backend version as the base (`--- backend` / `+++ local`), so local additions show as `+`. Projection diffs are unchanged (local base, `+` = what the next pull writes).
- Drifted `.claude/rules/*` files no longer suggest `nexus env set claude.profile` (several rules ship with every profile); the hint is `nexus reset`. The push refusal only mentions the dashboard for skills.

Confirmed (no change needed): the UPDATE check ignores `generated_at`, so a timestamp-only re-export is not reported.

3 new tests, 3 updated to the new classification. 663/663 tests passing, clippy/fmt clean.

## [0.28.0] - 2026-09-25

One command per function group (NEXUS-APP dispatch b5f7bfb0, revised specification).

### Added
- **Shared workspace classification** (`workspace_state`): every managed file is *content* (assigned agent files, `devbox.json`, `scripts/devbox/**`) or *projection* (CCX files, generated agent files such as `ccx-*`, `actor-profile-*`, `actors-json`, `env-nexus-local`, `rtk-filters*` and the headroom plugin, skills and OpenCode commands rendered by pull, the `CLAUDE.md` managed block, managed `.claude/settings.json` keys), with the same rules `nexus pull` applies. Unmanaged files are never listed; the non-selected runtime's projection is reported as STALE and never deleted.
- **`nexus status`** now also shows the agent environment (what `nexus run` starts), the Claude Code runtime (version vs. `ccx.compatibility.claudeCode`, plugins, last headroom summary), git identity (as `nexus git verify`), and every non-clean file with class, state and next action (`nexus push`, `nexus reset <path>`, `nexus env set ...`, `nexus pull`). `--output json`; exit 0 clean, 1 pending.
- **`nexus diff [path]`**: unified diffs for content (local vs backend) and projection (local vs what the next pull writes), key-level for managed settings and the `CLAUDE.md` block. Read-only; exit 1 when differences exist.
- **`nexus push [path]`**: content only. Modified agent files go through the agent-file sync; devbox changes become a workspace fork as before (`--name`, `--dry-run`, `--adopt-local` unchanged). Projection files are refused with the setting to change instead (e.g. `.claude/statusline/*` -> `nexus env set claude.hud`), so generated file keys never reach `af_sync` push.
- **`nexus reset [path]`**: content returns to the backend version, projection files to what the next pull writes (CCX lock, pull manifest, `CLAUDE.md` block hash and single settings keys updated accordingly). Without a path, all pending changes after one confirmation (`-y` skips it).
- **`nexus env get [key]` / `nexus env keys` / `nexus env set <key> <value> [--dry-run] [--pull]`** against `GET`/`PATCH /api/mcp/projects/{id}/settings`. Keys, types and allowed values come from the backend `schema`; bool values accept `true`/`false` (also `on`/`off`), nullable keys `unset`/`null`. `set` sends `expected_revision`, prints `changes` (empty = no-op), prints 400 `details` per field, reports 403 verbatim, and on 409 re-reads, shows what changed and retries once on confirmation. `claude.*` keys on an OpenCode project print a note. `--pull` runs `nexus pull` afterwards. Backends without the endpoint get a clear hint instead of a bare 404.
- **`nexus stash`** now also stashes modified agent content files (projection files never).

### Changed
- `nexus sync status|push|reset` and `nexus claude status|diff` are hidden, deprecated aliases for one release: they print the new command and delegate (`sync push/reset <file_key>` map to `nexus push/reset <path>`).
- Agent-file hashes are compared without the `generated_at:` line the backend re-stamps on every export (raw hashes recorded by older CLIs still match), so a local edit is reported as MODIFIED instead of CONFLICT, and pull records a hash that stays valid for the file on disk.
- When the backend sends several agent files for one `target_path`, pull (and status) use the first one and print a warning naming the ignored file keys, instead of letting them overwrite each other on every pull.

### Removed
- The old `nexus sync status` / `sync reset` implementations (replaced by `nexus status` / `nexus reset`).

28 new tests (classification per class incl. the timestamp case, projection detection, next actions, path matching, push refusal, reset per kind incl. CCX lock and single settings key, stash of agent files, duplicate agent files, env value parsing and schema handling, CLI parsing incl. deprecated aliases, and the settings endpoint against a local HTTP stub: GET, PATCH 200 with the exact request body, 400 details, 403, 409 revision). 661/661 tests passing, clippy/fmt clean.

## [0.27.0] - 2026-09-25

v0.26.0 acceptance findings (NEXUS-APP dispatch 4820e584) and a single start command (dispatch 442f0e97).

### Changed
- **`nexus run` follows the backend's `run_target`** (new `af_export.run_target: { tool, workspace, layout? }`, cached in `.nexus/config.toml` by `nexus pull` so `--no-db` resolves the same start). `workspace: "zellij"` starts zellij with the CCX layout through the full `nexus run` path (pre-launch checks, env injection, gh profile, `--account`, session summary); if zellij or the layout is missing, Claude Code starts directly with a one-line hint. Otherwise `run_target.tool` starts. `--tool` stays an explicit override; backends without `run_target` keep the previous `run.default_tool` / `agent_owner` resolution.
- **One runtime per project.** `agent_owner` is `opencode` or `claude-cli`; absent, unknown and the retired `both` value count as OpenCode (`config::is_claude_owner`). `nexus pull` / `nexus init` render exactly one projection: no `.opencode/commands`, `.opencode/plugins`, `opencode.json` or `.opencode/**` agent files for claude-cli projects, and no Claude Code projection (`.mcp.json`, `.claude/**`, `CLAUDE.md`) for OpenCode projects. The stale-projection warning stays (nothing is deleted).
- **`nexus claude launch` is deprecated**: hidden alias that prints "use nexus run" and delegates to it. README documents `nexus run` as the only start command.
- **Plain `nexus pull` no longer prompts for unchanged files.** Skills, skill resources and OpenCode commands are rendered and compared with disk: identical files are skipped silently, files unmodified since the last pull (hash in `<agentic_root>/generated/pull-manifest.json`) are updated without a prompt, and only locally edited files are listed and confirmed with one prompt. Declining keeps just those files; the rest of the pull (including CCX reconciliation) always continues. `nexus init` uses the same renderer and hash record.
- Agent files, `directives.md`, `devbox.json`, workspace scripts, `opencode.json`, `.mcp.json`, `model-routes.json`, `<agentic_root>/env` and `.claude/skills/` are no longer rewritten or reported when their content is unchanged (agent-file comparison ignores the `generated_at:` line the backend re-stamps on every export). Files Nexus wrote that cannot carry the `source: nexus-platform` marker (YAML/TOML/JSON) are recognised as managed via the sync manifest instead of being reported as user-managed.
- **`nexus upgrade` verifies the whole chain.** Releases now publish `install.sh` and `install.sh.sha256`; `nexus upgrade` fetches both from the latest GitHub release, verifies the SHA-256 in-process, and aborts on a missing or mismatching checksum. Only releases without the asset fall back to `nexus.gatewarden.eu` (verified when a checksum is published there).

### Fixed
- The "Existing .claude/ files detected ... nexus import" hint no longer lists Nexus-generated files (marker, nexus-managed block, sync manifest, or a claude-cli project's `.claude/settings.json`).
- No more `!! .env.nexus.local is a protected file, refusing to overwrite` on every pull: existing protected scaffolds are skipped silently (write-if-missing).
- The final tip names what `nexus run` starts (OpenCode, Claude Code, or the Claude Code workspace) instead of always "OpenCode".
- Doubled dot in pull output paths (`..nexus/env`, `..nexus/generated/model-routes.json`) and in the `.nexus/env` header.

14 new tests (run_target resolution for all shapes incl. zellij/layout missing, `--tool` override, fallback and legacy `both`; generated-file classification, pull twice without prompt or writes, locally edited file prompts only for itself, force; import-hint filtering; run tip labels; agent-file timestamp comparison; install-script checksum; run_target cache); MCP/run tests updated to the one-runtime rule. 633/633 tests passing, clippy/fmt clean.

## [0.26.0] - 2026-09-24

Completes the CCX (Claude Code Experience) CLI scope of NEXUS-APP ADR-0117 (dispatch 99f335e8).

### Added
- **Per-file CCX reconciliation in `nexus pull`.** New `af_export.ccx: { bundle, version, revision, compatibility.claudeCode }` field. When present, `agent_files` with category `claude_experience` are no longer written unconditionally: each is classified against the CCX lock (`<agentic_root>/claude/manifest.lock.json`) as create/clean/updated/drifted/conflict/unmanaged/adopt/orphaned and handled per the dispatch's state table. Local edits and never-managed files are kept and reported. Orphaned files (still locked, no longer sent) are deleted only if unmodified. The lock records bundle metadata and the sha256 of the exact bytes written, and is rewritten once, only after every CCX file was reconciled. When `ccx` is absent, nothing CCX-specific happens and an existing lock is left untouched.
- **Migration rule:** on the first CCX-aware pull (no lock, or a v0.25.x lock without a revision), hashes from `.nexus/sync-manifest.json` stand in for lock hashes, so files written by earlier CLIs are recognised as managed.
- **`nexus pull --force` / `--force-unmanaged` for CCX files.** `--force` overwrites drifted/conflicting files, deletes modified orphans, and replaces a locally edited `CLAUDE.md` block; `--force-unmanaged` additionally replaces files never managed by Nexus. `-y/--yes` deliberately does not imply either, so accepting prompts never discards local edits.
- **`CLAUDE.md` managed block conflict handling.** The sha256 of the block text is recorded in the lock; if the block was edited locally and a different block arrives, it is kept and reported as CONFLICT unless `--force`.
- **CCX section in the pull output** (CREATE/UPDATE/CLEAN/DRIFTED/CONFLICT/UNMANAGED/ADOPT/ORPHANED per file, the `CLAUDE.md` block, a SETTINGS line per managed key, and a HOOKS line per removed hook adapter).
- **`nexus claude status`**: read-only overview (only network call: `af_export`) of desired vs. locked bundle revision, per-file state, managed settings keys, `CLAUDE.md` block, `claude --version` vs. `compatibility.claudeCode` (warns outside the range), enabled plugins vs. `claude plugin list --json` (skipped if unavailable), and the last headroom `session_summary`. Supports `--output json`. Exit 0 when clean, 1 when changes are pending or conflicts exist.
- **`nexus claude diff`**: unified diff (local vs. desired) for every non-clean CCX file, key-level diff for managed settings, and the `CLAUDE.md` block. No writes; same exit codes.
- **`nexus claude launch`**: runs `<agentic_root>/claude/nexus-claude.kdl` through the regular `nexus run --tool zellij -- --layout <file>` path (all pre-launch checks and env injection) when the layout exists and zellij is on PATH; otherwise falls back to `nexus run --tool claude` with a hint. Supports `--account`, `--force`, `--skip-checks`.

### Fixed
- `classify_file_state()` classified a locked file whose local content already equals the desired content as CONFLICT when the lock hash was stale (e.g. after a pull that wrote files but failed before saving the lock). It is now CLEAN, per the dispatch's table (`clean: L == D`).
- Settings reconciliation now also drops array entries of a **still-managed** key that Nexus previously added but no longer sends (e.g. one entry removed from `permissions.deny`), keeping operator-added entries.
- The CCX lock's `applied_at` is now an ISO 8601 UTC timestamp (was epoch seconds).

### Changed
- `merge_claude_generic_settings()` now delegates to a pure `apply_generic_settings()`, which `nexus claude status`/`diff` run on a copy, so the commands never disagree with what a pull would do.
- New workspace dependency: `similar` (unified diffs).

30 new unit tests (every state-table row through the pull wiring incl. pull-twice-all-clean, drifted then `--force` restore, orphan deletion/keep/drop, sync-manifest migration incl. a v0.25.x settings-only lock, path traversal rejection, lock untouched on apply error, `CLAUDE.md` block conflict/force/update, still-managed array entry removal, settings change descriptions, version range and plugin list parsing, CLI parsing). 619/619 tests passing, clippy/fmt clean.

## [0.25.1] - 2026-09-24

### Added
- **Hook adapter removal when Nexus stops managing one** (NEXUS-APP ADR-0117 follow-up, dispatch 99f335e8, the same "stopped sending" problem as the v0.25.0 settings-key removal), e.g. when a project enables the `nexus-core` Claude Code plugin which carries the same five hooks and `claude_hook_adapters` becomes empty as a result. `merge_claude_hooks()` now records each adapter's registrations (event/matcher/command) and hook-file SHA-256 in the CCX lock (`<agentic_root>/claude/manifest.lock.json`), and on a later pull removes only the exact `.claude/settings.json` hook entries Nexus itself wrote (matched on event, matcher, and command) plus the corresponding file under `.claude/hooks/` -- but only if that file's content on disk still matches what Nexus last wrote there. An operator's own hook entries and any locally-modified hook script are left untouched.

4 new unit tests (removal of an entry no longer present, preservation of an operator-customized hook file, preservation of an operator's own settings entry for the same event, and confirmation a still-managed adapter is never reported as removed). 589/589 tests passing, clippy/fmt clean.

## [0.25.0] - 2026-09-24

### Added
- **CCX lock foundation and settings-key removal** (NEXUS-APP ADR-0117 follow-up, dispatch 99f335e8), closing the gap explicitly flagged as missing in v0.24.0: `merge_claude_generic_settings()` now removes a `.claude/settings.json` key (or array entries) that Nexus used to manage but no longer sends, without ever touching a value the operator changed themselves. New `<agentic_root>/claude/manifest.lock.json` (CLI-owned, atomic write via temp file + rename) records the settings state from the last pull; on the next pull, any key present in the lock but absent from the new `claude_settings.managed_keys` is removed if (and only if) its current value still matches what the lock recorded, and for array values only the lock's own entries are removed, leaving operator additions untouched.
- New `nexusctl::cmd::ccx` module also includes `classify_file_state()`, the full per-file reconciliation state machine (create/clean/updated/drifted/conflict/unmanaged/adopt/orphaned) specified in dispatch 99f335e8, built and unit tested against every row of the state table now so the next phase is pure wiring rather than new design. Not yet consumed by `nexus pull`.

### Deferred
- Per-file CCX pull wiring (writing CCX-governed files with lock-tracked hashes, the migration-adoption rule, `--force`/`--force-unmanaged` semantics, orphan deletion), the CLAUDE.md-block conflict path, the CCX pull output section, and the three new `nexus claude status`/`diff`/`launch` commands remain open, tracked against the same follow-up dispatch. The lock format's `bundle`/`version`/`revision` fields are `Option`al for now (populated once that wiring lands) so the lock can already track settings-merge history on its own in the meantime.

15 new unit tests (lock round-trip incl. atomic-write and corrupt-file handling, all 8 state-table rows, settings-key removal across scalar/array/still-managed/no-previous cases, and 2 end-to-end tests through `merge_claude_generic_settings`/`render_claude_projection` simulating two pulls). 585/585 tests passing, clippy/fmt clean.

## [0.24.0] - 2026-09-24

### Added
- **Generic `.claude/settings.json` merge from `af_export.claude_settings`** (NEXUS-APP ADR-0117 "CCX", dispatch bb782869). New `claude_settings: { managed_keys, values }` af_export field; new `merge_claude_generic_settings()` sets/replaces each managed dot-path (e.g. `permissions.deny`, `statusLine`, `attribution`, `enabledPlugins`, `extraKnownMarketplaces`) on every init/pull. Array values under a managed path are unioned with any existing entries rather than replaced, so an operator's own `permissions.deny` rules are preserved alongside Nexus's. `attribution` (Claude Code 2.1.281+) is delivered through this generic mechanism; the existing `includeCoAuthoredBy` write (v0.21.2) is unchanged for older CLIs.
- **Root `CLAUDE.md` managed block.** New `claude_md_managed_block` af_export field (markdown); new `merge_claude_md_managed_block()` maintains it between `<!-- BEGIN:nexus-managed -->`/`<!-- END:nexus-managed -->` markers on every init/pull. Everything outside the markers is user-owned and never rewritten; if no markers exist yet, the block is inserted at the top once, preserving the rest of the file (including other tools' own managed blocks, e.g. a Next.js `nextjs-agent-rules` block) untouched below it.
- **Path traversal hardening for `write_agent_file`.** The existing guard only rejected `..` components; a `target_path` that was merely *absolute* (e.g. `/etc/passwd`) previously slipped past it, since `Path::join` on an absolute path replaces the base entirely rather than nesting under it. New `validate_agent_file_target_path()` rejects any absolute path component in addition to `..`, then confirms in depth that the joined, lexically-normalized result still starts with the workspace root. Used by both `nexus init` and `nexus pull`.

### Deferred
- The CCX lock/manifest (`.nexus/claude/manifest.lock.json`, per-file clean/updated/conflict/create/unmanaged state, `--force-unmanaged`) and the new `nexus claude status`/`nexus claude diff`/`nexus claude launch` commands from the same ADR-0117 dispatch are a substantially larger, separate subsystem and are not part of this release. `merge_claude_generic_settings()` does not yet remove a managed key that a project stops sending (needs the lock to track prior state) -- every call is purely additive/replacing for whatever `managed_keys` the current `af_export` lists. Tracked as nexus-cli follow-up work.

25 new unit tests (path hardening, generic settings merge across top-level/nested/array-union/idempotency/preservation cases, CLAUDE.md managed-block creation/replacement/coexistence). 572/572 tests passing, clippy/fmt clean.

## [0.23.0] - 2026-09-24

### Changed
- **Per-project `gh` CLI profile handling now implements the accepted ADR-0116 contract**, superseding the ADR-0115 behaviour shipped hours earlier in v0.22.0 (NEXUS-APP dispatch 0350aee7: that dispatch was resolved one minute before the ADR-0116 amendment was accepted, so v0.22.0 shipped the superseded design). The bug in v0.22.0: an empty profile directory caused `nexus run` to strip `GH_TOKEN`/`GITHUB_TOKEN` and launch with an unauthenticated `GH_CONFIG_DIR`, silently defeating `gh` for the whole session.
- **Input source switched** from `git_config.gh` to `project.gh_effective` (`{ host, user, profile, source, origin }`). A `null`/absent `gh_effective` leaves the environment completely untouched, exactly as before.
- **Empty profile is now seeded, not silently broken.** When the profile has no cached login, `nexus run` resolves a token per `source` (`keyring` via `gh auth token`, `env:<VAR>`, or `auto` trying keyring then `GH_TOKEN` then `GITHUB_TOKEN`), verifies it via `gh api user` against the expected user, prompts `Import GitHub login <user> from <source> into profile <profile>? [Y/n]` (auto-accepted by the global `-y`/`--yes` flag), and writes it into the isolated profile via `gh auth login --with-token` on stdin. The token is never logged, printed, or passed as a command argument at any point.
- **Hard safety rule: if seeding is impossible, the resolved token belongs to the wrong user, or the operator declines, the environment is left *completely* untouched** (no `GH_CONFIG_DIR`, no token stripped) and a warning with the exact one-time login command is printed instead. A session must never end up less authenticated than it would have been without Nexus.
- `nexus run` and `nexus pull` now print a login follow-up line (e.g. `GitHub: octocat@github.com via profile octocat (project)`) reflecting the resolved identity, or the next-step hint if none is authenticated yet. `nexus pull`'s version is informational only and never attempts seeding.
- `nexus git verify`'s `gh` check now reads `gh_effective` instead of `git_config.gh` (same verification logic, `gh api user --jq .login --hostname <host>`, unchanged output shape). `nexus git verify`/`nexus git apply` also now tolerate a project with `gh_effective` but no `git_config` at all (previously required `git_config` to be present for either subcommand to do anything).
- New `nexus_core::api::GhEffective` type. `git_config.gh`/`GhConfig` are kept for backward wire deserialization but are no longer read by any nexus-cli logic.

25 new/updated unit tests across the seeding-source priority logic, verify-output classification, host-override rules, and the login-hint/status-line formatting. 545/545 tests passing, clippy/fmt clean.

## [0.22.0] - 2026-09-24

### Added
- **Per-project `gh` CLI profile support** (NEXUS-APP dispatch 8776d208, targeting the nexus-app 0.14.0 release). Projects can now declare `git_config.gh = { host, user, profile }`; the GitHub token itself never leaves the operator's machine and never passes through the Nexus backend, only the local profile name is recorded.
  - `nexus run` (all tools, not just Claude Code): when `git_config.gh` is set, launches the child process with `GH_CONFIG_DIR=~/.config/nexus/gh-profiles/<profile>` (created with `0700` permissions if missing) and strips `GH_TOKEN`/`GITHUB_TOKEN`/`GH_ENTERPRISE_TOKEN`/`GITHUB_ENTERPRISE_TOKEN` from the child environment, printing a one-line notice naming which variables were removed. Sets `GH_HOST` for a non-`github.com` host. Never modifies the parent shell, never runs `gh auth switch`, never writes a token to disk. `--show-env`/`--dry-run` show `GH_CONFIG_DIR`/`GH_HOST` and the names (not values) of any removed token variables.
  - New "gh Auth" pre-launch check: warns (never blocks) if the profile has no cached login for its host yet, printing the exact one-time command to run (`GH_CONFIG_DIR=... gh auth login --hostname <host>`).
  - `nexus git verify` gained a `gh` check: resolves the active login via `gh api user --jq .login --hostname <host>` under the profile's isolated `GH_CONFIG_DIR` and reports ok / mismatch (active vs. expected) / not logged in / `gh` not installed. `nexus git apply` is unchanged for `gh` (login is interactive by design; nothing to apply).
  - New `nexus_core::api::GhConfig` type and `GitConfig.gh: Option<GhConfig>` field (additive, no wire schema change).

### Confirmed
- `includeCoAuthoredBy` (shipped in v0.21.2): a missing `git_config.include_co_authored_by` is, and continues to be, treated as `false` (suppressed), matching nexus-app's own migration backfill for existing projects.

## [0.21.4] - 2026-09-24

### Added
- **`.claude/settings.json` now pre-approves a baseline of 14 read-only, side-effect-free Nexus MCP tools** (`session_list`, `kb_memory`, `kb_search`, `kb_get`, `dispatch_sweep`, `dispatch_inbox`, `dispatch_get`, `task_list`, `sk_list`, `sk_get`, `pd_list`, `project_list`, plus `session_create`/`session_append` on the same "expected every session, low risk" basis), reducing first-session approval-prompt friction for the calls every session-bootstrap skill makes in its opening turns. New `merge_claude_baseline_permissions()` follows the same idempotent, non-destructive merge pattern as the existing hooks and `includeCoAuthoredBy` merges: only appends missing entries into `permissions.allow`, never removes or reorders an operator's own additions, only rewrites the file when something actually changed. Deliberately excludes anything that creates, mutates, or deletes platform state (`adr_decide`, `task_create`, `dispatch_resolve`, `sk_update`, `doc_ingest`, `doc_delete`, etc.), which stay gated behind explicit per-session approval.
- **`nexus pull` now warns when a stale toolstack projection is left on disk after an `agent_owner` flavor change.** `pull` is intentionally additive-only and never deletes an unselected projection, so switching a project's flavor (e.g. `both`/`opencode` to `claude-cli`) previously left the no-longer-selected projection's files (`.opencode/` or `.claude/`) silently orphaned. An explicit warning is now printed instead, without auto-deleting anything, consistent with this codebase's conservative stance on operator data.

Both are follow-ups to a live `claude-cli` diagnostic pass against Nexus Showcase Beta, in the same vein as the `description`/`command_slug` fix shipped in v0.21.3.

## [0.21.3] - 2026-09-23

### Fixed
- **Skill `SKILL.md` writers no longer discard the backend's `description` and `command_slug`, and no longer duplicate frontmatter** (NEXUS-APP dispatch 5ddd6355). All three local skill writers (`write_claude_skill` in the Claude Code renderer, and `write_skill` in both `nexus init` and `nexus pull`) rebuild their own frontmatter block around `ExportedSkill.body` using a hardcoded local template that only carried `skill_id`/`name`/`version`/`source` (plus `command_slug` in the OpenCode writers only). `description` was dropped everywhere, which meant Claude Code's `/` command palette fell back to a raw slug or a blank summary instead of the platform-configured one-line description.
- Investigating this surfaced a second, more serious defect confirmed live in this repo's own `.nexus/skills/` workspace state: `ExportedSkill.body` already carries its own frontmatter block from the backend, which the local template was duplicating verbatim underneath its own, producing two stacked frontmatter blocks in the same file. New `claude_render::strip_frontmatter()` removes the backend's block before the local template rebuilds its own; used by all three writers.
- `description` is now written as a quoted, escaped YAML string (new `claude_render::yaml_escape()`) since free-text descriptions may contain `:` or other characters that break an unquoted YAML scalar; absent means an empty string is written, not the field being omitted, so the frontmatter shape stays uniform across skills.

## [0.21.2] - 2026-09-23

### Fixed
- **Claude Code no longer silently adds a `Co-Authored-By: Claude ...` trailer to commits** (NEXUS-APP dispatch 84e38bd7, escalated to a hard blocking requirement). Previously this depended entirely on an agent remembering the project directive against AI self-references on every single commit, which kept being missed. `includeCoAuthoredBy` (Claude Code's own runtime setting for this, verified against the shipped `cli.js`) is now written into `.claude/settings.json` from the project's `git_config.include_co_authored_by`, uninverted in Claude Code's own semantics (`true` means the trailer is added). Absent means suppress: a project with no value configured (created before this shipped) gets `includeCoAuthoredBy: false` written, not Claude Code's own default, since the whole point is that operators must not have to remember to configure this per project.
- The merge runs on every `nexus init`/`nexus pull`, not just at first creation, and is safe against an already-existing, operator-customized `settings.json`: only the `includeCoAuthoredBy` key is touched, every other key (including the `hooks` block from Track B3) is preserved verbatim. New `nexus_core::api::GitConfig::include_co_authored_by` field (additive, `serde(default)`, no schema change on the wire since `git_config` already reaches the CLI through the existing project-detail endpoint).

## [0.21.1] - 2026-09-22

### Added
- **`nexus run --account default`: reserved alias for the implicit `~/.claude` identity** (NEXUS-APP dispatch c0523ebe, follow-up to `--account` in v0.21.0). Previously the only way to use the already-logged-in default Claude identity was to omit `--account` entirely; every literal name unconditionally created a new, unauthenticated directory. `--account default` now behaves identically to omitting the flag (no `CLAUDE_CONFIG_DIR` override, no directory created), so scripts and aliases can always pass an explicit `--account <name>` regardless of how many real named accounts currently exist. Never warned about on non-`claude-cli` projects (unlike a real name), and never treated as a creatable/reservable slot: a literal `default` directory under `claude-accounts/` can never be created via `--account`. The pre-launch check panel shows `default (~/.claude, explicit)` to confirm the explicit choice was honored, distinct from the implicit `default (~/.claude)` shown when `--account` is omitted.
- The `--account` resolution logic was split into a pure decision core (`resolve_account`) separate from directory creation, so the `default`-alias behavior, the non-claude-project warning, and the real-account-name path are each independently unit tested without filesystem or network I/O.

## [0.21.0] - 2026-09-21

### Added
- **`nexus run --account <name>`: named Claude Code account switching for multiple Claude Max subscriptions** (NEXUS-APP dispatch ad6e0176, queued behind and built after the 8de19c71/v0.20.2 billing-auth fixes). Claude Code derives its Keychain credential storage key from `CLAUDE_CONFIG_DIR`; pointing an invocation at a Nexus-managed directory under `~/.config/nexus/claude-accounts/<name>/` gives that name its own isolated login and OAuth refresh cycle, entirely client-side. Nexus never stores or is aware of which account is selected; this is a pure local runtime convenience, scoped strictly to `claude-cli`/`both` projects (`agent_owner` gate via `wants_claude()`), and has no effect (with a warning) elsewhere. Without `--account`, behavior is unchanged (default `~/.claude`). A new "Account" entry in the `nexus run` pre-launch check panel shows which account name (or the default) is active, so the operator is not guessing.
- **Explicit only, by design**: a single named account per invocation, never automatic. There is no quota/rate-limit detection, no rotation list, and no fallback-on-failure logic, matching the operator's own posture of manually switching between their own subscriptions rather than tooling silently deciding for them. An already-set `CLAUDE_CONFIG_DIR` in the shell is never overwritten.
- Account names are validated before use (`validate_account_name`): plain alphanumeric/`-`/`_` identifiers only, rejecting empty names, `.`/`..`, and path separators, so a name cannot escape `~/.config/nexus/claude-accounts/`.

## [0.20.2] - 2026-09-21

### Fixed
- **`nexus run --force` no longer skips pre-launch checks entirely** (regression follow-up to NEXUS-APP dispatch 8de19c71, found and reproduced live by NEXUS-APP within minutes of the v0.20.1 release). The `Command::Run` dispatch in `mod.rs` computed `skip_checks || force`, so passing `--force` alone (without `--skip-checks`) skipped the whole `run_prelaunch_checks` panel, including the just-shipped Billing Auth check that is deliberately not supposed to be bypassable by `--force`. `--force` is documented as "skip pre-launch confirmation prompt (non-interactive/CI mode)" and must not imply skipping the checks themselves. Extracted the call-site mapping into `should_skip_prelaunch_checks(skip_checks, force)` (now `skip_checks` alone) with 3 dedicated regression tests pinning the exact bug shape, since the previous fix's unit tests exercised `run_prelaunch_checks`'s internal logic in isolation and could not see this call-site wiring bug.

## [0.20.1] - 2026-09-21

### Fixed
- **`nexus run` now hard-fails when an inherited `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` would silently defeat a Claude Max subscription** (NEXUS-APP dispatch 8de19c71). Claude Code's auth precedence puts these credentials ahead of the Keychain OAuth subscription login: if either is present and non-empty, `claude` authenticates via metered API-key billing instead of the Max subscription, with no warning or error in the common case. Nexus workspaces commonly carry `ANTHROPIC_API_KEY` in `.env.nexus.local` for unrelated reasons (BYOK provider keys, other tooling), and `nexus run` already injects those vars into the process environment before spawning the tool, so this was the default shape of such a workspace, not a contrived edge case. This new "Billing Auth" pre-launch check is scoped strictly to `claude-cli`/`both` projects (`direct_provider`/`nexus_gateway` projects rely on `ANTHROPIC_API_KEY` and are unaffected), and unlike every other pre-launch check it is deliberately NOT bypassable via `--force`: this is a billing-correctness bug, not a UX rough edge.

## [0.20.0] - 2026-09-21

### Added
- **`nexus run` and `nexus preflight` now respect the project's `agent_owner`** instead of assuming OpenCode (NEXUS-APP dispatch dfd4e655). `nexus run --tool claude` already worked mechanically, but every surrounding check was OpenCode-specific, so a `claude-cli` project got actively misleading output.
- `[project].agent_owner` is cached in `.nexus/config.toml` by `nexus link`, `nexus init`, and `nexus pull`, so launch-time commands resolve the tool flavor without a network round-trip. `nexus run` additionally refreshes it from `af_export` when it is talking to the backend anyway (no extra request). New `nexus_core::config` helpers: `load_agent_owner`, `update_agent_owner`, `tool_for_agent_owner`.
- `nexus preflight` checks for the `claude` binary, gated on `agent_owner` being `claude-cli` or `both`. OpenCode-only projects are never told to install a binary they do not launch.

### Changed
- The `nexus run` pre-launch MCP config check inspects the artifact that actually applies to the flavor: root `.mcp.json` for `claude-cli`, `opencode.json` for `opencode`, both for `both`. Previously a `claude-cli` project was warned "No opencode.json - run 'nexus init'" about a file it is never supposed to have.
- The post-session Headroom section detects the Claude Code hook adapter under `.claude/hooks/` (shipped in 0.19.0, Track B3) in addition to the OpenCode plugin at `.opencode/plugins/nexus-headroom-intercept.ts`. The adapter file name is server-supplied, so detection matches on the plugin name rather than a fixed path.
- `run.default_tool` is now optional (`Option<String>`), so "not configured" is distinguishable from an explicit `"opencode"`. Resolution order for `nexus run`: `--tool` > explicit `run.default_tool` > `agent_owner` (`claude-cli` -> `claude`) > `opencode`. Existing config files with an explicit `default_tool` are unaffected; an unset value is no longer written back to `~/.config/nexus/config.toml`.
- `both` and unknown/absent flavors continue to resolve to `opencode`. Workspaces linked before `agent_owner` was cached behave exactly as before until the next `nexus link` / `nexus init` / `nexus pull`.

### Notes
- Out of scope per the dispatch: no job/queue/daemon semantics, no headless plumbing or result write-back (those belong to ADR-0107 delegation work), and no changes to `claude_render.rs` or `.mcp.json` generation.

## [0.19.0] - 2026-09-21

### Added
- **Distribution for Claude Code plugin hook adapters** (Track B3, NEXUS-APP dispatch 2d5017f7). Previously `nexus pull`/`nexus init` had no way to get the actual `session-guard`/`headroom-intercept`/`compaction-plus`/`routing-guard`/`cost-control` adapter scripts onto a Claude Code project's disk, or to wire them into `.claude/settings.json`'s `hooks` block -- verified live end-to-end only via a manual, machine-specific workaround. Mirrors the existing OpenCode plugin distribution pattern (`.opencode/plugins/*.ts`, embedded server-side and written out by the CLI):
  - New additive `af_export` field `claude_hook_adapters: Option<Vec<ClaudeHookAdapter>>` (`nexus_core::api`): each entry carries a `plugin_name`, `target_path` (e.g. `.claude/hooks/nexus-session-guard.mjs`), the bundled single-file `body`, and a list of `hook_events` (`event`, optional `matcher`, optional `timeout`). nexus-cli is a thin consumer -- nexus-app/nexus-oc-plugins own the bundled script content and versioning.
  - `nexus init`/`nexus pull` now write each adapter's `body` to its `target_path` under `.claude/hooks/` (platform-managed generated code, always re-synced on every pull, like `.opencode/plugins/*.ts`).
  - `.claude/settings.json`'s `hooks` block is merged idempotently, keyed by plugin: a new `{matcher, hooks: [...]}` entry is appended per `(event, adapter)` pair only if no existing entry's command already references that adapter's `target_path`; existing entries (including operator-hand-written ones) are never touched or removed. Unlike the `env` block (routing-guard follow-up, create-once-only), this merge runs on every `nexus pull`/`nexus init`, including against an already-existing `settings.json` -- confirmed multiple plugins can validly share the same event key as independent array entries (e.g. `headroom-intercept` and `cost-control` both on `Stop`).
  - Hook event names are converted from Claude Code's PascalCase to the adapters' own kebab-case `argv[2]` subcommand contract (`PostToolUse` -> `post-tool-use`), confirmed directly against the adapter source; the hook payload itself is always read from stdin by the script, never passed as an argument.
  - No new `nexus preflight` check added: the existing generic Node.js check already covers this (Node is already a hard requirement for the npm-sourced MCP server).
  - 9 new unit tests: kebab-case conversion, script writing/re-sync, hooks-block merge (basic, idempotency, multi-plugin-same-event, operator-customization preservation, no-op with zero adapters), and a full-render integration test. Full workspace suite: 302/302 green, no clippy warnings, cargo fmt clean.

## [0.18.1] - 2026-09-21

### Added
- **`nexus_cost_summary` (local MCP server) now implements real Helicone-backed cost/spend queries** (NEXUS-APP dispatch af407643 follow-up). Per the exact spec supplied (`core/cost-control/helicone.ts`): queries `POST https://api.helicone.ai/v1/request/query` filtered by `Helicone-Session-Id`, aggregates `prompt_tokens`/`completion_tokens`/`prompt_cache_read_tokens`/`prompt_cache_write_tokens`/`helicone_cost` per model, and renders a markdown table plus grand totals. Reads `HELICONE_API_KEY` and `HELICONE_SESSION_ID`/`NEXUS_SESSION_ID` from the process environment; degrades gracefully with an honest, non-error message (not a crash, not fabricated numbers) when either is absent, matching the existing plugin's documented graceful-degradation behavior for an optional prerequisite. 6 new unit tests (credential-injected, no env-var mutation to keep parallel test execution safe): both graceful-degradation paths and markdown aggregation across multiple models. Full workspace suite: 293/293 green, no clippy warnings, cargo fmt clean.

## [0.18.0] - 2026-09-20

### Added
- **`nexus mcp-local`: a local stdio MCP server exposing tools with no Claude Code custom-tool equivalent** (NEXUS-APP dispatch af407643). Claude Code has no custom-tool-registration hook, so MCP is the only path to a new agent-callable tool; nexus-mcp (the hosted server) correctly declined to own these since they need machine-local filesystem/session state a hosted server cannot have (nexus-mcp dispatch d22de820). `nexus mcp-local` is a hidden CLI subcommand speaking MCP's newline-delimited JSON-RPC 2.0 stdio transport, spawned automatically by Claude Code via a new `nexus-local-tools` entry generated in `.mcp.json` for Claude Code projects (alongside the existing `nexus`/`nexus-headroom` entries). Exposes:
  - `nexus_headroom_intercept_retrieve` - reads the original uncompressed content behind a Headroom-compressed tool result, by content hash, from the project-local `.nexus/headroom-cache/<project_id>/<hash>.json` cache (nexus-oc-plugins' `OriginalStore` format). Supports an optional `query` argument to filter to matching lines, mirroring the equivalent OpenCode plugin tool. Hash input is validated (hex-only, bounded length) against path traversal.
  - `nexus_cost_summary` - registered for tool discovery, but intentionally returns an honest "not yet available" message rather than fabricated numbers: the underlying cost/spend computation (Helicone-backed telemetry) lives in nexus-oc-plugins' `core/cost-control` (TypeScript) and nexus-cli has no local data source to read it from yet. Flagged back to NEXUS-APP as a follow-up (needs either a concrete local data file or an API endpoint).
  - New `nexusctl::cmd::mcp_local` module, wired into both `.mcp.json` writers (`nexus init` and `nexus pull`, added if missing, never overwritten if the operator has customized the file). Logging and the CLI's own update-check are both suppressed/redirected to stderr for this subcommand so nothing but the JSON-RPC protocol itself ever reaches stdout.
  - 11 new unit tests (protocol shape, hash validation incl. path-traversal rejection, cache-read and query-filter behavior, honest-unavailable cost summary). Full workspace suite: 290/290 green, no clippy warnings, cargo fmt clean.

## [0.17.2] - 2026-09-20

### Added
- **`nexus init`/`nexus pull` now generate routing-guard adapter inputs for Claude Code projects** (NEXUS-APP dispatch 7a2d2adb, ADR-C05 Track B2). OpenCode's `routing-guard` plugin detects provider/model routing divergence live via the OpenCode SDK (`client.config.providers()`, `client.app.agents()`); Claude Code has no equivalent SDK surface, so its adapter instead reads two JSON files from disk. For `terminal_runtime=claude_code` / `claude-cli`-flavored projects where the backend supplies `runtime_spec` (ADR-C04/F1, additive af_export field, commit db5d053):
  - `<agentic_root>/generated/routing-catalog.json` - sourced from `runtime_spec.model_routes`.
  - `<agentic_root>/generated/agent-routing.json` - sourced from `runtime_spec.actors` and `runtime_spec.primary_agents`.
  - `.claude/settings.json` (on first creation only) now includes an `env` block with `NEXUS_ROUTING_GUARD_CATALOG_PATH` / `NEXUS_ROUTING_GUARD_AGENTS_PATH` pointing at the two files above, so the plugin adapter finds them without extra user configuration.
  - Fully additive and backward compatible: absent when the backend does not yet supply `runtime_spec` (older API versions), and does not change any existing `opencode.json` / `.mcp.json` / `.claude/skills/` / `.claude/agents/` behavior.
  - New `nexus_core::api::AgentFileExportResponse::runtime_spec: Option<serde_json::Value>` field (additive, untyped since the schema is server-owned).
  - 8 new unit tests covering catalog/routing-table generation, settings.json env injection (present and omitted), and the no-runtime_spec fallback. Full workspace suite: 279/279 green, no clippy warnings, cargo fmt clean.

## [0.17.1] - 2026-09-20

### Fixed
- **`.claude/agents/` was not rendered at all for `agent_mode=actor_based` + `claude-cli`-flavored projects** (NEXUS-APP dispatch c7701485 follow-up, found during live staging verification of v0.17.0). Root cause: the Claude Code renderer's actor writer only read the dedicated `actors` field on the af_export response (`ExportedActorFile`), but some backend project configurations deliver actor profiles exclusively through the generic `agent_files` list instead, with `target_path` already pointing at `<agentic_root>/actors/<slug>.md` (the same mechanism used for `AGENTS.md`/`directives.md`). That field can be empty even when actors are assigned and `<agentic_root>/actors/*.md` renders correctly via `agent_files`, since the two are actually different af_export fields. `write_claude_agents()` now merges both sources: the dedicated `actors` field and any `agent_files` entry whose `target_path` lives directly under an `.../actors/` directory (`.md` files only, so `AGENTS.md`/`CLAUDE.md` are never mistaken for actor profiles), deduplicated by slug. Verified against the reported repro (6 actors, `agent_mode=actor_based`, `agent_owner=claude-cli`): `.mcp.json`, `.claude/skills/`, and the OpenCode-side `.nexus/actors/*.md` were already confirmed correct in v0.17.0 and are unaffected by this fix. 4 new unit tests (regression test reproduces the exact reported shape: `agent_files`-only delivery, `actors` field empty). Full workspace suite: 271/271 green, no clippy warnings.

## [0.17.0] - 2026-09-20

### Added
- **Native Claude Code terminal renderer** (Track B1, NEXUS-APP Dispatch c7701485, ADR-C04/ADR-C06). `nexus init` and `nexus pull` now additionally generate a first-class Claude Code project projection alongside the existing OpenCode output, so a project works natively with `claude` in the terminal, not just via `nexus run`:
  - Root `CLAUDE.md` (thin wrapper, created only if absent, never overwritten): bootstraps Claude Code into `<agentic_root>/AGENTS.md`, `<agentic_root>/directives.md`, and `.mcp.json`.
  - `.claude/settings.json` (created only if absent): Claude runtime behavior only, contains no secrets and no OpenCode-specific statements.
  - `.claude/skills/<canonical-id>/SKILL.md`: one native Claude Code project skill per canonical Nexus skill, with resource files carried over. Skill directory names become Claude Code's `/<canonical-id>` slash-commands.
  - `.claude/agents/<slug>.md`: one native Claude Code sub-agent file per assigned project actor, reusing the same profile body already delivered for `<agentic_root>/actors/<slug>.md` (one canonical actor definition, two projections).
  - `.claude/hooks/` is intentionally **not** generated in this pass: hook *behavior* (Headroom, Compaction Plus, Session Guard, Routing Guard, Cost Control adapters) is separate follow-up work (Track B2), and Claude Code does not require the directory to exist for a hookless project.
  - New skill-naming migration: legacy `nx-*` canonical skill IDs are rendered under the `nexus-*` Claude Code command namespace (e.g. `nx-sec-scan` -> `.claude/skills/nexus-sec-scan/`), per ADR-C06 "Canonical command identity". `nexus-*` IDs pass through unchanged.
  - Additive and opt-in by flavor: entirely skipped when a project's tool flavor is `opencode`-only; the existing OpenCode projection (`opencode.json`, `.opencode/commands/`) is completely unaffected in every flavor. `.mcp.json` at the project root (already fixed in v0.16.9) is the shared MCP config consumed by this renderer.
  - New module `nexusctl::cmd::claude_render` with 12 new unit tests, including a skill-set parity check (no ID collisions from the `nx-*` -> `nexus-*` migration) per ADR-C04's acceptance criteria. Full workspace suite: 267/267 green, no clippy warnings.

## [0.16.9] - 2026-09-20

### Fixed
- **Claude Code project-scoped MCP config now renders to `.mcp.json` at the project root, not `<agentic_root>/mcp.json` (e.g. `.claude/mcp.json` or `.nexus/mcp.json`)** (NEXUS-APP dispatch b8e001e3, blocker for Track B1 "Claude Code renderer"). Per Claude Code's documented MCP installation scopes (code.claude.com/docs/en/mcp), a project-scoped, team-shared, git-committed MCP config must live at `.mcp.json` in the project root; the CLI previously wrote it under the configurable `agentic_root` instead, and a test in `init.rs` actively asserted that a root-level `.mcp.json` must NOT exist, treating the correct format as a "legacy" one. `nexus init`, `nexus pull`, and `nexus run`'s credential auto-sync now consistently read/write `.mcp.json` at the workspace root for the Claude Code projection; `nexus preflight`'s MCP config detection was updated to match. The OpenCode projection (`opencode.json`) and the `agentic_root`-relative convention for all other artifacts (skills, commands, directives, `CLAUDE.md`) are unaffected. `nexus deinit` already removed root `.mcp.json` correctly and required no change. Full workspace test suite (255/255) green, no clippy warnings.


### Added
- **Project-scoped commands now show which API backend they are targeting and the project's human-readable name, not just its opaque UUID.** `nexus pull`, `nexus push`, `nexus sync status`, `nexus project status`, and `nexus init` now print a consistent `API: <url>` / `Project: <Name> (<uuid>)` banner up front. The name is resolved from the locally linked `.nexus/config.toml` when available (no extra network call), falling back to a live `GET /api/mcp/projects/{id}` lookup otherwise; the UUID always remains visible in parentheses for exact identification, and the raw UUID alone is still shown if the name cannot be resolved (e.g. offline). Addresses feedback that `nexus pull` (and siblings) only ever showed a bare project UUID with no indication of which API URL was in effect, making it hard to spot a workspace silently pointed at the wrong environment. New shared helper module `nexusctl::cmd::display` with 2 new unit tests; full workspace suite: 255/255 green.

## [0.16.7] - 2026-09-20

### Fixed
- **`nexus login`/`nexus logout` no longer leak PAT scope across projects** (NEXUS-APP dispatch 51f7e592: "nexus login: PAT scope leaks globally, overrides other projects' prod tokens"). Previously `Credentials` had a single flat global store at `~/.config/nexus/credentials.toml`, so logging in to any project silently overwrote the token used by every other authenticated project on the machine. `nexus login`/`nexus logout` now default to a project-local `.nexus/credentials.toml` in the current workspace (already git-excluded via `nexus shadow`), scoped to that project only, mirroring the local/global provenance model already used by `nexus config set --local/--global`. Pass `--global` to opt in to the shared, machine-wide credential store explicitly. Token resolution order is now: `NEXUS_PRIVATE_TOKEN` env var, then project-local `.nexus/credentials.toml`, then global `~/.config/nexus/credentials.toml`. `nexus status` now reports which layer (`env`/`local`/`global`) supplied the active token. Added regression tests asserting that logging in/out of one project's local scope never mutates another project's local credentials.

## [0.16.6] - 2026-09-19

### Added
- **`nexus run` now auto-syncs project-scoped Nexus MCP credentials with the current global login** (Task a3bf595b, NEXUS-APP — "single place to maintain the token"). `opencode.json` and `.claude/mcp.json` deliberately bake a literal `NEXUS_API_URL`/`NEXUS_PRIVATE_TOKEN` (not an `{env:}` reference) so the MCP config keeps working even when the tool is launched without `nexus run` — that's an intentional, tested design, not a bug. The actual gap was that this baked copy silently drifted from the global login token whenever `nexus login` rotated it, with nothing short of a manual `nexus init`/`nexus pull` to fix it. `nexus run` already resolves the current, live-verified token on every invocation, so it now also rewrites the baked copy in place whenever it differs — only the two credential fields, no other formatting/keys touched, silent no-op when already in sync. In practice this means the user only ever manages one thing (`nexus login`), and every subsequent `nexus run` self-heals both `opencode.json` and `.claude/mcp.json`. Verified end-to-end against a deliberately-staled `opencode.json`: printed `refreshed Nexus credentials in: opencode.json`, rewrote the token, and the (also-new, see 0.16.5) live Headroom preflight check then passed. 4 new unit tests + full workspace suite (250/250) green.

## [0.16.5] - 2026-09-19

### Fixed
- **`nexus run`'s "Headroom" pre-launch check was env-var-presence-only and could report PASS while the `nexus-headroom-intercept` OpenCode plugin was silently running in `observe` mode** (root-caused via NEXUS-APP Task `a3bf595b`: a stale/rotated global CLI token caused the plugin's own preflight call to fail, silently downgrading `transform` -> `observe` for weeks while this check kept showing `PASS  Headroom  HEADROOM_MODE=transform`). The check now live-verifies via the same `GET /api/mcp/projects/{id}/preflight` endpoint the plugin itself calls (new `NexusClient::mcp_preflight()` / `McpPreflightResponse`), using the already-resolved auth token and linked project id. It now distinguishes: no project linked, not authenticated, live preflight unreachable/unauthorized (`FAIL`, with a pointer to `nexus status` / `nexus login`), and headroom disabled server-side for the project (`FAIL`) from an actually-verified `transform` mode (`PASS ... (preflight verified)`).

## [0.16.4] - 2026-09-11

### Fixed
- **`nexus run` post-session summary could appear to hang, unblocked only by a keypress** (dispatch 479a4ab5) - two independent, compounding issues in the post-session stats collection:
  - `git_head_sha`/`git_tags` (used to detect commit/tag activity during the session) are synchronous calls (`std::process::Command::output()` blocks the current OS thread until the subprocess exits). They were called directly inside the `async` block raced against `tokio::signal::ctrl_c()` via `tokio::select!`. Since a synchronous call runs to completion before the block's first real `.await` point, it silently defeated the "Press Ctrl+C to skip" promise shown to the operator for as long as the git call took -- Ctrl+C could not be observed until the blocking call returned, no matter how long that was. Both calls are now dispatched via `tokio::task::spawn_blocking`, so the race is genuine and Ctrl+C actually works regardless of how long the underlying git call takes.
  - All `git` subprocess invocations used for post-session stats (`git_head_sha`, `git_tags`, `git_count_commits`, `git_diff_stat`) and `nexus shadow`'s file-tracked-in-history check now pass `--no-pager`, so a `core.pager`/`GIT_PAGER` override that forces pagination can never make one of these calls wait on a keypress read from the controlling terminal.

### Changed
- Test suite: git-invoking test helpers (in `run.rs` and `shadow.rs`) now explicitly set `commit.gpgsign=false` on their temp repos. This removes a latent, pre-existing source of intermittent test flakiness on machines with `commit.gpgsign=true` set globally (common on dev machines), where concurrent test-suite `git commit` calls could contend for `gpg-agent` under parallel execution. No behavior change outside the test suite.

## [0.16.3] - 2026-09-10

### Fixed
- **Raw Rust stack backtrace printed alongside auth/API errors** - commands like `nexus pull` printed a `Stack backtrace:` block (with unhelpful, stripped-symbol frames such as `__mh_execute_header`) after errors like an invalid or missing API token, whenever `RUST_BACKTRACE` was set in the shell (common in dev shells / devbox). Root cause: `main()` returned the error up through Rust's default `Termination` handling, which Debug-formats `anyhow::Error` and includes any captured backtrace. `main()` now handles the dispatch error explicitly and prints only the clean, human-readable message (plus any cause chain) via `Display`, then exits with status 1 -- matching the clean error style `nexus status` already used. No behavior change for successful commands.

## [0.16.2] - 2026-09-08

### Added
- **Pull-time gate on `af_export` model-routing warnings** - `af_export` can now return an optional `export_warnings` array (e.g. an agent's model uses a provider Nexus can't verify, or a route-alias migration hasn't been applied on this backend). `nexus pull` renders these warnings verbatim, grouped by code with their hints, before writing `opencode.json` (and `<agentic_root>/mcp.json`), so a broken model/provider configuration is caught at pull time instead of surfacing mid-session as an opaque OpenCode error. Warnings are rendered as-is from the backend; the CLI does not re-derive the analysis client-side, since only the backend knows execution_mode, agent_mode, gateway availability, and the provider blocks it actually emitted.
  - Interactive sessions get a single-keypress `Continue anyway? [y/N]` prompt (default `N` -- declining skips the `opencode.json` / `mcp.json` write only; the rest of the pull, e.g. skills and directives, still completes).
  - `--yes` (or `--force`) bypasses the prompt and proceeds.
  - Non-interactive sessions (no TTY on stdin, e.g. CI) never block: warnings are printed and the pull proceeds automatically.
  - Unknown/future warning codes render generically rather than erroring, so this stays forward-compatible with new codes the backend may add.

## [0.16.1] - 2026-09-08

### Fixed
- **`opencode.json` MCP env missing `NEXUS_PROJECT_ID` -- agent could silently bind to the wrong project** - the generated `mcp.nexus.environment` block only ever contained `NEXUS_API_URL` and `NEXUS_PRIVATE_TOKEN`; with no project_id signal in the environment, an agent that lost track of which project it was working in had no reliable way to recover it short of guessing via a project-listing tool and name/slug matching. `nexus pull` and `nexus init` now also write `NEXUS_PROJECT_ID` (sourced from the `af_export` response / the resolved project id, which the CLI already has in hand) into that same environment block, so the correct project binding reaches the MCP server process deterministically.

### Added
- **Merge `opencode_instructions` into `opencode.json`'s `instructions[]` array** - `POST /api/mcp/agent-files` (`af_export`) can now return an `opencode_instructions` field (e.g. `["<agentic_root>/AGENTS.md"]`). `nexus pull` merges these paths into the top-level `instructions` array the same way `opencode_agents` is already merged into `agent`, so OpenCode loads the project's agent policy deterministically at session start instead of relying on its own upward `AGENTS.md` auto-discovery, which has no awareness of the `agentic_root` convention (default `.nexus/`) and was observed to non-deterministically miss the file across otherwise-identical runs. The merge is additive: pre-existing user entries in `instructions[]` are preserved, and re-running `pull` does not duplicate entries already present.

## [0.16.0] - 2026-09-06

### Added
- **`nexus config` git-style `--local` / `--global` precedence** - `nexus config` now supports two layers, mirroring git's own config model:
  - **Global** - `~/.config/nexus/config.toml` (machine-wide default, unchanged behavior).
  - **Local** - the project-local `.nexus/config.toml` gains a `[config]` section supporting `api_url`, `default_output`, and `no_color`, scoped to the current project only.
  - `nexus config set K=V --local` writes to the project-local file; `nexus config set K=V --global` (or no flag, unchanged default) writes to the global file.
  - Read-side precedence for every command: `--api-url`/`--output` CLI flag > `NEXUS_API_URL` env var > project-local config > global config > compiled-in default. Any command run inside a directory with a `.nexus/config.toml` automatically prefers the local value, falling back to global then default for keys the local file doesn't set.
  - `nexus config show` now prints per-key provenance (`local` / `global` / `default`) alongside both the global and (if present) local config file paths, so it's obvious at a glance which layer is in effect.
  - `nexus config path` gained `--local` / `--global` flags for symmetry; the unflagged default is unchanged (prints the global path).
  - This closes a real gap where juggling multiple projects across environments (e.g. a staging-only alpha-test project alongside production-bound projects) required either a one-off `--api-url` flag on every command, or a global `nexus config set api_url=...` that silently repointed every other project on the machine.

### Fixed
- **`nexus status` reported "Project OK" without any server-side validation** - the Project check previously just echoed the `.nexus/config.toml` binding back, so a workspace pointed at the wrong backend (e.g. `api_url` resolved to production while the linked project only exists on staging) got a clean bill of health with no way to detect the mismatch short of a later `pull`/`push` failure. `nexus status` now makes a real API call (`GET /api/mcp/projects/:id`) to confirm the project exists and is reachable at the effective `api_url`:
  - `OK <name> (<slug>)` only when the server confirms the project.
  - `ERR Not found at <api_url>` when the project does not exist there, with a hint to check `NEXUS_API_URL` / `--api-url` / `nexus config show` or re-link, distinct from an auth failure (different fix: rotate token vs. fix api_url/re-link).
  - `ERR Access denied at <api_url>` on a permissions failure, distinct from "not found".
  - `-- ... unverified` when authentication already failed or no token is configured, since the project binding cannot be confirmed without a valid session (avoids a redundant network call and misleading claims).
  - The `API URL:` line now also reports which layer supplied it (`flag`, `env`, `local`, `global`, `default`), composing directly with the `--local`/`--global` config precedence above so a mismatch is diagnosable from `nexus status` alone.

## [0.15.1] - 2026-09-03

### Fixed
- **`nexus push` misleading "run nexus pull first" error** - push now verifies the target project exists on the resolved backend before checking for a local baseline. A project that is absent from the targeted API URL (for example, targeting staging when the project lives on prod) now produces a precise "project not found on <api_url>, check NEXUS_API_URL / wrong backend" error instead of the misleading pull-first message. Auth and access failures are reported distinctly.
- **Precise workspace-scope diagnostics** - when the sync manifest is missing, empty, or tracks only the agent-files scope (workspace never pulled), the error now names the exact cause and points to `nexus pull --scope workspace` or `nexus push --adopt-local`, instead of the generic pull-first message.

### Added
- **`nexus push --adopt-local`** - publishes the current local workspace (devbox.json, scripts/devbox/**) as a new fork without a prior pull, bypassing the sync-manifest origin guard. This bootstraps a first fork from an untracked-but-existing local workspace without risking a clobber from pulling an older server-side fork. Honors `--dry-run` and `--name`.
- **Backend visibility in `nexus push`** - push output now prints the effective backend API URL alongside the project id, making wrong-backend situations diagnosable in one step (including under `--dry-run`).

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
