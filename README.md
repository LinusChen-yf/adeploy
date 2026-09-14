# ADeploy

ADeploy is a lightweight Rust tool for deploying applications across platforms through a gRPC-driven client/server workflow. Use it to push versioned artifacts and run pre/post hooks with predictable TOML configuration.

## Highlights
- Cross-platform deployment (Linux, macOS, Windows)
- Language-agnostic packaging with tar/flate2
- Ed25519 signing with a server-side key allowlist, checked before any upload
- Chunked uploads with live progress and server logs streamed back as they happen
- One `adeploy.toml` per project, committed alongside the code it deploys
- Optional pre/post deployment scripts and backups

## Quick Start

On the target machine, the binary is the only thing you have to put there:

```bash
adeploy server install      # generates adeploy.toml and the deploy root
adeploy server start
```

In the project you want to deploy:

```bash
adeploy init                       # write a commented adeploy.toml here
adeploy <host> <pkg> [pkg...]      # deploy one or more packages
adeploy --help                     # list available subcommands and flags
```

The first deployment is rejected until the client's key is authorized; the
client prints the key to paste into the server's `allowed_keys`, and the server
picks it up without a restart.

A deployment reports itself as it goes, rather than after it finishes:

```
Server accepted demo (6001142 bytes), deploy ID a708af0a-...
Uploaded 34% (2097152/6001142 bytes)
...
Uploaded 100% (6001142/6001142 bytes)
Running Before-deploy script /opt/adeploy/stop.sh
stopping service                     <- the hook's own output, line by line
installing dependencies
Before-deploy script succeeded
Extracting files into /opt/adeploy/deploy/demo
Deployment succeeded for demo
```

Build with `cargo build` first if you do not already have the binary.
`adeploy client <host> <pkg>` is the explicit spelling of the deploy line; both
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

## How a deployment travels

The client opens with a small signed message describing what it is about to
send — package name, size, SHA256, its public key, a nonce and a timestamp —
and only streams the archive once the server has accepted it. The signature
covers that whole description, not just the archive bytes, so an intercepted
request cannot be pointed at a different package, and the nonce and timestamp
stop it being replayed at all.

Two things follow from checking the key before the payload. An unauthorized
caller never gets to send an archive, and the server writes what it does
receive straight to a staging file rather than holding it in memory, so its
memory does not grow with the size of the package. The staged file is removed
once the deployment ends, however it ends.

The server also counts the bytes it actually receives: the declared size is a
claim from the client, so a stream that runs past it is cut off and one that
stops short is refused.

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
port = 6060            # port to dial; must match the server's listen_port
connect_timeout = 5    # seconds to establish the connection
deploy_timeout = 600   # seconds for upload plus everything the server does

[packages.demo]
sources = ["./dist/demo"]   # client: what to archive
deploy_path = "demo"        # server: where to unpack, under deploy_root
backup_enabled = true

# Per-host overrides; list only what differs from [defaults].
[remotes."192.0.2.10"]
deploy_timeout = 1800
```

`[defaults]` and `[remotes.*]` describe how *this client* reaches a server, and
the server reads neither. That separation is load-bearing: a project's
configuration travels to the server along with its packages, so nothing a client
sends may decide what the server enforces. Server policy lives in `[server]`,
and the parser rejects it anywhere else.

`connect_timeout` and `deploy_timeout` are deliberately separate: reaching an
unresponsive host should fail in seconds, while an upload followed by an
installer and a service restart may legitimately take minutes.

`deploy_timeout` travels with the request as its gRPC deadline, so the server
stops at the same moment the client does. Without it the server carried on
unpacking and running hooks after the client had already reported a timeout,
writing files that nobody was waiting for.

### On the server

The server generates its own `adeploy.toml` on first run (and during
`adeploy server install`), creates its deploy root, and reports both at startup
along with whether any client key is authorized yet. It never overwrites a file
that already exists.

It reads only the copy beside its own binary, never one found by searching
upward — starting the server from inside a project checkout must not make it
adopt that project's configuration. Use `--config <path>` to point it elsewhere.

Server-only settings live under `[server]`:

- `listen_port` — the port to bind. Clients dial it through their own `port`;
  the two are separate fields because they are separate decisions that merely
  share a default. Changing it requires a restart.
- `max_file_size` — the largest archive this server accepts. It is the only
  limit: the client does not pre-check, so the server's answer is the only one.
  It also bounds what an unauthenticated caller can make the server buffer,
  because a request is decoded before the handler that checks `allowed_keys`
  runs.
- `allowed_keys` — the base64 Ed25519 keys permitted to deploy, which the
  client prints when it is rejected.
- `deploy_root` — the base directory that relative `deploy_path` values land
  under, defaulting to a `deploy` directory beside the binary.

The server reloads this file when it changes, so adding a key does not require
a restart.
