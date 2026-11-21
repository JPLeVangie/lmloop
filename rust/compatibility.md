# Compatibility inventory: M2/M3 fixture checkpoint

This page compares the Python lmloop reference with the current Rust checkpoint. M2 provides a serial fixture runner; M3 adds a shared operator API, TUI and loopback web console. This is implementation evidence for fixtures, not production parity or rollout approval. Items are force-ranked:

- **P0**: correctness. Getting it wrong loses work or lies about success.
- **P1**: operator core. The daily CLI and dashboard flows.
- **P2**: deferred. Real, but not needed before the engine exists.

**Acceptance gate (updated by Pierce).** Fresh Rust state only: no import of legacy Python run, config or state data or of any other loop's data, no compatibility shim for legacy data, and no legacy migration or rollback rehearsal. The Python source is reference only; the live checkout and service are protected and nothing deploys. A real model pilot needs separate authorization. The small no-skill live run passed; the first five Museum live attempts failed with no commit and the sixth passed once (see [Native pilot run](#native-pilot-run-pilot-run)). **Cutover decision (Pierce):** OIDC and Android are retained and must be carried over before the Python service is swapped; changes to the Python baseline stay additive only. This inventory therefore compares **behavior** to carry forward natively. Rows that mention "import" or "migration" below are historical reference, not work the gate requires. Native CLI, TUI and dashboard features and the project, API, auth, PWA, Android and fixture CI gaps stay open.

