# lmloop Rust rewrite: M2/M3 fixture checkpoint

Tracked under `lm-1x4`, this isolated Rust slice extends M1's native status and diagnostic gate with an M2 serial fixture runner and M3 operator interfaces. It remains a fixture checkpoint, not a production replacement for Python lmloop. The operator API does not expose the task objective, and this work does not establish production-model behavior or project parity.

The fixture runner drives an operator-supplied executable with a versioned manifest, captures scoped Git changes, runs the gate against an exported snapshot, and commits only after the gate passes. The TUI and local web console use the same operator API. Approval and change requests record a final decision only; neither merges nor changes Git. The UI identifies these runs as fixture evidence, not production verification.

A separate `pilot-run` command runs a one-to-three-task manifest through the same engine with a real OpenCode launcher. `run OBJECTIVE --manifest FILE --run-id ID` reaches the same engine from the normal command, with stricter manifest checks. Both are labelled pilot evidence, are one-shot, and are not production. See [Native pilot run](#native-pilot-run) and [Normal CLI: `run --manifest`](#normal-cli-run---manifest). `run` without `--manifest` stays disabled (exit 69).

Compatibility inventory and ranked scope: [`compatibility.md`](compatibility.md).

## Acceptance gate (updated by Pierce)

The Rust rewrite starts from **fresh Rust state only**. It does not import legacy Python run, config or state data, or data from any other loop. It carries no compatibility shim for legacy data, and the gate requires no legacy migration or rollback rehearsal. The Python source stays a reference for behavior. The live Python checkout and its service are protected, and nothing here deploys. A real model pilot needs separate authorization. The no-skill live run passed. The first five Museum live runs failed with rc 10 and no commit, and the sixth passed once (see [Native pilot run](#native-pilot-run)).

**Cutover decision (Pierce).** OIDC and Android are retained: both must be carried over before the Python service is swapped. Changes to the Python baseline stay additive only. No swap has been scheduled or approved.

None of this drops the native work: CLI, TUI and dashboard features, plus the project, API, auth (including OIDC), PWA, Android and fixture CI gaps listed below, stay in scope.

## What this checkpoint includes

| In M2/M3 | Still out of scope |
|---|---|
| `fixture-run` / `fixture-resume`: serial tasks with a fake worker, exact tree gate and guarded commit flow | Production model execution, native project, API, auth, PWA, Android and fixture CI gaps |
| `pilot-run` and `run --manifest`: one to three tasks, one-shot, explicit `--state-root`, trusted manifest launcher, operator gate; no resume | Repeated or production-grade skill-heavy live results (one Museum pass so far), production use, objective-driven `run` (still exit 69 without `--manifest`) |
| `status`: native run and ownership view | Objective in the operator view; full Python CLI/API compatibility |
| `tui` and bare `lmloop`: keyboard operator console over the shared API; bare `lmloop` adds a reviewed one-shot pilot launch (see [Basic TUI](#basic-tui)) | General TUI run launch, PWA, Android, authentication, production deployment or service cutover |
| `web`: loopback-only JSON API and browser console; optional Basic auth via `--auth-file`, and an HTTPS `--public-origin` for a same-host TLS proxy | Direct non-loopback bind; public access without the operator-managed proxy/TLS boundary |
| `approve` / `request_changes`: final decision record only | Merge, deploy, delete, archive or other destructive controls |
| `run --dry-run` and `diagnostic-gate` from M1 | A production claim from fixture results or approval |

Git is the witness for fixture task changes. The worker stream only establishes a clean worker stop. A successful fixture run remains `fixture_runner_not_production_evidence`; M1 diagnostic snapshots remain `diagnostic_only_not_bound_to_git_snapshot`.

## Build and run

The toolchain lives in a user-local prefix (it was installed there for this project; no shell profile was changed):

```sh
export CARGO_HOME="$HOME/.local/share/lmloop-rust/cargo"
export RUSTUP_HOME="$HOME/.local/share/lmloop-rust/rustup"
export PATH="$CARGO_HOME/bin:$PATH"

cd rust
cargo build --locked
```

The binary is `rust/target/debug/lmloop`. Try it against a scratch state root so nothing lands in your real state directory:

```sh
BIN="$PWD/target/debug/lmloop"
S="$(mktemp -d)"        # private (0700) and owned by you, which the state-root checks require

# Dry run: prints a plan, exits 0, and creates nothing under $S.
"$BIN" --state-root "$S" run --dry-run --repo .. "try the plan output"
ls -A "$S"              # prints nothing

# Diagnostic gate: argv goes after `--` and runs without a shell.
"$BIN" --state-root "$S" diagnostic-gate demo --json -- /usr/bin/true
echo "exit $?"          # 0

# Read it back.
"$BIN" --state-root "$S" status demo --json
```

The read-back shows `phase: gate_finished`, `verified_completion: false` and `gate_evidence_reusable: false`. State lands in `$S/v1/runs/demo/` (`status.json`, `owner.lock`, `logs/gate-*.log`).

### Run the fixture checkpoint

The fixture manifest trusts the absolute worker and gate commands it names. Use only a disposable repository and a fake worker; do not point it at a production model. From `rust/`, create a scratch repo with a clean initial commit and private state directory, then write a manifest like this (replace the paths with absolute paths):

```json
{
  "manifest_version": 1,
  "repo": "/absolute/path/to/scratch-repo",
  "worker": { "argv": ["/absolute/path/to/rust/tests/fixtures/fake-opencode"], "timeout_secs": 30 },
  "gate": { "argv": ["/absolute/path/to/rust/tests/fixtures/gate-no-fail"], "timeout_secs": 30 },
  "tasks": [
    { "id": "t1", "prompt": "ok|src/one.txt|fixture change", "scopes": ["src/one.txt"] }
  ]
}
```

Run and inspect the fixture, then start the local interfaces in separate terminals as needed:

```sh
"$BIN" --state-root "$S" fixture-run demo --manifest /absolute/path/to/manifest.json
"$BIN" --state-root "$S" status demo --json
"$BIN" --state-root "$S" tui
"$BIN" --state-root "$S" web --port 0
```

Set `BIN` as in the build example above and provide the `manifest.json` described above. The fixture web server lists persisted runs; start it after the fixture run has created one.

`fixture-resume demo` continues a paused fixture run; its journal repair contract is under [Fixture-resume journal recovery](#fixture-resume-journal-recovery). The web command prints its loopback URL; open that address on the same machine. Its JSON API and page are fixture/operator tooling, not a production service. The server always binds only `127.0.0.1`, checks the exact `Host` on every request and same-origin `Origin` plus JSON content type for POST, emits no CORS permission, and applies a restrictive CSP and `no-store` response headers. Optional `--auth-file FILE` enables HTTP Basic auth for every route. The file must be a regular file owned by the current user, mode `0600`, at most 1024 bytes, containing one line `username:password` with both fields non-empty; one trailing newline is allowed. Do not put credentials in command-line arguments. Optional `--public-origin https://host[:port]` requires `--auth-file` and allows requests from that exact HTTPS origin when `Host` matches. Use it only behind a same-host TLS-terminating proxy that forwards the expected `Host` and, for POST, `Origin`; the proxy and TLS boundary are operator-managed. On the public origin a `GET` may omit `Origin`, because browsers do on top-level navigation. A present `Origin` must still equal the public origin, and `POST` always requires the exact `Origin`. A local TLS fixture with Chromium (not the deployed proxy) returned a 401 challenge, then an authenticated 200 for the page, assets and API. No deployed proxy/TLS setup was tested, and the setting does not add a non-loopback bind. There is no PWA, Android client or cluster browser access. Approval is a durable decision record only and does not move `HEAD`.

**Android read aliases (additive).** The server also answers the reads Android's client makes, with the same Host/Origin guard: `GET /health`, `GET /api/config` (`poll_seconds` 5.0, `read_only` false) and `GET /api/runs/lmloop/<id>`. `/health` is the one route that skips Basic auth, because Android's setup probe sends no credentials; it still needs the exact `Host` (and any present `Origin`) and returns only fixed fields: `status` "ok", `auth` ("basic" or "none"), `oidc` false and `read_only` false. Every `/api/runs` row and the project view carry the original Rust fields plus `route_id`, `project` (always `lmloop`), `state`, `title` (the run id), `paused` and `stopping`. `state` is coarser than `phase`: `failed`, `interrupted`, `stopped` and a diagnostic `gate_finished` read `stopped`; `completed` only when a serial run is `complete` with all tasks done and no failure; an unverifiable `complete` reads `unknown`. Fields the Rust store lacks (iteration, plan, model, agent, commits) are omitted, not invented. The dashboard also accepts Android's `#lmloop/<runId>` hash. API `Basic` auth is unchanged. There is no bearer, device-token or OIDC support, and no Android app was run against it.

For the command set and verified checkpoint limits, see [`compatibility.md`](compatibility.md).

### Native pilot run

`pilot-run` is the only command that may launch a worker named `opencode`. It reuses the fixture engine: scoped Git capture, exact-tree operator gate, guarded commit. It is a pilot, not a replacement for `run`, and not production.

```sh
"$BIN" --state-root "$S" pilot-run <run-id> --manifest /absolute/path/to/pilot-manifest.json
```

- **Explicit state root only.** `--state-root` must be on the command line. `LMLOOP_RUST_STATE_ROOT` and the default root do not count (exit 2). The root must not contain or sit inside the manifest repo (exit 2).
- **Same manifest format, one to three tasks.** Version 1, absolute `repo`, absolute `worker.argv[0]` and `gate.argv[0]`, timeouts (the worker's `timeout_secs` is optional only with a `watchdog`, see [Worker watchdog](#worker-watchdog-opt-in)), literal repo-relative `scopes`. Tasks run serially in manifest order with distinct session ids. The manifest is trusted: the operator chooses the launcher and the gate, and both run with the operator's privileges and no shell. Each task prompt is the worker's final argument; the existing operator gate runs for each task before its commit.
- **Operator gate.** The commit happens only after the gate passes against the exported tree. A failing gate, or a change outside `scopes`, commits nothing.
- **One-shot, no resume.** A pilot run is persisted as `pilot`. `fixture-resume` refuses it (exit 8, worktree preserved), and a second `pilot-run` on an existing run id is refused. Start a new run id. The operator API offers only Stop now as a control for a pilot; pause, boundary stop and resume are unavailable. Approve and request-changes stay decision records.
- **Honest labels.** Success prints `pilot evidence; not production verification`. The operator API reports the evidence as `serial_pilot_not_production_verification` and the run kind as `serial_pilot`. Approval records a decision only and changes no Git state.
- **No sandbox here.** The state-root guards above are all the isolation the Rust side provides. Any sandbox, network policy or credential isolation around the worker is external, and the operator's responsibility.
- **Exit codes.** The same as `fixture-run`: 0 committed, 2 usage, 7 owned elsewhere, 8 reconciliation, 10 task failed (uncommitted work preserved), 11 stopped.

Evidence so far, reported by the parent session (this doc pass reran none of it):

| Run | Result |
|---|---|
| Offline: real OpenCode v2.2.7 against a scripted upstream | The worker loaded the Impeccable skill and wrote a site. The site passed the exact-tree Chromium gate and was committed in a scratch repo. This shows the launcher, gate and commit path work with a real OpenCode binary. It says nothing about any model. |
| Live, no-skill: small serial run, scratch `/tmp/opencode/lm1x4.live.0Y2opk/live` (parent ran `verify.sh acc` and `verify.sh live` independently) | Three exact ordered commits, each in a fresh session; requests that overlapped were rejected. Small run only: it does not show zero auxiliary inference, and no claim is made that there was none. |
| Live: Museum of Lost Umbrellas, `cliproxyapi/agent-local`, separately authorized by the user (first attempt) | The worker loaded the skill, then timed out at the 540 s worker limit. The last of five pinned stream requests hit the 240 s bridge deadline. Exit 10: no site edits, no gate run, no screenshot, no commit. No evidence identifies a model-specific root cause. Artifacts: `/tmp/opencode/lmloop-live.M2z4qg`. |
| Live: Museum reruns, scratch `/tmp/opencode/lm1x4.mus.th4sLL` and `/tmp/opencode/lm1x4.mus2.dpaiw0` | Hidden maintenance agents were disabled and a local filter was in place; upstream saw zero auxiliary requests. A scripted pass met the original desktop and mobile gate. Both live runs still ended rc 10 on timeout after loading the full skill and reference files, before any first write or gate. The second run asked for the first write immediately and did no better. No commit, no cutover. |
| Live: Museum, longer approved run (900 s request cap, 1500 s worker cap), same prompt, model, full skill and gate; scratch `/tmp/opencode/lm1x4.mus3.XSkZhD/live` | Ended in 935 s with rc 10 and no completed writes. Request 3 streamed 3,045,520 bytes before its deadline. The worker recorded an incomplete write (`stream ended without finish_reason`), and the local latch stopped an upstream retry. No gate ran, no commit, and the base HEAD `600324a` stayed clean. The offline latch tests and the scripted gate pass, but that is not live evidence. No cutover. |
| Live: Museum, fifth attempt, one run, same prompt and model, same 120 s gate (same hash); scratch `/tmp/opencode/lm1x4.mus4.CKDUcQ/live`. The 15-minute absolute bridge deadline was disabled by setting idle-secs to 300; the 3600 s worker backstop was unchanged in Rust. The run survived an agent server restart. | The worker timed out at 3601 s (rc 10). Two main upstream requests succeeded, taking 1424 s and 1679 s. `index.html` was written at about t+3156 s (~52m36s), 22,499 bytes, and was the only file changed; `style.css` stayed unchanged and empty. The live gate never ran and there were 0 commits. The base checkout (`3c6e81c`) holds the uncommitted `index.html`, which was preserved. A scripted rehearsal gate passed, but it is separate and is not live evidence. No cutover. |
| **Live: Museum, sixth attempt, passed.** One run, watchdog (600 s startup and idle, no `timeout_secs`) with the SSE beacon, scratch `/tmp/opencode/lm1x4.mus5.THm2By/live`, run `r1`. The parent reports the desktop/mobile gate was unchanged; this doc pass did not compare the gate hash against earlier runs. | The doc pass read the artifacts. `rc.txt` is `lmloop rc=0` and `lmloop.out` is the pilot-committed line. `run.json` is `complete` with one task done. One commit, `fffece2` (`lmloop pilot: t1`), sits on top of `base` (`f587da2`) and changes `index.html` and `style.css` (2,113 insertions, 1 deletion). The checkout was clean afterwards. The gate `report.json` is `pass: true` with no failed checks at both 1440 and 390. The run took 5,213 s (`start.ts` to `end.ts`, about 87 minutes). `watchdog.json` is `ended` with 4,422 model pulses, 20 completed tools (last: `edit`) and no warning or trip. This is a single run: one pass does not show repeatability, zero auxiliary inference or any model-specific cause for the earlier failures. The run ran inside an outer bubblewrap sandbox built by the operator's script, not by Rust. No cutover. |

Together these show the native path commits ordered, fresh-session work in a small live run, and that one skill-heavy Museum run has now passed the live gate and committed under the watchdog. They do not establish repeatability, production readiness, Python parity or zero auxiliary inference in general. The first five Museum attempts failed (the fifth wrote its first file only after about 52 minutes, too late for the worker backstop). The sixth removed the fixed worker cap and let the watchdog end the run on silence only, and it passed; the data does not say whether that change, run-to-run variation or something else explains the difference. Fake-worker tests cover three-task order, distinct sessions, and stopping after task failure.

`--state-root` also reads `LMLOOP_RUST_STATE_ROOT`. Without either, the root is `$XDG_STATE_HOME/lmloop-rust`, then `~/.local/state/lmloop-rust`.

### Normal CLI: `run --manifest`

Checked against `src/cli.rs` (`RunArgs`, `run`, `manifest_prepare`, `launch_pilot`) and `tests/serial.rs` (`run_with_a_private_manifest_uses_the_pilot_engine_and_its_labels`, `run_refuses_unsafe_manifest_use_and_creates_nothing`). This doc pass read the code and tests and reran neither.

```sh
"$BIN" --state-root "$S" run "any non-empty text" --manifest /absolute/path/to/pilot-manifest.json --run-id r1 --repo /absolute/path/to/repo
```

It is the same one-shot pilot engine as `pilot-run`, reached from the normal command. The new flags:

- **`--manifest FILE`** names the operator manifest. It requires `--run-id`.
- **`--run-id ID`** is the new run id (`[A-Za-z0-9_-]`, 1-64 chars). It requires `--manifest`.
- **`--repo DIR`** is the checkout to run in (default: the current directory). With `--manifest` it must be the repo the manifest names.

Limits and refusals (all exit 2 and create no run unless noted):

- **The objective is not used.** `OBJECTIVE` must still be non-empty and at most 256 KiB (`-` reads stdin), and `--repo` must pass the structural `.git` check. It is then dropped: the worker receives only the manifest's task prompts, and stderr says so (`objective is not sent to the worker`).
- **Conflicting flags.** `--manifest` conflicts with `--dry-run`, `--detach` and `--max-iterations`. Clap rejects the combination.
- **Explicit state root.** `--state-root` must be on the command line. `LMLOOP_RUST_STATE_ROOT` and the default root do not count, and the root must not overlap the repo.
- **Stricter manifest than `pilot-run`.** The path must be absolute and, once resolved, outside the repo. The file must be a regular file you own with mode 0600 (the same check the TUI applies). A mode 0644 file is refused. `pilot-run` does not require 0600.
- **Repo match.** The manifest's `repo` must resolve to the `--repo` checkout. A manifest naming another repo is refused (`not the repo to run in`). The run uses the resolved `--repo`.
- **Same pilot limits.** One to three tasks, serial, one-shot. A repeated run id is refused (exit 2) and `fixture-resume` refuses the run (exit 8, `HEAD` unchanged). The operator API labels it `serial_pilot`, and success prints the pilot-evidence line.
- **No `--manifest`, no `--dry-run`:** still exit 69, and the message names `--manifest`, `--run-id` and `--state-root`. `run --dry-run` is unchanged.
- **Exit codes** after the checks above are the pilot codes: 0 committed, 2 usage, 7 owned elsewhere, 8 needs reconciliation, 10 task failed, 11 stopped, 12 paused (reported, never resumable).
- **No sandbox, not production.** The same trust model as `pilot-run`: the manifest's launcher and gate run with your privileges and no shell.

### Worker watchdog (opt-in)

Checked against `src/serial.rs` (`Watchdog`, `Dog`, `validate`), `src/protocol.rs` (`Progress`, `tool_event`) and `tools/sse_progress.py`. A watchdog lets a worker run without a fixed wall-clock cap, ending it only when the model goes quiet.

**Manifest.** Add `watchdog` to the `worker` object only. The gate may not have one (`watchdog is for the worker only`).

```json
"worker": {
  "argv": ["/absolute/path/to/opencode-launcher"],
  "watchdog": { "progress_file": "/absolute/private/path/progress", "startup_secs": 600, "idle_secs": 600 }
}
```

- **Hard gate.** `timeout_secs` is optional on the worker only when `watchdog` is present. Without a watchdog, `worker.timeout_secs` is still required, and the gate always needs one. With both, the hard cap still applies and its failure is `worker_rejected`, not `worker_watchdog`. Fields are `progress_file` (absolute), `startup_secs` and `idle_secs`, each 1–3600. Unknown fields such as `tool_secs` are refused.
- **Trust model.** The manifest and the progress file are trusted, operator-owned input. The file must be created fresh by the operator for each run, outside the checkout, in an isolated location the worker cannot write: absolute and normal (no symlink), a regular file you own, mode `0600`, one hard link. The first task of a run refuses a file that already has records (`progress_unsafe`). Rust cannot check that the model can't reach the file; that isolation is the operator's job.
- **Bridge integration.** A trusted writer on the upstream bridge appends the literal record `model\n` to that file after each substantive model delta (text, reasoning or tool-call). `tools/sse_progress.py` is a count-only wrapper for the OpenAI-style SSE relay: it writes at most one record per second, copies no upstream text, ignores keepalives, usage-only chunks, `[DONE]` and empty deltas, and never alters, delays or withholds the relayed bytes. Offline tests: `python3 -m unittest rust/tools/test_sse_progress.py`.
- **Fail closed.** Each poll validates newly appended bytes as whole `model\n` records. A shrink, replacement, extra hard link, any other byte, or growth past 4 MiB trips the watchdog (`progress source failed closed`).
- **Startup and idle.** `startup_secs` runs from worker start to the first model record, and `idle_secs` is the allowance between model records after that. Only model records count as liveness. Worker stdout lines, retries, keepalives and Git changes never do. A trip ends the worker and fails the task as `worker_watchdog` (exit 10, uncommitted work preserved).
- **Completed-only tool reporting.** The worker's CLI reports a tool only after it finishes. Tools are counted from completed `tool_use` events (`tools_completed`, `last_tool`), and `tools_observed` is always `completed_only`. A running tool looks the same as a silent model, so set `idle_secs` longer than the longest tool you expect.
- **Advisory loop warning.** After three consecutive identical completed tool calls (same input and output) with no change to the task's scoped files, the evidence carries a `warning`. It never stops the worker. Scoped files are fingerprinted only for at most 8 literal file scopes and 1 MiB in total. A directory scope, a symlink, a non-regular file or anything over the bound gives "unknown", and no warning is made.
- **Evidence.** `watchdog.json` in the run directory (written at most once a second while watching, and at the end) holds `state` (`watching`, `tripped`, `ended`), model-record counts, last tool and any warning or trip reason. It survives a failed task; the next task replaces it. The operator API, TUI and web console show it.

Limits:

- The advisory warning is not a Git witness. Git remains the only witness of what a task changed.
- There is no in-flight tool detection. A long-running tool cannot be told from a stalled model.
- There is no ship or deployed-proxy claim. Nothing here has been run through a deployed bridge or proxy.
- It measures model activity, not progress: a model that streams for an hour without writing still passes. It cannot tell a useful run from a slow one.

Evidence, reported by the parent session. This doc pass reran none of the tests or runs, but did read the artifacts named below:

| Run | Result |
|---|---|
| Offline scripted pass, real OpenCode against a scripted upstream with the SSE beacon, scratch `/tmp/opencode/lm1x4.sse.m9EZGi/sc/A_pass` (60 s startup and idle limits) | The doc pass read the artifacts: `rc.txt` is `lmloop rc=0`, one commit `lmloop pilot: t1` on top of `base`, a gate `report.json` with `pass: true` at 1440 and 390, and `watchdog.json` `ended` with 5 model records, 4 completed tools and no trip. Scripted, so it says nothing about any model. Sibling scenarios `B_startup` and `C_idle` ended `rc=10`; their trip reasons were not re-read here. |
| Live Museum, detached, 600 s startup and idle, scratch `/tmp/opencode/lm1x4.mus5.THm2By/live` | **Completed and passed.** An earlier read found it still `watching` (107 records, no trip), so no outcome was claimed then. It has since finished: `DONE` exists, `rc.txt` is `lmloop rc=0`, commit `fffece2` (`lmloop pilot: t1`) is on top of `base`, the gate `report.json` is `pass: true` at 1440 and 390, and `watchdog.json` is `ended` with 4,422 model pulses, 20 completed tools and no trip, after 5,213 s. The trusted beacon wrote the records; the watchdog never tripped. It is one run, so it shows the watchdog tolerating a long, busy run, not that it catches a stalled one live (the offline `B_startup` and `C_idle` scenarios ended `rc=10`). |

### Basic TUI

Checked against `src/cli.rs` (`dispatch`) and `src/tui.rs`.

- **Bare `lmloop` on a TTY** is scoped to the exact Git checkout of the current directory. It lists only runs whose manifest repo is that checkout. A linked worktree counts as its own, separate checkout. Outside a repository it lists nothing and does not fall back to the global list. Without a TTY on stdin and stdout it prints help and exits 2.
- **`lmloop tui`** is explicit and global: it shows all runs and cannot launch.
- **Launch (`N`, bare `lmloop` only).** It needs `--state-root` on the command line (the environment variable and default do not count) and a trusted manifest: an absolute path outside the repo, a regular file owned by you with mode 0600 (stricter than `pilot-run`), with one to three tasks (the same limit as `pilot-run`; the review screen still says "one task") and a repo naming this checkout. The manifest is read once. The TUI shows the loaded policy for review, and **Ctrl-Y** launches that exact policy. Any other key cancels, so typed or pasted text cannot start a run. The launch is a one-shot existing pilot run with the same guards as `pilot-run`, and pilot evidence labels apply. A pilot offers only Stop now.
- **No general run.** The TUI starts no objective-driven run. Ordinary `run` stays disabled (exit 69).
- **List cap.** The run list is still capped to the newest 100 runs of the state root, and the checkout filter applies after the cap (`MAX_RUNS` in `src/operator.rs`). Use a dedicated pilot state root.
- **Not tested here.** This doc pass reran no tests and launched no pilot.

### Auth edge, units and CI

Checked against `edge/gateway.py`, `deploy/`, `.gitea/workflows/rust-ci.yml` and `web/app.js`. The source and tests have passed the checks listed under [Verification](#verification). That establishes source readiness only: nothing is installed, enabled or running, and no real issuer, proxy or Android device has been tried.

- **Edge.** `edge/gateway.py` is a loopback-only Python server that sits between a TLS proxy and the Rust backend. It reuses `web/auth.py` (OIDC) and `web/device_auth.py` unchanged and needs `PyJWT` and `requests`. `--check` validates the whole configuration without binding or touching the network. Secrets come from `env:`/`file:` references, never argv.
- **Identity.** The OIDC `sub` must be on the required `LMLOOP_EDGE_OPERATOR_SUBS` allowlist, at the callback and on every request. Optional Android bearer tokens (`LMLOOP_EDGE_DEVICE_TOKENS`) are read-only: `GET /api/config`, `/api/runs` and `/api/runs/lmloop/<id>`, and never a write.
- **Routes.** A closed allowlist; everything else is 404. Open: `/health` (fixed JSON with `auth` "oidc" and `read_only` true, no upstream call), `/login`, `/login/start`, `/oauth/callback`, `/logout`. Session only: `/`, `/app.css`, `/app.js`, `/api/runs/<id>`, log reads (`bytes` up to 65536), `POST …/control` (`pause`, `stop`, `stop_now`) and `POST …/decision` (`approved` or `changes_requested`, note up to 2000 bytes). There is no resume. A write needs the session cookie, the exact public `Origin`, a same-origin `Sec-Fetch-Site`, a matching `X-CSRF-Token` and a JSON body. Approvals stay recorded-only: the edge adds no approval power.
- **Upstream.** Each request is rebuilt from scratch with fixed headers and the Basic credential from the backend's `0600` auth file. Redirects are not followed, and status, content type and size (1 MiB) are checked. A request has a 30 s deadline, and 16 run at once. `/api/config` gains `user`, `csrf`, `auth` and `oidc`, and `read_only` is true for a bearer.
- **Dashboard.** `web/app.js` fetches `/api/config` before each mutation to get the CSRF token, never stores it, hides Resume behind the edge, and shows a sign-in prompt when the session has expired. A Basic 401 keeps its browser credential prompt.
- **Units.** `deploy/` holds `lmloop-rust-web.service`, `lmloop-rust-edge.service`, `edge.env.example` and a README. They are user units that run from an immutable release under `~/lmloop-rust/current` with fresh state, and touch nothing of the Python `lmloop-web` unit. Both bind loopback only. `edge.env.example` holds placeholders, no secret values.
- **CI.** `.gitea/workflows/rust-ci.yml` is configured to run fmt, locked clippy, tests and build, the Node checks, the edge tests against the built backend and the SSE adapter tests on pull requests and pushes to `main` that touch `rust/**`, `*.py` or `web/**`. Its source checks passed locally as listed under [Verification](#verification); hosted-runner execution is not verified.
- **Open blockers.** (1) A real Caddy on another host reaching the edge's loopback needs an authenticated private transport; the deploy README lists options and none is configured. (2) The operator OIDC subject and the Vault references for the secrets are not set up.
- **Name mismatch.** The units, cookie names (`lmloop_preview_session`) and example client id say "preview". Pierce asked for the existing live URL on `main`, with no preview. That wording needs reconciling before any rollout.
- **Python unit.** The old Python unit is removed only after an authenticated Rust cutover, and none has happened.

### Cutover status

**No cutover has been performed.** Python lmloop remains the production path, and nothing here replaces or deploys over it. Pierce wants `main` and the existing live URL, not a preview, and the Python unit removed only after an authenticated Rust cutover. Before any cutover:

- The production Rust `run` path is disabled (exit 69 without `--manifest`). Only the experimental one-shot pilot exists, as `pilot-run` or `run --manifest`. Its small no-skill live run committed, and the sixth Museum live attempt passed under the watchdog (one run). The first five Museum attempts failed (exit 10, no commit; the fourth, with longer caps, ended on an incomplete stream, and the fifth timed out at the 3600 s worker backstop after a late write to `index.html`).
- **Decision (Pierce): OIDC and Android are retained.** Both must be carried over before the Python service is swapped, and Python baseline changes stay additive only. Neither is done in Rust.
- The Rust binary binds loopback only and offers optional Basic auth plus an HTTPS-origin check for a same-host TLS proxy; it does not provide Python's OIDC, device-token, CSRF and read-only modes itself. The edge above supplies these paths in source, but it is unproven against a real issuer, proxy and device. Android's `/health`, `/api/config` and run-view reads have additive aliases (see the web paragraph above). Only a local TLS fixture has been tried, not a deployed proxy. See [`compatibility.md`](compatibility.md).
- There is no legacy state migration: Rust starts from fresh state only and reads no Python run, config or state data.

## Commands and exit codes

Checked against `src/cli.rs` (`EXIT_*` constants and the `after_help` text). Codes 3 and 4 mean different things for `status` and `diagnostic-gate`, so read them per command.

| Command | Code | Meaning |
|---|---|---|
| any | 2 | Usage error. Includes clap's own rejections and `--dry-run` with `--detach`. |
| any | 1 | Other error (I/O, unsafe path, bad state root, unreadable or unsupported snapshot found by `diagnostic-gate`). |
| `run --dry-run` | 0 | Input valid; plan printed. |
| `run --dry-run` | 2 | Empty or oversized objective (over 256 KiB), `--repo` fails the `.git` shape check, or flag conflict. |
| `run` (no `--dry-run`, no `--manifest`) | 69 | Refused: the objective-driven engine is not implemented. |
| `run --manifest --run-id` | 0 / 2 / 7 / 8 / 10 / 11 / 12 | The one-shot pilot (see [Normal CLI](#normal-cli-run---manifest)). Exit 2 also covers a missing `--state-root` flag, a manifest that is not absolute, outside the repo, 0600 and owned by you, a manifest repo that differs from `--repo`, a state root overlapping the repo, an existing run id, and `--dry-run`, `--detach` or `--max-iterations` alongside it. |
| `pilot-run` | 0 / 2 / 7 / 8 / 10 / 11 | As for `fixture-run`. Exit 2 also covers a missing `--state-root` flag, a manifest with more than three tasks, a state root overlapping the repo and an existing run id. |
| `fixture-run` / `fixture-resume` | 0 | Fixture run completed. |
| `fixture-run` / `fixture-resume` | 2 | Invalid manifest, run id or invocation. |
| `fixture-run` / `fixture-resume` | 7 | Another process owns the run. |
| `fixture-run` / `fixture-resume` | 8 | State needs manual reconciliation; no ambiguous state was advanced. Includes an existing Git `index.lock` after the ref update: the landed intent is preserved and the lock is left in place. |
| `fixture-run` / `fixture-resume` | 10 | Task failed; uncommitted work is preserved. |
| `fixture-run` / `fixture-resume` | 11 | Stopped at a task boundary or by STOP-NOW. |
| `fixture-run` / `fixture-resume` | 12 | Paused at a task boundary. |
| `status` | 0 | Snapshot read. |
| `status` | 3 | No such run, or no runs at all. |
| `status` | 4 | The run exists but its snapshot is invalid, unsupported or unsafe. |
| `diagnostic-gate` | 0 | Gate ran and exited 0 (diagnostic only). |
| `diagnostic-gate` | 3 | No argv given: absent gate. Not a pass. |
| `diagnostic-gate` | 4 | Gate exited non-zero (other than 127) or died on a signal. |
| `diagnostic-gate` | 5 | Executable not found, or exit 127. |
| `diagnostic-gate` | 6 | Hit `--timeout-secs` and was torn down. |
| `diagnostic-gate` | 7 | Another process owns the run; nothing was changed. |
| `diagnostic-gate` | 8 | The snapshot says `gate_running` but nobody holds the lease. Manual reconciliation required. Preserves the prior snapshot and controls and starts nothing; the claim attempt may still update the owner PID metadata. |
| `diagnostic-gate` | 9 | SIGTERM, SIGINT or SIGHUP: gate torn down, snapshot says `interrupted`. |

`fixture-run` accepts a manifest with version 1, an absolute repository path, absolute worker and gate argv, and 1–16 task entries. Each task has a simple id, prompt and literal repo-relative scopes. The fixture runner refuses a worker named `opencode` (only `pilot-run` allows it); this guard does not make arbitrary operator commands safe. Worker and gate commands run with the operator's privileges. The worker receives the task prompt as its final argument; no shell is used.

`status` also reports an owner-lost run (`gate_running` with no live lease) as `effective_phase: interrupted` with `needs_reconciliation: true`, and still exits 0.

## Limits you should know about

- **`.git` validation is structural only.** `run --dry-run` accepts a `.git` directory containing a `HEAD` file, or a `gitdir:` pointer file whose target is a directory. It does not run Git and does not prove HEAD resolves. The dry run says so in its output.
- **M1 diagnostic gate has no Git binding.** Its snapshot is not tied to a commit, diff or tree. The separate fixture runner uses Git snapshots, but is not the objective-driven production engine.
- **Git recovery has limits.** The gate export is verified against exact Git blob IDs and executable modes both before and after the gate. A bounded serial fingerprint remains as a supplemental check for directory and mode comparisons. This detects content and mode changes, but is not a race-free sandbox. One shared real-index guard serves the normal and resumed fixture paths. It creates the per-worktree Git `index.lock` exclusively and holds it through revalidation and publication. Under the lock, the index must hold exactly the old or new tree, so independently staged trees are preserved. An existing `index.lock` is never cleared. If one is present after the ref update, `fixture-run` and `fixture-resume` exit 8, keep the landed commit intent and print guidance. After you reconcile the lock, `fixture-resume` retries without a duplicate commit. The guard does not cover concurrent writers that touch only refs, because they never take `index.lock`. If the process dies holding its own lock, that lock needs manual reconciliation. The regenerated index may drop extensions such as a split index, the untracked cache and resolve-undo data. Crash recovery completes only a recorded commit intent whose ref, parent and tree match; other interrupted states require manual reconciliation. `fixture-resume` repairs only a torn event-log tail (see [Fixture-resume journal recovery](#fixture-resume-journal-recovery)); anything else in the journal needs manual reconciliation. Stale controls written as an owner exits can be observed by the next owner.
- **Objective and native gaps remain.** `run` is still dry-run-only (ordinary production run disabled); the objective is not exposed in the operator view. There is no native project support, full CLI/TUI/dashboard parity, PWA, Android client or fixture CI. Web `--auth-file` and `--public-origin` provide optional Basic auth and a proxy-origin check, verified only against a local TLS fixture, not a deployed proxy. The first five Museum live attempts ended with exit 10 and no commit; the small no-skill run and the sixth Museum attempt (one run) committed. OIDC and Android remain unported and are required before the Python service swap. The TUI remains pilot-only. Legacy Python import, shims, migration and rollback rehearsal are out of the acceptance gate by decision, not unfinished work. The operator approval record is a recorded decision only.
- **Diagnostic-gate output capture can be partial.** Each stream keeps 128 KiB. A pipe still open after a 2 s grace period is recorded as `output_incomplete`, and `log_truncated` marks the size cap.
- **Containment is partial, and no sandbox exists.** The gate leads its own process group, which is killed on exit, timeout or signal. A descendant that calls `setsid` leaves that group and is not contained. If the `lmloop` process itself is SIGKILLed, nothing tears the group down; the run then reads as owner-lost (exit 8 on retry). Neither case is claimed to be contained, and neither guarantees that no success is recorded: if the leader exits 0, a diagnostic `passed` can be published even when a descendant escaped or capture was partial. That `passed` is never verified or reusable evidence. The gate gets a stripped environment (`PATH HOME USER LOGNAME LANG LC_ALL LC_CTYPE TZ TMPDIR TERM`), null stdin and the operator's own privileges. It is not sandboxed.
- **The state root is guarded, not proven safe.** `state::resolve_root` refuses `~/.local/state/lmloop`, anything inside a Python lmloop checkout (`lmloop.py` plus `rundir.py`) or a `.lmloop` directory, relative paths and `..`, and ancestors owned by another user or world-writable without the sticky bit. It cannot see bind mounts or hard-link aliases, or another process of yours changing the tree. A custom or isolated root is your responsibility beyond that.
- **State files are private.** Directories must be mode 0700 and files 0600, owned by you. Symlinks are refused.
- **Stale `STOP`/`STOP-NOW` are archived, not deleted.** They move to `stop-archive/`, and only after the lease is won. A losing claim leaves them untouched.
- **argv is not stored.** The snapshot keeps the program name and the argument count only.

## Verification

Reported by the parent session and QA for this worktree. The doc pass did not rerun them. None of them use a live model. Earlier counts (M1, 85, 93, 98, 114) are historical, not current totals.

**Latest parent verification (offline/local):** under `umask 022`, all 118 locked Rust tests passed after the CLI fixtures were fixed to create private state directories explicitly. `cargo fmt --check` and locked Clippy passed. `/usr/bin/python3 edge/test_gateway.py` passed 30 tests, including real-Rust integration; `python3 -m unittest rust/tools/test_sse_progress.py` passed 14; Node passed 10 tests in `web/app.csrf.test.js`, and `node --check rust/web/app.js` passed. These checks establish source readiness only. They do not establish a hosted Gitea CI run, installed units, or a live issuer/proxy/device deployment.

A separate scratch run under `/tmp/opencode/lm1x4-proof/` (parent-reported; the doc pass did not read its logs, which hold fixture secrets) passed 51 fixture checks. It exercised the real Python OIDC and device-token verifier and the Rust backend over loopback fixture TLS, `GET` only. It used no real issuer, no real proxy and no Android app, so it is not deployed-edge or device evidence.

| Check | Result |
|---|---|
| `cargo fmt --check` | pass (parent-verified) |
| `cargo clippy --locked --all-targets -- -D warnings` | pass (parent-verified) |
| `cargo test --locked` (under `umask 022`) | 118 tests passed after explicit private scratch-state directory fix (12 lib, 24 CLI, 15 Git snapshot, 49 serial, 11 TUI, 7 web) |
| `cargo build --locked` | pass |
| `/usr/bin/python3 rust/edge/test_gateway.py` | 30 passed, including real-Rust integration |
| `python3 -m unittest rust/tools/test_sse_progress.py` | 14 passed |
| `node --test rust/web/app.csrf.test.js` | 10 passed |
| `node --check rust/web/app.js` | pass |
| Hosted Gitea workflow / installed units / live issuer-proxy-device | Not verified |
| QA fixture exercise | Real paused → resume → approve flow passed; approval left `HEAD` unchanged. Guarded local API checks passed. |
| Visual review | Desktop and mobile Chromium review; keyboard and reduced-motion checks. See [`web/design/visual-review.md`](web/design/visual-review.md) for limits and scope. |

## Fixture-resume journal recovery

Checked against `src/protocol.rs` (`recover`, `scan`, `archive_tail`) and `src/serial.rs` (`resume`, `journal`). `events.jsonl` is the fixture run's append-only journal.

- **Who and when.** Only `fixture-resume`, and only after it wins the run lease. A losing claimant (exit 7) leaves the journal, controls and run state untouched. Repair runs after the phase check and before any control or engine change.
- **Eligible phases.** `paused`, `idle`, and `committing` where the commit has landed (attached branch ref, parent and tree all match the recorded intent). `complete` returns success without opening the journal. `failed`, `stopped`, `worker` and `gate` (owner lost mid-task), and a `committing` run whose ref is not at the intended commit, are refused before the journal is read.
- **Refusals (exit 8, nothing written).** A missing `events.jsonl` on a run that has state (`journal_missing`; history is never invented). Any complete (newline-terminated) record that is not valid JSON or not exactly the seven-key v1 object (`journal_malformed`), carries another version (`journal_version`) or another run id (`journal_foreign`), or is misnumbered (`journal_sequence`; records must run 1, 2, 3, …). A symlink, foreign or shared file, or hard-linked journal (`journal_unsafe`). Records and sizes beyond the bounds (`journal_too_large`). The refusal message says events, controls and run state were left as found.
- **What counts as torn.** Only the bytes after the last `\n`. A complete-looking JSON record without its newline was never acknowledged, so it is torn too and is not counted in the sequence.
- **Archive before truncate.** The unterminated suffix is copied byte for byte, even if it parses, to `journal-recovery/events-tail-<offset>.<k>` (`<offset>` is the byte length of the complete records). The copy goes to a unique `create_new` temp file (0600), is fsynced, then renamed into place without replacing anything (`renameat2` `RENAME_NOREPLACE`; plain `rename` only where the kernel returns ENOSYS or EINVAL, safe because the lease is held), and the directory is fsynced. Only then is the journal truncated to the last complete record and fsynced. Complete records are never dropped or rewritten. If the file size changed during recovery, it refuses (`journal_malformed`) without truncating.
- **Reuse and collisions.** `k` runs from 0 to 7. An existing archive with identical bytes is reused (re-fsynced), so a retry after an interrupted recovery adds no duplicate. A different file at that name is never touched; the next `k` is tried. A symlink, non-private or hard-linked archive is refused. Eight different tails at one offset give `journal_archive_full`. Temp files from an interrupted copy are left in place, not cleaned.
- **Bounds.** At most 1024 records, 4 MiB per line and 32 MiB per journal. Scanning reads at most 32 MiB plus one byte. An append that would pass a bound is refused with `journal_too_large`.
- **Sequence after repair.** The next record continues from the last complete record's `seq` plus one. `EventLog::open` itself refuses an unrecovered tail (`journal_torn`) rather than append after it.
- **`status` is read-only.** `status`, and the operator API's reads, never repair, archive or truncate; they report state and lease only.
- **Evidence.** Fixture tests in `tests/serial.rs` simulate interruption by writing torn bytes, pre-seeding archives and stale temp files, and re-running resume. They cover exact-byte archive then continued sequence, an unterminated but valid record, torn tail after CAS, archive reuse without overwrite, a losing owner leaving a torn journal as found, a missing journal, and hard-link refusal. There is no power-loss or fault-injection proof: fsync ordering is by construction and review, not tested against a crash.

## Where things are

| Path | Role |
|---|---|
| `src/cli.rs` | Command line, exit codes, dry-run, status, fixture commands |
| `src/state.rs` | State root resolution and guards, run store, M1 diagnostic `Snapshot` schema (1) |
| `src/serial.rs`, `src/git.rs`, `src/protocol.rs` | Fixture task lifecycle, Git snapshots and worker/event protocol |
| `src/operator.rs`, `src/tui.rs`, `src/web.rs` | Shared read/control/decision API and M3 interfaces |
| `src/lease.rs` | `owner.lock` OS lock: `claim`, `peek` |
| `src/gate.rs` | Gate execution, outcomes, signal handling, bounded capture, log files |
| `tools/sse_progress.py`, `tools/test_sse_progress.py` | Count-only SSE progress beacon for the opt-in worker watchdog, and its offline tests |
| `tests/` | CLI, Git snapshot, serial fixture, TUI and web tests |
| `edge/gateway.py`, `edge/test_gateway.py` | OIDC/bearer edge in front of the backend, and its tests |
| `deploy/` | User units, `edge.env.example` and deploy README |
| `web/app.csrf.test.js` | Node tests for CSRF, expired-session and refresh behaviour in `web/app.js` |
| `../.gitea/workflows/rust-ci.yml` | Gitea CI workflow; source checks passed locally, hosted run unverified |

Beads: `lm-1x4` (epic) remains in progress. This checkpoint records fixture M2/M3 evidence, one offline real-OpenCode pilot, one small no-skill live run that committed, five Museum live attempts that failed with exit 10 (three timeouts, a longer-cap run that ended on an incomplete stream, then a 3600 s worker timeout after a late `index.html` write with no gate run), and a sixth that passed under the watchdog (`fffece2`, 5,213 s, 4,422 model pulses, desktop and mobile gate pass). Under the updated acceptance gate (fresh Rust state, no legacy import or migration/rollback rehearsal), the epic's native CLI/TUI/dashboard, project/API/auth (OIDC retained)/PWA/Android (retained)/fixture CI criteria are not met, and the live-model pilot has one passing run, not a repeatable result. `lm-pzi` is closed. `lm-pv8` and `lm-pv8.1` remain in progress and are unaffected. Android/PWA compatibility is tracked separately as `lm-2bk` and `lm-xw1.7`. Since then source has added an OIDC/bearer edge, user units and a CI workflow (see [Auth edge, units and CI](#auth-edge-units-and-ci)); the source checks are recorded above; no live issuer, proxy or device test has been performed.
