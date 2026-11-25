# Rust preview: user-systemd packaging (lm-1x4)

Two user units, `lmloop-rust-web` (Rust backend, 127.0.0.1) and `lmloop-rust-edge`
(OIDC edge, 127.0.0.1), run from an immutable release. Both bind loopback only and
neither can be told otherwise, so a TLS proxy on another machine cannot reach them
directly (see Topology). Nothing here touches the Python `lmloop-web` unit, its
checkout or its state (`~/.local/state/lmloop`).

## Topology

```
browser/Android -> HTTPS proxy (e.g. Caddy) -> private transport -> 127.0.0.1:<edge port> -> 127.0.0.1:<backend port>
```

Pick one way to make that middle hop; this repo configures none of them:

- **Proxy on the same host as the edge.** `reverse_proxy 127.0.0.1:<edge port>`. Nothing else is needed.
- **Proxy on another host.** The hop must be an authenticated, encrypted private link that *ends at this host's loopback*, such as an SSH tunnel or a VPN with a forwarder that listens on the VPN address only. The edge speaks plain HTTP and trusts nobody by network position, so do not forward its port onto a LAN or public interface.
  - If the tunnel's listening end is on the proxy host, the proxy upstream is that tunnel's local address.
  - A forwarder on this host needs its own firewall rule limiting it to the proxy host.

Whichever you choose, the proxy must send the public `Host` unchanged (the edge answers 421 otherwise), keep the browser's `Origin` and cookies, and terminate TLS. The edge does no TLS and does not read `X-Forwarded-*`.

## Layout

```
~/lmloop-rust/releases/<commit>/    tracked tree of one commit + rust/target/release/lmloop; chmod -R a-w
~/lmloop-rust/current -> releases/<commit>
~/.config/lmloop-rust/edge.env          0600  from edge.env.example
~/.config/lmloop-rust/backend-auth      0600  one line `username:password`, <= 1024 bytes
~/.config/lmloop-rust/oidc-client-secret, session-secret   0600
~/.local/state/lmloop-rust/             0700  Rust state, fresh (systemd StateDirectory)
```

The edge imports `config.py`, `harness*.py`, `models.py` and `web/` from the
release root, so a release is the whole tree, not just `rust/`.

## Build a release (from a commit, never the dirty checkout)

```sh
export CARGO_HOME="$HOME/.local/share/lmloop-rust/cargo" RUSTUP_HOME="$HOME/.local/share/lmloop-rust/rustup"
export PATH="$CARGO_HOME/bin:$PATH"
REL=~/lmloop-rust/releases/<commit>
mkdir -p "$REL" && git -C <repo> archive <commit> | tar -x -C "$REL"
cargo build --release --locked --manifest-path "$REL/rust/Cargo.toml"
chmod -R a-w "$REL"
ln -sfn "$REL" ~/lmloop-rust/current
```

## Install and start

```sh
mkdir -p ~/.config/systemd/user ~/.config/lmloop-rust && chmod 700 ~/.config/lmloop-rust
cp lmloop-rust-web.service lmloop-rust-edge.service ~/.config/systemd/user/
cp edge.env.example ~/.config/lmloop-rust/edge.env   # edit, chmod 600, create the secret files
systemctl --user daemon-reload
systemctl --user enable --now lmloop-rust-web lmloop-rust-edge
```

Edge needs `PyJWT[crypto]` and `requests` for `/usr/bin/python3`. To roll a new
release, repoint `current` and `systemctl --user restart lmloop-rust-web lmloop-rust-edge`.

## Check without starting

```sh
systemd-analyze verify --user ./lmloop-rust-web.service ./lmloop-rust-edge.service
```
(`ExecStart` paths are reported missing until a release exists.)

## New run (scratch only; not enabled in live units)

