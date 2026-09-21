# ADeploy

ADeploy pushes a build to a machine and installs it there, over gRPC. A project
describes its own deployment in one committed file; a server needs nothing but
the binary and a key to trust.

## Highlights
- Cross-platform (Linux, macOS, Windows), language-agnostic packaging
- One `adeploy.toml` per project, committed alongside the code it deploys
- **A server holds no configuration for anything deployed to it** — the
  description travels with the package, signed
- Ed25519 signing checked *before* any upload, with pairing instead of copying
  keys by hand
- **Encrypted, with both ends identified, and no certificate to obtain** — the
  server generates its own on first run and the client records it while pairing
- Chunked uploads with live progress, and the server's own output streamed back
  as it happens
- Hooks that run scripts the package ships, so nothing has to be put on the
  server first
- Snapshots and rollback, and a swap that leaves the live deployment untouched
  if anything fails

## Quick Start

On the target machine, the binary is the only thing you have to put there:

```bash
adeploy server install      # registers the service and writes its adeploy.toml
adeploy server start
```

In the project you want to deploy:

```bash
adeploy init                       # write a commented adeploy.toml here
adeploy list                       # what this project declares
adeploy pair <host>                # exchange identities, then wait to be approved
adeploy <host> <pkg> --dry-run     # what would be sent, without sending it
adeploy <host> <pkg> [pkg...]      # deploy one or more packages
adeploy rollback <host> <pkg>      # put the previous deployment back
adeploy --help                     # list available subcommands and flags
```

Build with `cargo build` first if you do not already have the binary.
`adeploy client <host> <pkg>` is the explicit spelling of the deploy line; both
forms are equivalent.

### Before sending anything

`adeploy list` reads the configuration and contacts nothing — the package and
remote names every other command wants, and a mark against any source that is
not there. A name that matches no package is an error naming it, so a typo
cannot quietly deploy everything except the package you meant. `--dry-run` really builds the archive and shows what it holds, so it
also answers whether packaging works at all:

```
Would deploy demo to 192.0.2.10:6060
      scripts/stop.sh                 34 B
      scripts/start.sh                33 B
      app.bin                         2.9 MiB
      app.conf                        3 B
  4 file(s), 2.9 MiB packed into 2.9 MiB, sha256 ce833fd0...
Nothing was sent; drop --dry-run to deploy
```

### A deployment as it happens

```
Uploaded 34% (1048576/3000938 bytes)
Server accepted demo (3000938 bytes), deploy ID cdabe0fa-...
Uploaded 69% ... 100%
Verifying archive hash
Unpacking the new deployment
Running Before-deploy [1/2]: echo preparing
preparing
Running Before-deploy [2/2]: scripts/stop.sh
stopping service                        <- the script's own output, line by line
Before-deploy succeeded
Creating backup snapshot
Swapping in /opt/demo
Running After-deploy: scripts/start.sh
service started
Deployment succeeded for demo
```

### Running as a Service
```bash
adeploy server run                     # run in the foreground, for a first look
adeploy server install                 # install the server as a system service
adeploy server install --user          # install a per-user service (systemd --user / launchd)
adeploy server start                   # start the installed service immediately
adeploy server status                  # inspect the current service state
adeploy server stop                    # stop the running service
adeploy server uninstall               # remove the service definition
adeploy server clients                 # review who may deploy
```

`adeploy server` on its own prints this list rather than starting anything:
running a server is `adeploy server run`, so a mistyped subcommand cannot leave
a daemon behind.
Pass `--label <name>` to customise the service identifier (defaults to `adeploy`). Add `--no-autostart` to skip starting on boot or `--disable-restart-on-failure` to prevent automatic restarts when the service exits with an error.

## Configuration

Run `adeploy init` to write a fully commented starting point, then edit the
`[packages.*]` table.

The client finds `adeploy.toml` by walking up from the working directory, the
way `cargo` finds `Cargo.toml`, so committing it with your project makes
`adeploy <host> <package>` work from anywhere inside the checkout. **Relative
paths in `sources` resolve against the directory holding the file**, never
against the working directory — that is what makes one committed configuration
behave identically on every machine. `--config <path>` overrides the search.

