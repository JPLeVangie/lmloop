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

**Behaviour**

- Launch goes through the edge's authenticated browser session. Nothing else can start a run. (A direct call to the backend uses Basic auth instead.)
- `202` means the run was published. `504` means the outcome is uncertain. Check the run list. Nothing retries automatically.
- STOP-NOW takes effect immediately, except while a commit is being published.
- Failed or stopped edits are kept. There is no resume, merge or deploy.
- One run slot per server process. It is not a cross-process checkout lock, so two servers can still collide on one checkout.
- The worker edits the approved checkout in place. The review screen says so and labels the field "Policy revision".

**Production gate.** All of these must hold before this runs in a live unit, and Pierce must approve it:

- A pinned, isolated launcher and relay that protect state, auth and config, and make no auxiliary inference calls.
- A fresh watchdog file for each run, provisioned per run. A nonempty or unusable file is rejected. A static path passes if the file is zero-byte, but production still requires a fresh file each run.
- Immutable gate and tooling.
- An exclusive checkout, with cross-process exclusion.
- Policy approval and a rollout decision.

**Tested so far.** Parent-verified final pass, after the follow-ups above landed. All of it is scratch and local; none of it ran in a live unit.

- Full local suite green: `cargo fmt --check`, `clippy -D warnings`, 140 Rust tests (15 in web), gateway 41 (including a real-Rust dirty-checkout refusal with HEAD and the edit preserved), `sse_progress` 14, Node 29, `git diff --check` clean.
- Fresh Chromium rerun against the rebuilt binary, at 390x844 and 1440x900: one POST returning 202 each, no POST before acknowledgement, no overflow or page errors, run detail "1 of 1 committed". Scratch commits `695fa4e` (tree `86952012`) and `4a230148` (tree `63845ff1`) each match `snapshot_captured` and the `commit_intent` tree with the gate passed. Only `notes/objective.txt` changed. The scratch repo is clean and all QA listeners are down.
- Live stays login-only, a release without this feature, and healthy.

**Still open.** Cross-process exclusion or an exclusive checkout, a pinned isolated launcher and relay, per-run watchdog provisioning, policy approval and rollout, a real device, and hosted CI.
