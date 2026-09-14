# ADeploy

ADeploy is a lightweight Rust tool for deploying applications across platforms through a gRPC-driven client/server workflow. Use it to push versioned artifacts and run pre/post hooks with predictable TOML configuration.

## Highlights
- Cross-platform deployment (Linux, macOS, Windows)
- Language-agnostic packaging with tar/flate2
- Ed25519 signing with a server-side key allowlist
- One `adeploy.toml` per project, committed alongside the code it deploys
- Optional pre/post deployment scripts and backups

## Quick Start
```bash
adeploy init                       # write a commented adeploy.toml here
adeploy server                     # start the gRPC server
adeploy <host> <pkg> [pkg...]      # deploy one or more packages
adeploy --help                     # list available subcommands and flags
```
Build with `cargo build` first if you do not already have the binary.

`adeploy client <host> <pkg>` is the explicit spelling of the third line; both
forms are equivalent.

### Running as a Service
```bash
adeploy server install                 # install the server as a system service
adeploy server install --user          # install a per-user service (systemd --user / launchd)
adeploy server start                   # start the installed service immediately
adeploy server status                  # inspect the current service state
adeploy server stop                    # stop the running service
adeploy server uninstall               # remove the service definition
```
Pass `--label <name>` to customise the service identifier (defaults to `adeploy`). Add `--no-autostart` to skip starting on boot or `--disable-restart-on-failure` to prevent automatic restarts when the service exits with an error.

## Configuration

Both halves of a deployment live in a single `adeploy.toml`. Run `adeploy init`
to write a fully commented starting point, then edit the `[packages.*]` table.

The client looks for `adeploy.toml` by walking up from the working directory,
the way `cargo` finds `Cargo.toml`. Commit the file with your project and
`adeploy <host> <package>` works from anywhere inside the checkout. The server
reads the copy sitting beside its own binary. Either can be pointed elsewhere
with `--config <path>`.

**Relative paths in `sources` resolve against the directory holding
`adeploy.toml`, never against the working directory.** That is what makes a
committed configuration behave identically on every machine.

```toml
[defaults]
port = 6060            # the server listens here, the client dials it
connect_timeout = 5    # seconds to establish the connection
deploy_timeout = 600   # seconds for upload plus everything the server does
max_file_size = 104857600

[packages.demo]
sources = ["./dist/demo"]   # client: what to archive
deploy_path = "demo"        # server: where to unpack, under deploy_root
backup_enabled = true

# Per-host overrides; list only what differs from [defaults].
[remotes."192.0.2.10"]
deploy_timeout = 1800
```

`connect_timeout` and `deploy_timeout` are deliberately separate: reaching an
unresponsive host should fail in seconds, while an upload followed by an
installer and a service restart may legitimately take minutes.

Server-only settings live under `[server]` in the copy beside the server
binary — `allowed_keys` (the base64 Ed25519 keys permitted to deploy, which the
client prints when it is rejected) and `deploy_root` (the base directory that
relative `deploy_path` values land under). The server reloads this file when it
changes, so adding a key does not require a restart.
