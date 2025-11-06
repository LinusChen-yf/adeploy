# ADeploy

ADeploy is a lightweight Rust tool for deploying applications across platforms through a gRPC-driven client/server workflow. Use it to push versioned artifacts, run pre/post hooks, and manage rollbacks with predictable TOML configuration.

## Highlights
- Cross-platform deployment (Linux, macOS, Windows)
- Language-agnostic packaging with tar/flate2
- Secure SSH key authentication and configurable timeouts
- Optional pre/post deployment scripts and backups

## Quick Start
```bash
./adeploy server                          # start the gRPC server with server_config.toml
./adeploy client <host> <pkg1> [pkgN...]  # deploy one or more packages from client_config.toml
./adeploy client 192.168.50.11 myapp myapp2
./adeploy --help                 # list available subcommands and flags
```
Build with `cargo build` first if you do not already have the binary.

### Running as a Service
```bash
./adeploy server install                 # install the server as a system service
./adeploy server install --user          # install a per-user service (systemd --user / launchd)
./adeploy server start                   # start the installed service immediately
./adeploy server status                  # inspect the current service state
./adeploy server stop                    # stop the running service
./adeploy server uninstall               # remove the service definition
```
Pass `--label <name>` to customise the service identifier (defaults to `adeploy`). Add `--no-autostart` to skip starting on boot or `--disable-restart-on-failure` to prevent automatic restarts when the service exits with an error.

## Configuration Basics
Sample templates live in `config_example/`. Copy the appropriate template into the same directory as the `adeploy` binary and name it `client_config.toml` (for client runs) or `server_config.toml` (for server runs). The executable automatically loads the config file from its own directory.

### Client ([`client_config.toml`](config_example/client_config.toml))
The template covers package sources, per-host overrides, and a fallback block. Each field is documented inline so you can mirror the structure while tweaking values for your environment.

### Server ([`server_config.toml`](config_example/server_config.toml))
This template walks through listener settings, allowed deploy keys, hook scripts, and backup controls. Refer to the embedded comments for the exact behavior of every knob.

Use `./adeploy server` with the sample server config, then run `./adeploy client 192.168.50.11 demo` (or list multiple packages) to push the demo package defined in the client template.