Browser launch of a new run exists in source for scratch use. Neither unit above
enables it, and no unit flag or rollout turns it on. Background: [Native pilot
run](../README.md#native-pilot-run), [Basic TUI](../README.md#basic-tui),
[Auth edge, units and CI](../README.md#auth-edge-units-and-ci).

**Inputs**

- An explicit state root, a `0600` `--auth-file` and a `0600` `--launch-registry` (version 1). `--launch-registry` without `--auth-file` is refused at startup.
- The registry points at private, external, single-task manifests. The registry is read at startup only. The manifests are reread on both review and launch.
- Each manifest carries a reviewed repository-relative scope and caps. See [Worker watchdog](../README.md#worker-watchdog-opt-in) for the caps.
- Template scopes are checked with `git::check_scope`. An invalid scope makes the template unusable ("the project template is not usable"). So does a template with no room for a 4096-byte objective.
- The watchdog progress file must be zero-byte at preflight and again at launch, on both options. An absent, partial or used file makes the run ineligible ("the worker progress file is not fresh or usable"). `Dog::start` applies the same zero-byte check.
- Optional `watchdog.writer_gid` selects the trusted progress-file writer group. For the backend CLI, pass `--owner-uid` and `--writer-gid` together to enable the identity handoff. The progress file must be a fresh, single-link file with exact mode `0620`, owned by the backend UID, under a protected parent outside the checkout. Only the dedicated relay is trusted to write it; do not give the backend a worker path or relay credentials. Without the pair, legacy mode is unchanged. Separate backend and relay identities have not yet been runtime-proven; no unit or account changes were made, and launch is not enabled.

**Behaviour**

- Launch goes through the edge's authenticated browser session. Nothing else can start a run. (A direct call to the backend uses Basic auth instead.)
- `202` means the run was published. `504` means the outcome is uncertain. Check the run list. Nothing retries automatically.
- STOP-NOW takes effect immediately, except while a commit is being published.
- Failed or stopped edits are kept. There is no resume, merge or deploy.
- One run slot per server process. Separately, an advisory, nonblocking lock (`lmloop-checkout.lock` in the worktree's own Git directory) excludes a second run on the same Git worktree across Rust processes and state roots. Linked worktrees have separate Git directories, so they are independent.
- A busy checkout returns a typed `409` (`busy`) before launch, with no run directory created. A standalone resume that may write also returns `409` when the checkout is held. The run's own lease metadata may still be updated on a busy resume.
- The lock file is never unlinked or truncated, and a symlinked or hard-linked one is refused.
- **Limit.** The lock is advisory and dies with the process that holds it. It does not protect against a worker orphaned after its launcher dies, or against arbitrary Git writers that never take it. No NFS or other network filesystem was verified.
- The worker edits the approved checkout in place. The review screen says so and labels the field "Policy revision".

**One-shot projects (optional).** A registry entry may add `"one_shot": true`. Without it, nothing changes: opt-in stays the default and existing entries and revisions are untouched. The registry is read at startup, so changing the flag needs a restart. The flag is part of the reviewed "Policy revision", so a page reviewed before the change gets `stale_revision`.

- **Scope.** The claim is the file `v1/one-shot/<project id>` under the private state root (mode `0600`, in a private directory). It is keyed to that state root and project ID. It applies only to browser launch through the backend API (`POST /api/runs`). The CLI and TUI launch paths, other project IDs and other state roots do not consult it.
- **Timing.** Checks for objective, revision and eligibility run first, and none of them spends the claim. The claim is then rechecked and taken under the engine's checkout guard, before the run is published and before a worker starts. The guard stays held through the run. The file, its directory, `v1` and the state root are fsynced before the launch continues.
- **Fixed `409` outcomes.** All are typed, with fixed text and no paths. The edge relays a reason only if it is on its fixed allowlist; anything else, such as a path or claim contents, becomes the generic "The project is not ready to launch."
  - `ineligible`, "the one-shot launch was already used": the claim exists. Review shows the same reason and the project is not eligible.
  - `ineligible`, "the one-shot claim could not be checked": the claim file or its directory is unreadable, malformed, for another project, a symlink, or in a shared (non-private) directory. Nothing is created, removed or rewritten.
  - `ineligible` with a checkout reason, or `busy` ("another run holds this checkout; nothing was started"): refused under the guard, no run directory and no claim.
- **What keeps the claim.** Once taken, it is never removed by code. A run that fails, never produces a worker pulse, is stopped with STOP-NOW, or ends with an ambiguous outcome still holds it. A `504` (`launch_pending`) is ambiguous: the launch may still take the claim and create a run after the reply, so treat the claim as possibly held. Two processes sharing one state root cannot both launch: one gets `202`, the other `409`.
- **Operator duty.** A spent or unverifiable claim is not cleared automatically and nothing here recommends clearing it. Before any deliberate offline re-arm, look at the Runs list, the processes and the run evidence (events, checkout, progress file) to learn what the earlier launch did. Keep the old claim and that evidence rather than discarding either.
- **Limit.** Durability rests on fsync. Power loss was not simulated.

**Production gate.** All of these must hold before this runs in a live unit, and Pierce must approve it:

- A pinned, isolated launcher and relay that protect state, auth and config, and make no auxiliary inference calls.
- A fresh watchdog file for each run, provisioned per run. A nonempty or unusable file is rejected. A static path passes if the file is zero-byte, but production still requires a fresh file each run.
- Immutable gate and tooling.
- An exclusive checkout. The advisory lock above is not enough: it cannot stop an orphaned worker or an arbitrary Git writer.
- Policy approval and a rollout decision.

**Tested so far.** Parent-verified final pass, after the follow-ups above landed. All of it is scratch and local; none of it ran in a live unit.

- Full local suite green: `cargo fmt --check`, `clippy -D warnings`, 140 Rust tests (15 in web), gateway 41 (including a real-Rust dirty-checkout refusal with HEAD and the edit preserved), `sse_progress` 14, Node 29, `git diff --check` clean.
- Fresh Chromium rerun against the rebuilt binary, at 390x844 and 1440x900: one POST returning 202 each, no POST before acknowledgement, no overflow or page errors, run detail "1 of 1 committed". Scratch commits `695fa4e` (tree `86952012`) and `4a230148` (tree `63845ff1`) each match `snapshot_captured` and the `commit_intent` tree with the gate passed. Only `notes/objective.txt` changed. The scratch repo is clean and all QA listeners are down.
- Live stays login-only, a release without this feature, and healthy.
- Checkout exclusion, parent-verified separately: `cargo fmt --check`, 54 serial tests and 17 web tests. No production change.
- One-shot claim, parent-verified: five one-shot web tests, one serial hook test and 42 gateway tests. Independent security review cleared the earlier lock and durability blockers. Power loss was not simulated. Scratch only; live remains disabled.

**Still open.** An exclusive checkout (the advisory lock does not cover orphaned workers, arbitrary Git writers or NFS), a pinned isolated launcher and relay, per-run watchdog provisioning, the watchdog handoff, a failed authenticated model smoke, policy approval and rollout, a real device, and hosted CI.
