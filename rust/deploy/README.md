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