```toml
[defaults]
port = 6060            # port to dial; must match the server's listen_port
connect_timeout = 5    # seconds to establish the connection
deploy_timeout = 60    # seconds for the server's work, from the last byte
tls = true             # verify the server and encrypt the connection

[packages.demo]
sources = ["./dist/demo"]        # includes dist/demo/scripts/
deploy_path = "/opt/demo"        # absolute directory on the server
backup_enabled = true
before_deploy = ["sc stop demo", "scripts/prepare.cmd"]
after_deploy = "scripts/start.cmd"

# Per-host overrides; list only what differs from [defaults].
[remotes."192.0.2.10"]
deploy_timeout = 1800
```

A package describes its deployment end to end, and all of it travels with the
package. Adding one, moving one, or changing a command is a change to the
project, committed with the code it deploys — there is nothing to keep agreed on
the other machine.

### Sources

A directory source contributes its *contents*, so `sources = ["./dist/demo"]`
puts whatever is inside `dist/demo` at the top of the deployment, and
`dist/demo/scripts/stop.sh` arrives as `scripts/stop.sh`. Files land at the top
by name. Executable bits survive the round trip. There is no glob expansion.

### Hooks

`before_deploy` and `after_deploy` take one command or a list, run in order,
with the first failure stopping the rest. Before-deploy aborts the deployment if
it fails; after-deploy only warns, since the files are already in place by then.

Before-deploy runs with the **unpacked package** as its working directory, and
after-deploy from the live deployment. That is what makes a script the package
ships usable: `scripts/prepare.cmd` is simply there, rather than something that
had to be placed on the server and kept in step with the code by hand.

### Timeouts

`deploy_timeout` starts when the last byte arrives, so it never has to leave
room for the upload — size it for what your hooks do. The server is told the
value and stops at it too, rather than working on after the client has given up.
It cuts both ways: too low a value aborts a deployment that was going to
succeed, part way through, so raise it for a package whose hooks run an
installer. A hook still running when the deadline passes is killed with it,
rather than left on the server with nobody waiting for it.

The upload itself has no timeout and needs none. How long a transfer
legitimately takes depends on the package and the link, so any limit would have
to be revisited whenever either changed. A connection that breaks reports an
error on its own; one that is silently gone — a suspended machine, a pulled
cable, an expired NAT entry — is what HTTP/2 keepalive is for, and both ends
enable it.

### On the server

The server generates its own `adeploy.toml` on first run, and during
`adeploy server install`, then reports at startup where it is, which identity it
is presenting, and whether any client key is authorized yet. Its TLS certificate
is generated the same way and at the same time. It never overwrites a file that
already exists.

It reads only the copy beside its own binary, never one found by searching
upward — starting the server from inside a project checkout must not make it
adopt that project's configuration. `--config <path>` points it elsewhere.

There are three settings, and none of them mentions a package:

```toml
[server]
listen_port = 6060
tls = true
allowed_keys = []
```

- `listen_port` — the port to bind. Clients dial it through their own `port`;
  the two are separate fields because they are separate decisions that merely
  share a default. Changing it requires a restart.