"Rust today" is what `rust/src` does now. Anything else is a plan, not a promise. The Python source is read-only reference; nothing in `rust/` imports, reads or writes it. The implementation and verification described here are limited to the isolated fixture checkpoint plus the `pilot-run` evidence under [Native pilot run](#native-pilot-run-pilot-run). No production service was changed. Live results are mixed: a small no-skill run committed, five Museum runs failed (three on timeouts, the longer-cap run on an incomplete stream, the fifth on the worker backstop), and a sixth Museum run passed under the watchdog (see that section).

Sources named below are paths in this worktree (`lmloop.worktrees/rust-rewrite`, which still holds the Python baseline at `14521f7`).

## P0: correctness

| Contract | Python source | Rust today | Plan |
|---|---|---|---|
| Config layering: defaults, then global, then repo, then CLI overrides | `config.py`: `load` (defaults from `config_defaults.DEFAULTS`, `GLOBAL_CONFIG` = `~/.config/lmloop/config.toml`, `PROJECT_CONFIG` = `.lmloop.toml`), then `override_agent`, `override_review`, `override_decision` | Not read; no TOML configuration in this checkpoint. | Resolve once at launch and persist the full result with the run. |
| Legacy stop migration | `config.load`: a legacy `[stop] max_iterations` fills `initial_turns` and `hard_turn_ceiling` unless those are set | None | Out of the acceptance gate: no legacy config is read, so no translation is planned. Native stop policy is defined fresh. |
| Resume restores the persisted policy, not current defaults | `rundir.py` `run-state.json`; `runrecord.py` docstring; `loop.py` `Run.attach` | None | Persist the full resolved native policy and restore it on resume. **Do not promise CLI overrides on resume.** Python lets `resume` take `--agent`, `--model`, `--thinking`; a native resume should refuse or ignore them until that is decided. |
| One owner per run | Python: pid file via `runrecord.holder` (advisory, checks the command line) | `src/lease.rs`: OS lock on `owner.lock`, PID is metadata only. `claim`, `peek`. | Keep. Never let both engines own one run. |
| Stale `STOP`/`STOP-NOW` cleared only by the winning owner | `rundir.py` resume lock around clearing sentinels | `src/cli.rs` `archive_stop`, after `lease::claim` wins; a loser leaves them alone | Keep. Rust archives instead of deleting (invariant 1). |
| Dry run creates nothing; `--detach` conflicts with it | `lmloop.py` `run --dry-run`, `--detach` | `cli.rs` `RunArgs` (`conflicts_with`), `run` | Keep. |
| Gate failure, missing gate and timeout block advancement | `loop.py` gate handling; `tests/test_audit_regressions.py` in `python-audit-fixes` | `src/serial.rs` refuses task advancement unless the fake worker stream ends with a clean stop and the gate passes; diagnostic gates remain unbound | Fixture flow captures a scoped Git tree, checks the gate export and candidate again, then records commit intent and compare-and-swaps the branch ref. This is fixture evidence, not a production engine. |
| Git is the only witness | `docs/design.md` invariant 2 | `src/git.rs` snapshot tree and ref; worker stream only reports its stop | The gate export is verified against exact Git blob IDs and executable modes before and after the gate. A bounded serial fingerprint remains supplemental for directory and mode comparisons. This is not a race-free sandbox. |
| Nothing is discarded | `docs/design.md` invariant 1 | No deletes anywhere; stop files archived | Keep. No reset, clean or worktree removal. |
| Versioned status | `runrecord.py` `SCHEMA_VERSION = 1`; version 0 means unversioned | `state.rs` `SCHEMA_VERSION = 1`, `read_snapshot` rejects other versions (`unsupported_version`) and unknown fields | See run directory below: the Python v1 and Rust v1 documents are **different formats under the same number**. They never share a directory (`v1/runs/` under a separate root). Rust neither reads nor converts the Python document. |

## P1: operator core

### Python CLI flags and deliberate breaks

Flags from `lmloop.py` (`add_parser` / `add_argument`, lines 698 to 778) against `rust/src/cli.rs`:

| Python | Rust checkpoint | Note |
|---|---|---|
| `run OBJECTIVE` (`-` reads stdin), `--dry-run`, `--detach`, `--max-iterations` | Same names. `run --dry-run` works. `--detach` is accepted but conflicts with `--dry-run`. `--repo DIR`, `--manifest FILE` and `--run-id ID` are new. | Non-dry `run` without `--manifest` exits 69. With `--manifest` and `--run-id` it is the one-shot pilot (see [Normal CLI](#normal-cli-run---manifest)); the objective is validated and then not sent. `fixture-run` and `fixture-resume` run only the manifest-driven fixture engine. |
| `--agent`, `--model`, `--tools`, `--thinking`, `--gate`, `--issue`, `--review-*`, `--decision*` | Not present | The fixture runner's worker/gate argv are manifest-only. Planner, reviewer and decision-model inference are not planned for the one-model workflow. |
| `resume`, `list`, `models`, `prune`, `close`, `history`, `attach`, `doctor`, `init` | Not present | Not implemented in the Rust checkpoint. `web` exists as an M3 local fixture/operator console, but is not Python dashboard parity. |
| `status [RUN_ID] [--json]` | Same shape, new document | Different state root, different JSON. Exit codes 0, 3, 4. |
| none | `--state-root` / `LMLOOP_RUST_STATE_ROOT` (global), `diagnostic-gate`, `fixture-run`, `fixture-resume`, `pilot-run`, `run --manifest --run-id`, `tui`, `web` | New fixture/operator commands, not Python features. `fixture-run` refuses a worker executable named `opencode`; `pilot-run` allows it. Commands in an operator-supplied manifest still run with operator privileges. |

Deliberate CLI breaks:

- **No destructive commands.** No reset, clean, delete, worktree removal or force option, in the CLI or later in the TUI. Python's operator-initiated `archive` and `delete` live in `web/workspace.py` and `web/service.py` (`archive_run`, `delete_run`); whether they are ever ported is an operator decision, not a default.
- **Approval does not merge.** Python's dashboard has `merge` (`web/service.py` `merge_local`) beside `approval`. Rust approval records a decision only. Merge and deploy stay separate, explicit actions.
- Exit codes are a documented contract, listed in `cli.rs` `after_help`. Python's are not.

### Run directory, status and events

Python layout (`rundir.py` docstring and `RunDir.__init__`), at `<worktree>/.lmloop/runs/<run-id>/`: `prompt.md`, `base-commit`, `notes.md`, `plan.md`, `handoff.md`, `lmloop.log` (JSONL events), `iteration-<n>.jsonl`, `iteration-<n>-prompt.md`, `gate-<n>.log`, `sessions/iter-<n>.jsonl`, `status.json`, `run-state.json`, `loop.pid`, `resume.lock`, and the sentinels `STOP`, `STOP-NOW`, `PAUSE`.

Rust state lives at `<state-root>/v1/runs/<run-id>/` (not in the worktree). M1 diagnostic files include `status.json`, `owner.lock`, `logs/gate-<nanos>-<pid>.log` and `stop-archive/`; fixture files include `run.json`, `events.jsonl`, `decision.json`, control archives and task/gate logs.

| Item | Python | Rust | Status |
|---|---|---|---|
| `status.json` | v0 (no `schema_version`) or v1 (`runrecord.SCHEMA_VERSION`); `updated_at`; read by `lmloop.py` and `web/runs.py` | M1 diagnostic snapshot v1 (`state.rs` `Snapshot`, `deny_unknown_fields`, `updated_at_unix`, `phase`, `gate`); fixture lifecycle is in `run.json` | Different shapes. By the acceptance gate, no import or shim is planned; neither Python format is read. |
| Events | `lmloop.log` JSONL; event names inherited from the predecessor dashboard; `pid` on each record | M2: append-only `events.jsonl`, versioned envelope, fsynced after state publication | Fixture event stream exists. `fixture-resume` repairs only an unterminated final line, with archive before truncate; every other journal fault is refused. See the contract below. |
| Handoff and plan | `handoff.md` (capped 4000 chars by `_HANDOFF_LIMIT`), `plan.md` | None | P1, with the same bounds. |
| Pause | `PAUSE` sentinel | Fixture runner honours `PAUSE` at task boundaries; the operator API offers it only while a live owner can act | Fixture control, not parity with the Python loop. |
| Location | Inside the worktree under `.lmloop/` (git-excluded) | Separate state root | Deliberate: a distinct root until compatibility is proven. |

### Fixture-resume journal contract

Source: `src/protocol.rs` (`recover`, `scan`, `archive_tail`, `EventLog::open`) and `src/serial.rs` (`resume`, `journal`). Fixture tests: `tests/serial.rs`. Operator-facing summary: [README](README.md#fixture-resume-journal-recovery).

| Aspect | Contract |
|---|---|
| Who repairs | Only `fixture-resume`, after it wins the run lease and before any control or engine change. A losing claimant (exit 7) changes nothing. |
| Eligible phases | `paused`, `idle`, `committing` with the commit landed (attached ref, parent and tree match the intent). `complete` returns success with no journal access. `failed`, `stopped`, `worker`, `gate` and an unlanded `committing` are refused (exit 8) before the journal is read. |
| Refused, nothing written (exit 8) | Missing journal for a run with state (`journal_missing`); any newline-terminated record that is malformed, not the exact seven-key v1 object, of another version, of another run, or out of sequence (`journal_malformed`, `journal_version`, `journal_foreign`, `journal_sequence`); unsafe journal file (`journal_unsafe`); bound overflow (`journal_too_large`). |
| Torn | Only bytes after the last `\n`, even when they parse as a record. They are not counted in the sequence. |
| Archive first | Exact suffix bytes go to `journal-recovery/events-tail-<offset>.<k>` (`<offset>` is the length of the complete records): unique `create_new` 0600 temp, fsync, `RENAME_NOREPLACE` rename (plain `rename` only on ENOSYS/EINVAL, under the held lease), directory fsync. Only then is `events.jsonl` truncated to the last complete record and fsynced. If the size changed meanwhile, it refuses and does not truncate. |
| Reuse | `k` is 0 to 7, at most eight archives per offset. Identical existing bytes are reused, so a retry adds nothing. A different file is never touched (next `k`). Symlinked, non-private or hard-linked archives are refused. Eight different tails give `journal_archive_full`. Leftover temp files are not cleaned. |
| Bounds | 1024 records, 4 MiB per line, 32 MiB per journal; scan reads at most the bound plus one byte; append refuses past a bound. |
| Sequence | Resumes at last complete `seq` plus one. `EventLog::open` refuses an unrecovered tail (`journal_torn`). |
| Status | `status` and operator-API reads are read-only: no repair, archive or truncate. |
| Evidence limit | Interruption is simulated in fixtures (planted torn tails, pre-seeded archives and temps, repeated resume). There is no power-loss or fault-injection proof of the fsync ordering. |

### Native TUI and local web operator surface (M3)

`lmloop tui` and `lmloop web --port 0` read and act through `src/operator.rs`; they share availability checks and state. The TUI refreshes periodically, supports keyboard navigation and controls, and restores terminal mode on exit. Quitting detaches without writing a control; it is refused while a resume is running in that process. On narrow terminals, the list and detail views take turns.

The web server serves its bundled page and JSON API from `src/web.rs`, on `127.0.0.1` only. Routes are `GET /`, `/app.css`, `/app.js`, `/api/runs`, `/api/runs/<id>`, `/api/runs/<id>/logs/<name>` and `POST /api/runs/<id>/control`, `/resume`, `/decision`. Requests require the exact bound `Host`; POST additionally requires its exact `Origin`, JSON content type and, when present, `Sec-Fetch-Site: same-origin`. On the `--public-origin` host only, a `GET` may omit `Origin` (browsers omit it on top-level navigation); a present `Origin` must still match, and POST never may omit it (`src/web.rs` `admit`, `tests/web.rs`). There is no CORS or non-loopback bind option. Optional `--auth-file FILE` enables HTTP Basic auth on every route. The file must be regular, owned by the current user, mode `0600`, at most 1024 bytes, and contain one line `username:password` with non-empty fields (one trailing newline is accepted). Optional `--public-origin https://host[:port]` requires `--auth-file`, accepts only an exact HTTPS origin, and admits requests whose `Host` matches that origin. Use this only behind a same-host TLS-terminating proxy forwarding the expected `Host` and `Origin`; proxy and TLS configuration are operator-managed. A genuine local TLS fixture with Chromium, reported by the parent, returned a 401 challenge and then an authenticated 200 with assets and API; this is not the deployed proxy, and no deployed proxy/TLS setup has been tested. The Rust listener remains bound to `127.0.0.1`; these options do not permit a non-loopback bind. Responses use `no-store`, `nosniff`, frame denial and a strict CSP; request bodies, in-flight requests, list size and log tails are bounded. The remote agentbrowser in the cluster could not reach the local listener, so Chromium review was local; no LAN exposure was added.

**Basic TUI scope and launch.** Bare `lmloop` on a TTY is scoped to the exact canonical Git checkout of the cwd and lists only runs whose manifest repo is that checkout. A linked worktree is a separate checkout. Outside a repository it lists nothing (no global fallback); without a TTY it prints help and exits 2. `lmloop tui` is explicit, lists all runs and cannot launch. In bare mode `N` launches one pilot: it needs an explicit `--state-root` and a trusted manifest (absolute, outside the repo, owner-only regular file with mode 0600, one to three tasks, repo equal to this checkout). The manifest is read once, the TUI shows the loaded policy, and **Ctrl-Y** runs that exact policy; any other key cancels. This is the existing one-shot `pilot-run` path with its guards and labels, not a general run: the TUI starts no objective-driven run, and `run` stays exit 69. The run list is still capped to the newest 100 runs of the state root before the checkout filter (`MAX_RUNS`, `operator.rs`). Source: `src/cli.rs` `dispatch`, `pilot_prepare`; `src/tui.rs`; `src/operator.rs` `list_runs_in`. Not rerun in this doc pass.

**Cutover not performed.** Python lmloop stays the production path. **Decision (Pierce):** OIDC and Android are retained and must be carried over before the Python service swap; Python baseline changes stay additive only. Prerequisites still open: the production Rust `run` path is disabled without `--manifest` and only the experimental one-shot pilot exists (`pilot-run` or `run --manifest`), with a small no-skill live run committed, the first five Museum live attempts failed (the reruns after disabling hidden maintenance agents also hit rc 10 before any first write, the longer-cap run ended on an incomplete stream, and the fifth timed out at the 3600 s worker backstop after a late `index.html` write) and a sixth Museum attempt passed under the watchdog (one run, not a repeatable result); the Rust web console is loopback-only with optional Basic auth and an HTTPS-origin check for a same-host TLS proxy, unlike the Python dashboard's OIDC, device-token, CSRF and read-only behavior; only a local TLS fixture has been tried, not a deployed proxy; and there is no legacy state migration (fresh Rust state only, by the acceptance gate).

The operator API does not expose the objective. Approval and changes-requested records are final decision records only; they do not alter Git or claim production verification. The design visual ledger is [`web/design/visual-review.md`](web/design/visual-review.md). It distinguishes earlier iterations from final and targeted rerender evidence; neither establishes a full visual audit.

### Python web dashboard: endpoint families

Reference: `web/server.py` (`do_GET`, `do_POST`, lines 328 to 537) and `web/service.py`. Python endpoint parity is not implemented. Its loopback-only default remains a reference, not a Rust deployment claim.

| Family | Endpoints | Notes |
|---|---|---|
| Health and sign-in | `GET /health`, `/login`, `/login/start`, `/oauth/callback`, `/logout` | OIDC via `web/auth.py`. Never cached by the service worker. Rust: only `GET /health` exists, unauthenticated but behind the exact Host/Origin guard, with fixed `status`, `auth` ("basic"/"none"), `oidc` false and `read_only` false fields (Android's `HealthResponse` requires them). No sign-in routes. |
| Shell | `GET /`, `/index.html`, `/static/*`, `/sw.js`, `/manifest.json` | Served without auth. |
| Config and models | `GET /api/config`, `/api/models?harness=` | `/api/config` hands the CSRF token to the page. Rust: `GET /api/config` returns only `poll_seconds` 5.0 and `read_only` false (no CSRF token); `/api/models` is not implemented. |
| Projects | `GET`/`POST /api/projects`, `GET /api/projects/<id>/agent`, `/api/projects/<id>/beads` | |
| System prompt | `GET`/`POST /api/system-prompt` | 20,000 character cap. |
| Runs | `GET /api/runs`, `GET /api/runs/<project>/<run>` | Rust: both exist as additive Android aliases; the only project is `lmloop`, any other returns `no_run`. Rows keep the Rust fields and add `route_id`, `project`, `state`, `title` (run id), `paused`, `stopping`. `state`: `failed`/`interrupted`/`stopped`/diagnostic `gate_finished` read `stopped`; `completed` only for a serial run that is `complete` with every task done and no failure, otherwise `unknown`. Iteration, plan, model, agent and commit fields are omitted, not invented. The dashboard also accepts `#lmloop/<runId>`. Covered by a `web.rs` unit test and `tests/web.rs`. |
| Run controls | `POST /api/runs`, `POST /api/runs/<project>/<run>/<action>`: `pause`, `resume`, `stop`, `stop-now`, `continue`, `archive`, `delete`, `merge`, `approval` (`approve`, `reject`, request changes) | Action routing in `server.py` `control` and `service.control`. |
| Preview | `GET`/`POST /api/runs/<project>/<run>/preview[/<action>]` | |
| Push | `POST /api/push/subscribe`, `/api/push/unsubscribe` | Allowed in read-only mode. |

### API auth and mutation

- Modes in `web/auth.py`: `none`, `proxy`, `oidc` (explicit via `LMLOOP_WEB_AUTH_MODE`, else inferred). A network bind without an identity boundary is refused in every mode. `proxy` needs `LMLOOP_WEB_TRUSTED_PROXIES`.
- `POST` requires authentication, honours `LMLOOP_WEB_READ_ONLY` (403, except `/api/push/*`), requires same origin in `proxy` mode, and requires an `X-CSRF-Token` matching the session in interactive (OIDC) mode.
- **Device-token `GET` only.** `web/device_auth.py` matches `Authorization: Bearer <token>` from `LMLOOP_WEB_DEVICE_TOKENS` (`label=reference`). `server.py` `device_session` is called from `do_GET` for `/api/*` and must never be called from `do_POST`. A device token can read but never mutate a run. The Rust API must keep that split.
- **Rust status.** API `Basic` auth is unchanged and covers every route except `GET /health`. Bearer, device-token and OIDC verification are not in the Rust binary; the gateway provides those paths.
- **Gateway.** `edge/gateway.py` reuses `web/auth.py` and `web/device_auth.py` unchanged, binds loopback only and forwards a closed route allowlist to the Rust backend with a rebuilt request and the backend's Basic credential. It checks an operator `sub` allowlist (`LMLOOP_EDGE_OPERATOR_SUBS`) at the callback and on every request. Bearer tokens are `GET` only and limited to `/api/config`, `/api/runs` and `/api/runs/lmloop/<id>`; a bearer on a write is 401. Writes (`control`: pause, stop, stop_now; `decision`: approved, changes_requested) need the session cookie, the exact public `Origin`, a matching `X-CSRF-Token` and a JSON body. Resume is not offered. Its `/health` is its own fixed answer (`auth` "oidc", `read_only` true), not the backend's. Approvals remain recorded-only. Python parity gaps remain: no project, push, preview, system-prompt or models routes, and no archive, delete, merge or continue controls. See [`README.md`](README.md#auth-edge-units-and-ci).
- **Deployment evidence.** Parent verification passed 30 gateway tests, including real-Rust integration, plus the Android read aliases and guarded writes via fixtures. The fixtures do not prove a real issuer, deployed proxy or Android-device flow. A remote Caddy still needs authenticated private transport to the edge's loopback. The Vault secret references and operator OIDC subject are also outstanding. Unit packaging has not been installed or started; hosted Gitea CI has not been run. The source checks below establish source readiness only, not live deployment.

## P2: deferred

### PWA and Android

- **Service worker** (`web/static/sw.js`, cache `lmloop-shell-v40` in this tree. The dirty Python reference at `lmloop.worktrees/python-audit-fixes` owns the later `lmloop-shell-v41` bump (`web/static/sw.js` line 15); it is uncommitted there, so this tree does not carry it). Caches the shell only: `/`, `/static/app.js`, `/static/style.css`, `/static/icon-192.png`, `/manifest.json`, `/static/offline.html`. Excluded and never cached: `/api/*`, `/login*`, `/oauth*`, `/health`, non-GET and cross-origin requests. Handles `push` and `notificationclick`. Bump `SHELL` whenever the shell changes.
- **Android WebView CSS.** On at least one real WebView (Pixel 9 Pro XL, Chrome/151), bare `vh` and `dvh` resolve to `0px` on `max-height`, even though `CSS.supports` says true. `app.js` sets `--vh1` from `window.innerHeight`, and `style.css` uses `calc(var(--vh1) * N)`. A Rust-served dashboard must keep that, and a desktop-browser screenshot does not verify it.
- **Decision (Pierce): OIDC and Android are retained.** Both must be carried over before the Python service is swapped, and Python baseline changes stay additive only. This page lists them as open, not as dropped or done.
- **Android app** (`android/`): native shell (`NativeShellBridge.kt`), server URL store (`ServerConfigStore.kt`) and API client (`LmloopApiClient.kt`). It is a consumer of the endpoints above. Android and PWA parity is tracked separately as `lm-2bk` and `lm-xw1.7`, and this rewrite does not close either.
- **Passkey gap.** A search of this tree for `passkey`, `WebAuthn` and `navigator.credentials` found nothing, and the sign-in paths seen are OIDC and device tokens. That absence does not prove only those paths exist, and it does not support any claim of Android build or passkey parity.
- **Build gap.** M1 did not build or run the Android app, the web service or the PWA, so it cannot claim Android build or passkey parity. Android CI history in the vault is not evidence about this rewrite.

### Not planned to port as-is

`pi` and `omp` harnesses, the decision model, the planner and reviewer inference paths, and the `models` catalogue probing. The workload is one worker on one local model.

## Native pilot run (`pilot-run`)

Source: `src/cli.rs` (`pilot_run`, `pilot_finish`), `src/serial.rs` (`load_pilot_manifest`, `start_pilot`, `begin`, `resume`), `src/operator.rs` (`PILOT_EVIDENCE`). Tests: the `pilot_*` and `a_pilot_*` cases in `tests/serial.rs`.

| Aspect | Contract |
|---|---|
| Scope | One to three tasks per run, serially in manifest order, through the same engine as `fixture-run`: scoped Git capture, exact-tree operator gate, guarded commit for each task. Not the objective-driven engine; `run` without `--manifest` is still disabled (exit 69). |
| `run --manifest` | Source: `src/cli.rs` (`RunArgs`, `run`, `manifest_prepare`); tests: `run_with_a_private_manifest_*` and `run_refuses_unsafe_manifest_use_*` in `tests/serial.rs`. `run OBJECTIVE --manifest FILE --run-id ID [--repo DIR]` calls the same pilot path (`launch_pilot`, `serial::start_pilot`). The objective is checked (non-empty, at most 256 KiB, `--repo` passes the structural `.git` check) and then dropped; stderr says so. `--manifest` requires `--run-id` and conflicts with `--dry-run`, `--detach` and `--max-iterations`. Stricter than `pilot-run`: the manifest path must be absolute and resolve outside the repo, and the file must be regular, owned by you and mode 0600 (the TUI check). Its `repo` must resolve to `--repo`, whose resolved path the run uses. `--state-root` must be explicit and not overlap the repo. A repeated run id is exit 2; `fixture-resume` exits 8. Exit codes are the pilot codes (0, 2, 7, 8, 10, 11, 12). One-shot, no sandbox, not production. Not rerun in this doc pass. |
| State root | `--state-root` must be given explicitly (the environment variable and default do not count) and must not overlap the manifest repo. Both violations are exit 2. |
| Launcher and gate | Trusted manifest: absolute argv, no shell, operator privileges. Only `pilot-run` may name a worker `opencode`; `fixture-run` refuses it. Policy comes from the CLI command, never from the manifest. |
| Task order and sessions | Tasks run serially in manifest order, each with a distinct session id. Fake-worker tests cover three-task order, distinct sessions, and stopping after task failure. |
| Resume | None. The run is persisted as `pilot`; `fixture-resume` exits 8 with the worktree preserved. A repeated `pilot-run` with the same id exits 2. Pilot messages never point at `fixture-resume`. |
| Operator API | Kind `serial_pilot`, evidence `serial_pilot_not_production_verification`. Only Stop now is offered as a control. Approval is a decision record, no Git change. |
| Sandbox | None in Rust beyond the state-root guards. Any sandbox around the worker is external and the operator's job. |
| Worker watchdog | Opt-in `worker.watchdog` (see below). Worker only; the gate always needs `timeout_secs`. |

### Worker watchdog contract

Source: `src/serial.rs` (`Watchdog`, `Dog`, `validate`, `task`), `src/protocol.rs` (`Progress`, `tool_event`), `src/gate.rs` (`run_monitored`), `tools/sse_progress.py`. Tests: `watchdog_*` and related cases in `tests/serial.rs`, `tool_events_are_completed_only_and_bounded` and the progress test in `src/protocol.rs`, `tools/test_sse_progress.py`. Operator summary: [README](README.md#worker-watchdog-opt-in). No Python equivalent: the Python loop's stall clock is a separate mechanism.

| Aspect | Contract |
|---|---|
| Manifest | `worker.watchdog = { progress_file, startup_secs, idle_secs }`, worker only (a gate watchdog is refused). `deny_unknown_fields`, so `tool_secs` is refused. Seconds are 1 to 3600; `progress_file` is absolute. |
| Hard gate | `worker.timeout_secs` becomes optional only when a watchdog is present; otherwise it is required. The gate's `timeout_secs` is always required. A present hard cap still applies, and its failure reads `worker_rejected` (TimedOut), not `worker_watchdog`. STOP-NOW still works. |
| Trust | Trusted, isolated, operator-owned progress file, created fresh per run: absolute and normal (no symlink), outside the checkout, regular, owned by the operator, mode 0600, one link. The first task refuses a file that already holds records (`progress_unsafe`). Isolation from the model is not checkable in Rust. |
| Bridge wire | A trusted bridge appends the literal record `model\n` per substantive model delta. `tools/sse_progress.py` is a count-only wrapper (at most one record per second, no upstream text, bytes relayed unchanged). Any other byte, a shrink, replacement, extra hard link or growth past 4 MiB fails closed (`progress_invalid`, tripping the watchdog). Records present at open are the baseline. |
| Startup / idle | `startup_secs` from worker start to the first model record; then `idle_secs` between records. Only model records are liveness: stdout, retries, keepalives and Git changes are not. A trip kills the worker and fails the task as `worker_watchdog` (exit 10, work preserved). |
| Tool reporting | Completed tools only (`tools_observed: completed_only`): the CLI reports a tool when it finishes. A running tool is indistinguishable from a silent model, so `idle_secs` must exceed the longest tool. |
| Advisory loop warning | Three consecutive identical completed tool calls (same input and output) over unchanged scoped files set `warning`. Never stops the worker. Fingerprint only for at most 8 literal file scopes and 1 MiB; directory scopes, symlinks, non-regular files or oversize give "unknown" and no warning. A same-inode truncate-and-rewrite that is longer by the next poll looks like growth (`shortcut` noted in `protocol.rs`); the writer is trusted. |
| Evidence | `watchdog.json` (schema 1: state `watching`/`tripped`/`ended`, record counts, last tool, warning, trip), written at most once a second and at the end, kept after a failure, replaced by the next task. Shown by `status`, the operator API, the TUI and the web console. |
| Not provided | Not a Git witness (Git still is the only witness). No in-flight tool detection. No ship or deployed-proxy claim. It measures model activity, not useful progress. |

Evidence, reported by the parent and not rerun in this doc pass:

- **Offline, real OpenCode v2.2.7, scripted upstream.** The worker loaded Impeccable and wrote a site. It passed the exact-tree Chromium gate and was committed in a scratch repo. This exercises the launcher, gate and commit path with a real OpenCode binary and no model.
- **Live, no-skill, small serial run** (scratch `/tmp/opencode/lm1x4.live.0Y2opk/live`; parent ran `verify.sh acc` and `verify.sh live` independently). Three exact ordered commits, each in a fresh session; overlapping requests were rejected. Small run: no claim of strictly zero auxiliary inference.
- **Live, user-authorized: Museum of Lost Umbrellas, `cliproxyapi/agent-local`, first attempt.** The worker loaded the skill, then timed out at the 540 s worker limit. The last of five pinned stream requests hit the 240 s bridge deadline. Exit 10: no site edits, gate, screenshot or commit. The evidence does not show a model-specific root cause. Artifacts: `/tmp/opencode/lmloop-live.M2z4qg`.
- **Live Museum reruns** (scratch `/tmp/opencode/lm1x4.mus.th4sLL`, `/tmp/opencode/lm1x4.mus2.dpaiw0`). Hidden maintenance agents disabled plus a local filter; upstream saw zero auxiliary requests. A scripted pass met the original desktop and mobile gate. Both live runs ended rc 10 on timeout after full skill and reference loads, before a first write or gate. The second asked for the first write immediately and was no better. No cutover.
- **Live Museum, longer approved run** (scratch `/tmp/opencode/lm1x4.mus3.XSkZhD/live`; same prompt, model, full skill and gate; 900 s request cap, 1500 s worker cap). Ended in 935 s with rc 10 and no completed writes. Request 3 streamed 3,045,520 bytes before its deadline. The worker recorded an incomplete write (`stream ended without finish_reason`), and the local latch stopped an upstream retry. No gate, no commit; base HEAD `600324a` clean. The offline latch tests and scripted gate pass; that is not live evidence. No cutover.
- **Live Museum, fifth attempt** (scratch `/tmp/opencode/lm1x4.mus4.CKDUcQ/live`; one run, same prompt and model, same 120 s gate hash). The 15-minute absolute bridge deadline was disabled with idle-secs 300; the 3600 s worker backstop is unchanged in Rust. The run survived an agent server restart. Two main upstream requests succeeded, taking 1424 s and 1679 s. `index.html` was written at about t+3156 s (~52m36s), 22,499 bytes, the only file changed; `style.css` stayed unchanged and empty. The worker timed out at 3601 s (rc 10); the live gate never ran and there were 0 commits. Base `3c6e81c` keeps the dirty `index.html`, preserved. A scripted rehearsal gate passed, separately and not as live evidence. No cutover.

- **Watchdog, offline scripted pass** (scratch `/tmp/opencode/lm1x4.sse.m9EZGi/sc/A_pass`, 60 s startup and idle). The doc pass read the artifacts: `lmloop rc=0`, commit `lmloop pilot: t1` over `base`, gate `report.json` `pass: true` at 1440 and 390, `watchdog.json` `ended` with 5 model records, 4 completed tools and no trip. Scripted; no model.
- **Live Museum, sixth attempt, passed** (scratch `/tmp/opencode/lm1x4.mus5.THm2By/live`, run `r1`, watchdog 600 s startup and idle, no `timeout_secs`, with the SSE beacon). The doc pass read the artifacts. An earlier read had found it still `watching` with no result; it has since finished. `rc.txt` is `lmloop rc=0`; `run.json` is `complete`; the single commit `fffece2` (`lmloop pilot: t1`) sits on `base` (`f587da2`) and changes `index.html` and `style.css` (2,113 insertions, 1 deletion); `git status` was empty; the gate `report.json` is `pass: true` with no failed checks at 1440 and 390. Elapsed 5,213 s (`start.ts` to `end.ts`). `watchdog.json`: `ended`, 4,422 model pulses, 20 completed tools (last `edit`), no warning or trip. The parent reports the desktop/mobile gate was unchanged; the doc pass did not compare gate hashes. The run was wrapped in an outer bubblewrap sandbox made by the operator's `run.sh`, not by Rust. One run: repeatability, zero auxiliary inference and the cause of the earlier failures are all unestablished. No cutover.

Net: ordered, fresh-session native commits are shown live for the small no-skill run, and one skill-heavy Museum run has passed the live gate and committed under the watchdog. This is not repeatability, production readiness, Python parity or general proof of zero auxiliary inference.

## OpenCode compatibility

Observed from `opencode run --help` (v2.2.7), reported by the parent: `--format json`, `--standalone`, `--archived`, `--agent`, `--model provider/model#variant`, `--session`/`-s`, `--continue`, and an auto-approval flag. **Auto-approval (`--auto`) is prohibited** for this project.

The Python adapter (`harness_opencode.py` `argv`) builds `run --format json`, then `--standalone --archived --agent lmloop` when a tool allowlist is set, then `--model <model>[#<variant>]`. It denies every permission by default and allows the named tools through `OPENCODE_CONFIG_CONTENT`, and sets `OPENCODE_DISABLE_PROJECT_CONFIG=1`.

The fixture tests use a fake worker. `fake-opencode` is a test stand-in; it does not establish behavior against a production OpenCode installation, catalogue, plugins or auxiliary inference. The offline pilot above ran a real OpenCode v2.2.7 against a scripted upstream, which shows launcher compatibility but no model behavior. The Python adapter's `--standalone` and permission behavior has not been exercised by Rust fixtures.

## Python reference status

The Python numbers in earlier M1 handoffs are historical and are not Rust checkpoint evidence. No Python suite or live service was run for this documentation pass; the production checkout and service were not touched. The acceptance gate does not call for Python differential tests.

## Fixture checkpoint and verification

| Fixture / check | State | Evidence scope |
|---|---|---|
| `tests/cli.rs`, `tests/git_snapshot.rs`, `tests/serial.rs`, `tests/tui.rs`, `tests/web.rs` | Present; parent verified 118 offline locked Rust tests passing under `umask 022` after the fixture mode fix (12 lib, 24 CLI, 15 Git snapshot, 49 serial, 11 TUI, 7 web) | CLI/state safety, Git snapshot and recovery, serial fixtures, TUI and local API behavior. Fake-worker pilot tests cover three-task order, distinct sessions and task-failure stopping. |
| `tests/fixtures/fake-opencode`, `gate-no-fail`, `gate-127` | Present | Deterministic worker and gates; no live model or network required |
| M2 fixture run | Parent QA verified | A real paused → resume → approve sequence; approval left `HEAD` unchanged. |
| Real index guard | Covered by `tests/git_snapshot.rs` and `tests/serial.rs` | A pre-existing `index.lock` is refused and left in place on both the normal (`fixture-run`) and resumed (`fixture-resume`) paths. Both exit 8, keep the landed commit intent and print guidance; retry after reconciliation adds no duplicate commit. Linked worktrees use their own index and lock. |
| Local API guard | Parent QA verified | Guarded loopback API behavior; no non-loopback access. |
| Design ledger | Targeted desktop/mobile Chromium review | Keyboard and reduced-motion checks were exercised. Earlier screenshots are design iteration history, not final evidence; the final scope and known gaps are recorded in `web/design/visual-review.md`. No cluster browser reached the loopback server. |
| Rust gates | Parent-verified pass | Under `umask 022`, `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo build --locked` and `cargo test --locked` passed; all 118 offline tests passed after the CLI fixtures began creating private state directories explicitly (12 lib, 24 CLI, 15 Git snapshot, 49 serial, 11 TUI, 7 web). Earlier 85-, 93-, 98-, 114- and pre-fix 118-test reports are historical. The documentation pass did not rerun anything. |
| Auth edge tests | Parent-verified pass | `/usr/bin/python3 rust/edge/test_gateway.py`: 30 passed, including real-Rust integration. `node --test rust/web/app.csrf.test.js`: 10 passed; `node --check rust/web/app.js`: pass. |
| SSE adapter tests | Parent-verified pass | `python3 -m unittest rust/tools/test_sse_progress.py`: 14 passed. |
| Gitea CI | Source checks passed; hosted workflow unverified | `.gitea/workflows/rust-ci.yml` pins Rust 1.100.0 and runs fmt, locked clippy, tests, build, Node checks, edge tests and SSE adapter tests. Hosted-runner execution has not been verified. |
| Units and deploy | Packaged; install and live operation unverified | `deploy/` has two user units, `edge.env.example` and a README. Units have not been installed, enabled or started; `systemd-analyze verify` is not reported as run. Open: authenticated private transport from a remote Caddy to the edge's loopback, Vault references and operator OIDC subject. Pierce wants `main` and the existing live URL, with no preview; the "preview" names in the units, cookies and client id still need reconciling. |
| Legacy import, shims, migration/rollback rehearsal | Out of the acceptance gate | Fresh Rust state only; not required and not attempted. |
| Native project, PWA, Android, fixture CI and the full CLI/TUI/dashboard feature set | Not done (source and tests are present; deployment and hosted CI remain unverified; see the rows above) | Web has optional `--auth-file` Basic auth and `--public-origin` for a same-host TLS proxy, but only a local TLS fixture was tried; no deployed proxy/TLS setup or authenticated deployment is claimed. The TUI remains pilot-only; no parity or production validation is claimed. |
| Offline real-OpenCode pilot | Parent reports pass | Real OpenCode v2.2.7, scripted upstream, scratch repo; launcher, gate and commit path only. |
| Live model pilot | Small no-skill run passed; Museum: five failed, sixth passed | No-skill: three ordered commits in fresh sessions. Museum (`cliproxyapi/agent-local`): first attempt and two reruns timed out (exit 10 / rc 10, no commit; first artifacts `/tmp/opencode/lmloop-live.M2z4qg`); a fourth, longer-cap run (900 s request / 1500 s worker) ended rc 10 in 935 s on an incomplete stream, no completed writes; a fifth run (idle-secs 300, 3600 s worker backstop) wrote `index.html` only at ~52m36s, then hit rc 10 at 3601 s with no live gate and no commit. The sixth run (watchdog 600 s startup and idle, no worker cap) passed: rc 0, commit `fffece2`, gate pass at 1440 and 390, 5,213 s, 4,422 model pulses. One passing run only; no repeatability and no root cause for the earlier failures. |
| Retained before the Python swap | OIDC and read-only Android: edge drafted, unproven (decision) | Pierce decided OIDC and Android are retained and must be carried over before the Python service swap; Python baseline changes stay additive only. The gateway source and Rust read aliases implement the carry-over paths; neither has been exercised against a real issuer, deployed proxy or Android device. The old Python unit is removed only after an authenticated Rust cutover, which has not happened. |

M2 recovery refuses ambiguous states without advancing them. In the specific `committing` recovery path, it completes a commit only when the recorded ref, parent and tree match.

The shared real-index guard serves the normal and resumed paths. It creates the per-worktree Git `index.lock` exclusively and holds it through revalidation and publication. Under the lock the index must hold exactly the old or new tree, so independently staged trees are preserved. It never clears an existing lock. With one present after the ref update, both paths exit 8 with the landed intent preserved and guidance printed, and a retry creates no duplicate commit. Remaining gaps:

- Writers that change only refs never take `index.lock`, so concurrent ref-only writers are not covered.
- A lock left by an interrupted run of its own needs manual reconciliation.
- The regenerated index may drop extensions (split index, untracked cache, resolve-undo).
- Skip-worktree and assume-unchanged entries are refused rather than merged.

Crash refusal and controls left stale across owner exit also remain limitations, as do journal faults other than an unterminated tail (see the journal contract above); inspect the run and repository state before manual recovery. Exact Git blob-ID and executable-mode checks verify the gate export before and after execution; a bounded serial fingerprint supplements directory and mode comparisons. This is not a race-free sandbox.
