# Repository Guidelines

## Project Structure & Module Organization
The deployment runtime lives in `src/`, with entrypoints in `main.rs` routing to modules such as `auth.rs`, `client.rs`, and `server.rs`. gRPC contracts sit in `proto/`; `build.rs` compiles them at build time via `tonic-prost-build`, and attaches any lint attributes the generated types need. Integration tests are under `tests/` and share helpers in `tests/common`; place new fixtures next to the scenarios that consume them. Use `logs/` for local trace output but never commit generated artifacts or cargo `target/` contents.

## Build, Test, and Development Commands
- `cargo build` — compile the CLI and server binaries with protobuf generation.
- `cargo run -- server` — start the gRPC deployment server using the `adeploy.toml` beside the binary.
- `cargo run -- client <host> <package>` — push a deployment defined in the project's `adeploy.toml`.
- `cargo run -- init` — write a commented `adeploy.toml` into the working directory.
- `cargo +nightly fmt` / `cargo +nightly fmt --check` — apply or verify repository rustfmt settings.
- `cargo clippy --all-targets -- -D warnings` — lint everything, warnings as errors, the way CI does.
- `cargo test` — execute unit and integration suites locally.

## Coding Style & Naming Conventions
Formatting is enforced by the committed `rustfmt.toml`: two-space indentation, crate-grouped imports, and `snake_case` functions or modules. Prefer `UpperCamelCase` for types, `SCREAMING_SNAKE_CASE` for constants, and descriptive error variants under `thiserror`. Document public APIs with `///` comments and keep gRPC service identifiers aligned with `proto/*.proto` names.

## Testing Guidelines
Use `cargo test` for the full suite. `tests/integration_tests.rs` drives the normal client against a real server and should mirror real deployment sessions; `tests/protocol_tests.rs` is the opposite and builds messages by hand, which is where anything a captured, tampered or lying client could do belongs. Add async scenarios with `tokio::test` when exercising networking paths. Cover both success and failure branches for new features, wiring shared scaffolding through `tests/common`. Name new test files `<feature>_tests.rs` to keep discovery predictable.

Several behaviours here are only observable across repeated runs — port allocation, and anything the test harness sleeps around. Run the suite a few times before trusting a change that touches them.

## Commit & Pull Request Guidelines
Follow the Conventional Commit-style prefixes present in history (`fix:`, `refactor:`, `misc:`) and keep subjects under 65 characters. Each pull request should include a purpose summary, the commands run for verification, linked issues, and screenshots or log excerpts for user-visible changes. Request a maintainer review and rerun `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` before pushing updates.

## Configuration & Security Notes
Both ends read a file named `adeploy.toml`, but they are different files. A project's copy describes its packages end to end and is committed with the code; the client finds it by walking up from the working directory. The server's copy sits beside its binary, is generated on first run, and holds only `listen_port` and `allowed_keys` — it carries nothing about any package, because the description travels with the deployment and is covered by its signature. Both accept `--config <path>`. Relative `sources` resolve against the directory holding the config, never the working directory; never embed credentials in code. Key material belongs outside version control—store only mock keys or fingerprints needed for tests. When protobuf schemas change, rebuild with `cargo build` to regenerate bindings and ensure ports, timeouts, and deploy paths remain configurable through TOML rather than hardcoded constants.