- `tls` — serve over TLS, using the certificate generated beside this file on
  first run. On by default; see [Who the client thinks it is talking
  to](#who-the-client-thinks-it-is-talking-to).
- `allowed_keys` — the base64 Ed25519 keys permitted to deploy, which the client
  prints when it is rejected. Keys approved through pairing live in
  `paired.toml` instead and are simply unioned with these; `adeploy server
  clients` shows both.

The server reloads this file when it changes, so adding a key does not require a
restart.

## Pairing

```bash
adeploy pair 192.0.2.10            # on the client; prints its key fingerprint
adeploy server clients             # on the server: everyone, and what to do about them
```

Pairing settles both directions in the one trip an operator already makes. The
client records which server answered, and the server queues the client's key for
a person to approve:

```
# on the server, at startup
Server identity: SHA256:Ee8JHrdPheCK6SvTa7mFvHMAU6lNW2li+LtGjktdnRg

# on the client
This machine's key fingerprint: SHA256:DPHRhwwNlQe81FJZJcXN0fwnhMsqRX0hJ4Ty7awZzHk
Server identity: SHA256:Ee8JHrdPheCK6SvTa7mFvHMAU6lNW2li+LtGjktdnRg  (recorded)
Check it matches the `Server identity` line in 192.0.2.10's own log
```

**Comparing the fingerprints is what makes any of this mean anything**, rather
than trusting whoever reached the queue first or whoever answered on that
address. `adeploy server clients` is where that comparison happens: it shows
who is waiting, who is trusted, and who has been refused, and acts on the row
in front of you rather than on a selector copied between commands. The running
server picks up every decision without a restart.

```
Waiting for approval
   1  linus-laptop  192.0.2.44:51288   SHA256:DPHRhwwNlQe81FJZJcXN0fwnhMsq...   2m ago
Trusted
   2  build-box     192.0.2.12:40110   SHA256:Mn2Zx8kLpQr4TvWy6BcDeFgHiJk...   3d ago

Pick a number, [r]efresh, [q]uit: 1

  linus-laptop  192.0.2.44:51288
  SHA256:DPHRhwwNlQe81FJZJcXN0fwnhMsqRX0hJ4Ty7awZzHk
  Compare that fingerprint with the one printed on the client itself.
  [a]pprove, [r]eject, [Enter] to go back:
```

A trusted row offers `[r]evoke` instead. Keys from `allowed_keys` are listed
too, marked as belonging to the configuration file, which is not rewritten
here. Piping the output prints the lists and exits without prompting.

Rejecting answers one request. The client is told it was refused and exits
non-zero, the refusal is dropped as it is delivered, and asking again starts a
fresh request — so refusing the wrong row costs somebody one more `adeploy
pair`, not their ability to pair at all.

`adeploy pair` then holds until somebody has decided, because the person
running it is usually the person walking over to approve it:

```
Request queued on 192.0.2.10: Queued for approval
Approve it on 192.0.2.10 with:  adeploy server clients  (fingerprint SHA256:DPHRhww...)
Waiting for that approval - Ctrl-C is safe, the request stays queued
Approved by 192.0.2.10 after 34s; deployments will work now
```

Ctrl-C really is safe: the request is already recorded on the server, so
stopping the wait costs the wait and not the work. `--no-wait` returns as soon
as the request is queued, for scripts that have nobody to wait for.

`Pair` is the one method that cannot require a key, since establishing one is
the point. A request is self-signed, which proves the sender holds the key it is
presenting — enough to stop anyone queueing keys they do not control — and then
waits for a human. The queue is bounded and deduplicated by key, so a client
polling while it waits cannot fill it.

Approvals live in `paired.toml` beside the server binary, written by the tool
and kept out of `adeploy.toml` so the server never rewrites a file an operator
hand-edited. `allowed_keys` still works and is simply unioned with what has been
approved.

### Who the client thinks it is talking to

There is no certificate authority here and nothing to obtain or renew. The
server generates a certificate for itself on first run — `server.crt` and
`server.key`, beside its configuration — and the client records it the first
time it pairs, in `known_servers.toml` beside its own key. Every later
connection is checked against that record. It is what SSH does with host keys,
for the same reason: both machines belong to you, so you are the authority.

The certificate is issued for the fixed name `adeploy` rather than for a host
name or an address. A server cannot know which of its addresses a client will
dial, and an address baked into a certificate is one that cannot change without
reissuing it. What decides trust is the recorded certificate, so the address is
free to move.

A server whose identity no longer matches is refused outright:

```
Failed to connect to 192.0.2.10:6060: transport error: invalid peer certificate:
UnknownIssuer. If 192.0.2.10 was rebuilt or replaced, its identity changed; run
`adeploy pair 192.0.2.10 --force` after checking that is what happened
```

`--force` accepts an identity that differs from the one on file, for a machine
that really was rebuilt. Without it, nothing overwrites a recorded identity.

Both ends can be turned off with `tls = false` — under `[server]` on the server,
under `[defaults]` or one `[remotes.*]` on the client — which exists for
reaching a server too old to offer it. It leaves every deployment readable by
anyone on the network, and lets any machine on that address pass for the real
one.

## What travels to the server

The client opens with a small signed message: the package name, the archive's
size and SHA256, its public key, a nonce, a timestamp, the deploy timeout, and
the manifest — where to unpack, whether to snapshot, and the commands to run.
The archive itself only follows once the server has accepted that.

**The signature covers all of it.** Covering the description rather than only
the bytes is what binds an archive to the package it was meant for, fixes where
it lands and what runs around it, and — with the nonce and timestamp — stops a
captured request being replayed at all.

Two things follow from checking the key before the payload. An unauthorized
caller never gets to send an archive, and the server writes what it does receive
straight to a staging file rather than holding it in memory, so its memory does
not grow with the size of the package. The staged file is removed once the
deployment ends, however it ends.

The declared size is a claim, not a fact: the server counts the bytes it
actually receives, cuts off a stream that runs past it, and refuses one that
stops short.

Worth being plain about what this does and does not buy. An approved client
chooses the commands the server runs as itself, so approving one is trusting it
with the machine. The server refuses only what is certainly a mistake — a
relative `deploy_path`, the filesystem root, or its own directory.

The signature and the transport answer different questions, which is why both
are there. The signature says who is asking and binds these bytes to this
package, this path and these hooks — it is checked before a byte of archive is
accepted, and holds across the upload, the rollback and everything else. TLS
says nothing about any of that; it keeps the archive from being read on the way
and tells the client which machine it reached.

## How a deployment is applied

```
receive → verify hash → unpack beside the live deployment
        → before_deploy   (working directory: the unpacked package)
        → carry over what the package does not ship
        → snapshot
        → swap into place
        → after_deploy    (working directory: the live deployment)
```

The new tree is assembled under a sibling directory and moved in with a rename.
Unpacking straight over the deploy path meant a failure part way through left a
directory that was neither the old deployment nor the new one, and a running
service could read half-replaced files for as long as extraction took. **A
failed deployment now leaves the live one exactly as it was**, and nothing is
left beside it.

Unpacking happens before the hook, so the slowest step is outside the downtime
window and before-deploy has the package's scripts to hand. Carrying over the
old content happens after it, once the hook has stopped whatever was writing —
copying a live directory can otherwise capture a file mid-write.

Files the package does not ship survive: uploads, logs, a database. A deployment
overwrites what it ships and leaves everything else.

The snapshot and the swap are the same move. The live deployment has to leave
the deploy path either way, so with `backup_enabled` it is renamed into the
snapshot directory rather than copied there and then deleted — nothing is
copied at all when the two sit on one filesystem. Snapshots kept on another
filesystem are out of `rename`'s reach and still copied.

Only one deployment may hold a `deploy_path` at a time. A second is refused
outright, before it uploads anything: two of them would each assemble a tree
and then swap in whatever order they finished, and the one that lost would
snapshot and carry over from the winner's half-installed state.

A server killed mid-swap leaves the deployment in the directory it had just
been moved to, and the next one puts it back rather than mistaking an empty
deploy path for a first install. Working directories and staged uploads from
a run that died are removed once they are old enough that nothing could still
be using them.

## Rolling back

```bash
adeploy rollback <host> <pkg> --list          # what is available
adeploy rollback <host> <pkg>                 # the most recent snapshot
adeploy rollback <host> <pkg> --to backup_20260914_100512
```

A deployment with `backup_enabled` snapshots the directory before replacing it,
and those snapshots are what rollback restores — through the same phases and the
same hooks, because putting files back has the same requirement: the service
holding them has to stop first and start after. Restoring always replaces rather
than merges, since a snapshot is a complete picture of what the directory held.

It also snapshots the current state before restoring, so a rollback can itself
be undone.

Snapshots live beside the server binary, under the package's name:
`<server dir>/demo/backup_<timestamp>/`. Where they go is the server's own
business rather than something a client asks for — a client that could place them
could also point them at a directory it wanted emptied by the next rollback.
Names carry a timestamp to the second and are disambiguated when two land inside
the same one, which a rollback does by design.
