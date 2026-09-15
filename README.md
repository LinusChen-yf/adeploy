# ADeploy

ADeploy is a lightweight Rust tool for deploying applications across platforms through a gRPC-driven client/server workflow. Use it to push versioned artifacts and run pre/post hooks with predictable TOML configuration.

## Highlights
- Cross-platform deployment (Linux, macOS, Windows)
- Language-agnostic packaging with tar/flate2
- Ed25519 signing with a server-side key allowlist, checked before any upload
- Chunked uploads with live progress and server logs streamed back as they happen
- One `adeploy.toml` per project, committed alongside the code it deploys
- Optional pre/post deployment scripts, snapshots, and rollback to any of them

## Quick Start

On the target machine, the binary is the only thing you have to put there:

```bash
adeploy server install      # generates adeploy.toml and the deploy root
adeploy server start
```

In the project you want to deploy:

```bash
adeploy init                       # write a commented adeploy.toml here
adeploy list                       # what this project declares
adeploy pair <host>                # ask the server to trust this machine
adeploy <host> <pkg> --dry-run     # what would be sent, without sending it
adeploy <host> <pkg> [pkg...]      # deploy one or more packages
adeploy rollback <host> <pkg>      # put the previous deployment back
adeploy --help                     # list available subcommands and flags
```

`adeploy list` reads the configuration and nothing else — the package and remote
names every other command wants, and a mark against any source that is not
there. `--dry-run` really builds the archive and shows what it holds, so it also
answers whether packaging works at all:

```
Would deploy demo to 192.0.2.10:6070
      app.bin                       878.9 KiB
      app.conf                      7 B
  2 file(s), 878.9 KiB packed into 879.3 KiB, sha256 24a33d5e...
Nothing was sent; drop --dry-run to deploy
```

Pairing queues a request; an operator on the server approves it:

```bash
adeploy server pending             # who is waiting, and from where
adeploy server approve 1           # by position, or by fingerprint
```

Both ends print the same key fingerprint. Compare them before approving — that
comparison is what makes the approval mean anything, rather than trusting
whoever reached the queue first. `adeploy server keys` lists who is trusted and
`adeploy server revoke` withdraws it. The running server picks all of this up
without a restart.

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

## Pairing

`Pair` is the one method that cannot require a key, since establishing one is
the point. A request is self-signed, which proves the sender holds the key it is
presenting — enough to stop anyone queueing keys they do not control — and then
waits for a human. Nothing is trusted until someone approves it.

Approvals live in `paired.toml` beside the server binary, written by the tool.
They are kept out of `adeploy.toml` so the server never rewrites a file an
operator hand-edited, losing their comments and layout. `allowed_keys` still
works and is simply unioned with what has been approved.

The queue is bounded and deduplicated by key, so a client polling while it waits
cannot fill it, and filling it at all needs that many distinct keys — which an
operator looking at a full queue can see.

## Rolling back

A deployment with `backup_enabled` snapshots the directory before replacing it.
Those snapshots are what rollback restores:

```bash
adeploy rollback <host> <pkg> --list          # what is available
adeploy rollback <host> <pkg>                 # the most recent snapshot
adeploy rollback <host> <pkg> --to backup_20260914_100512
```

Rolling back runs the same before and after hooks a deployment does, because
putting files back has the same requirement: the service holding them has to
stop first and start after. It also snapshots the current state before
replacing it, so a rollback can itself be undone.

Snapshots live under `<deploy_root>/.backups/<package>` unless `backup_path`
says otherwise.

## Replacing a deployment

The new tree is assembled under a sibling directory and moved into place with a
rename. Unpacking straight over the deploy path meant a failure part way through
left a directory that was neither the old deployment nor the new one, and a
running service could read half-replaced files for as long as extraction took.
A failed deployment now leaves the live one exactly as it was.

`clean_deploy` decides what the new tree starts from. Off by default, the
existing directory is copied in first, so files the package does not ship —
uploads, logs, a database — survive; the archive then overwrites what it does
ship. Turned on, the new tree contains only what the archive holds, so files
dropped from a package stop lingering on the server. That is usually what you
want for a directory that is entirely build output.

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
transfer_timeout = 60  # seconds the upload may go without moving data
deploy_timeout = 60    # seconds for the server's work, from the last byte

[packages.demo]
sources = ["./dist/demo"]   # client: what to archive
deploy_path = "demo"        # server: where to unpack, under deploy_root
clean_deploy = false        # server: replace the directory rather than merge
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

The three phases are bounded separately, because they fail for unrelated
reasons and scale with unrelated things:

| | bounds | sized for |
|---|---|---|
| `connect_timeout` | establishing the connection | reaching a host that is up |
| `transfer_timeout` | the upload going **quiet** | a link that is alive |
| `deploy_timeout` | the server's work, from the last byte | what your hooks do |

`transfer_timeout` is a gap, not a budget. How long a transfer legitimately
takes depends on the size of the package and the speed of the link, so a total
would have to be revisited every time either changed — the same trap a single
combined timeout sets. A link that has gone quiet for a minute has gone quiet
regardless of both.

`deploy_timeout` starts when the last byte arrives, so it never has to leave
room for the upload. The server is told the value and stops at it too, rather
than working on after the client has given up. It cuts both ways: too low a
value aborts a deployment that was going to succeed, part way through, so raise
it for a package whose hooks run an installer or restart a service.

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
- `allowed_keys` — the base64 Ed25519 keys permitted to deploy, which the
  client prints when it is rejected.
- `deploy_root` — the base directory that relative `deploy_path` values land
  under, defaulting to a `deploy` directory beside the binary.

The server reloads this file when it changes, so adding a key does not require
a restart.
