# nexus-cli

Rust CLI for the mpowr-nexus platform. Provides project scaffolding, authentication, and configuration management.

## Workspace Structure

```
nexus-cli/
├── Cargo.toml          # Workspace root
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

## Commands

```
nexus init [path]       Initialize a Nexus project workspace
nexus login             Authenticate with the Nexus platform
nexus logout            Remove stored credentials
nexus status            Show auth, project, and workspace status
nexus link [--project-id <id>]  Bind a project to the current workspace
nexus unlink            Remove project binding from the workspace
nexus deinit [--force]  Remove all AI scaffold files from the workspace
nexus config show       Display configuration
nexus config set K=V    Update a configuration value
nexus config path       Show the config file path
```

## Build

```
cargo build --release
```

## Test

```
cargo test --workspace
```
