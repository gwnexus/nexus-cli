# Contributing to Nexus CLI

Thank you for your interest in contributing to the Nexus CLI.

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/<your-username>/nexus-cli.git`
3. Create a branch: `git checkout -b feature/my-feature`
4. Install the pre-commit hook: `cp hooks/pre-commit .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit`

## Requirements

- Rust >= 1.85 (install via [rustup](https://rustup.rs))
- `cargo fmt` and `cargo clippy` must pass without warnings

## Development Workflow

```bash
# Build
cargo build

# Run tests
cargo test --workspace

# Format code
cargo fmt --all

# Lint
cargo clippy --workspace -- -D warnings
```

## Pull Request Guidelines

- Keep PRs focused on a single change
- Include tests for new functionality
- Ensure `cargo test --workspace`, `cargo fmt --check`, and `cargo clippy` pass
- Write clear commit messages following [Conventional Commits](https://www.conventionalcommits.org/)

## Reporting Bugs

Open an issue on GitHub with:
- Your OS and architecture
- Nexus CLI version (`nexus --version`)
- Steps to reproduce
- Expected vs. actual behavior

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md).
