# Repository Guidelines

## Project Structure & Module Organization
The deployment runtime lives in `src/`, with entrypoints in `main.rs` routing to modules such as `auth.rs`, `client.rs`, and `server.rs`. gRPC contracts sit in `proto/`; `build.rs` compiles them at build time via `tonic-build`. Integration tests are under `tests/` and share helpers in `tests/common`; place new fixtures next to the scenarios that consume them. Use `logs/` for local trace output but never commit generated artifacts or cargo `target/` contents.

## Build, Test, and Development Commands
- `cargo build` — compile the CLI and server binaries with protobuf generation.
- `cargo run -- server` — start the gRPC deployment server using `server_config.toml`.
- `cargo run -- client <host> <package>` — push a deployment defined in `client_config.toml`.
- `cargo fmt` / `cargo fmt --check` — apply or verify repository rustfmt settings.
- `cargo clippy -- -D warnings` — lint with all warnings elevated to errors.
- `cargo test` — execute unit and integration suites locally.

## Coding Style & Naming Conventions
Formatting is enforced by the committed `rustfmt.toml`: two-space indentation, crate-grouped imports, and `snake_case` functions or modules. Prefer `UpperCamelCase` for types, `SCREAMING_SNAKE_CASE` for constants, and descriptive error variants under `thiserror`. Document public APIs with `///` comments and keep gRPC service identifiers aligned with `proto/*.proto` names.

## Testing Guidelines
Use `cargo test` for the full suite; integration flows in `tests/integration_tests.rs` and `tests/ssh_tests.rs` should mirror real deployment sessions. Add async scenarios with `tokio::test` when exercising networking paths. Cover both success and failure branches for new features, wiring shared scaffolding through `tests/common`. Name new test files `<feature>_tests.rs` to keep discovery predictable.

## Commit & Pull Request Guidelines
Follow the Conventional Commit-style prefixes present in history (`fix:`, `refactor:`, `misc:`) and keep subjects under 65 characters. Each pull request should include a purpose summary, the commands run for verification, linked issues, and screenshots or log excerpts for user-visible changes. Request a maintainer review and rerun `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` before pushing updates.

## Configuration & Security Notes
Client and server settings are loaded from `client_config.toml` and `server_config.toml` located beside the binaries; never embed credentials in code. Key material belongs outside version control—store only mock keys or fingerprints needed for tests. When protobuf schemas change, rebuild with `cargo build` to regenerate bindings and ensure ports, timeouts, and deploy paths remain configurable through TOML rather than hardcoded constants.
