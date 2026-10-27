<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/assets/logo-light.svg">
  <img alt="lmloop" src="docs/assets/logo-light.svg" width="400">
</picture>

A loop that hands one objective to a local model, works on it for hours in a git
worktree, and commits what it actually did.

```bash
lmloop run "refactor the dashboard stat grid into two rows"
```

It drives [`pi`](https://github.com/badlogic/pi-mono),
[`omp`](https://github.com/can1357/oh-my-pi) (a pi fork with its own browser and
tools), and [`opencode`](https://github.com/opencode-ai/opencode). Tools, skills,
models and prompts belong to the agent; the loop's only jobs are isolation,
iteration, and never throwing work away.

## Documentation

| Document | For |
|---|---|
| [docs/design.md](docs/design.md) | the invariants and why each one exists |
| [docs/failure-modes.md](docs/failure-modes.md) | how runs fail, with the evidence for each |
| [docs/models.md](docs/models.md) | measured local-model behaviour and window budgets |
| [docs/architecture.md](docs/architecture.md) | what each module does |
| [docs/operations.md](docs/operations.md) | running, steering, reviewing, deploying |
| [docs/review.md](docs/review.md) | reviewer personas, drift checks, approval |

`AGENTS.md` is the short version for an agent working on this repo.

## Why this exists

Existing loops are tuned for fast, reliable cloud agents, where a discarded
iteration costs ninety seconds and redoing it is cheaper than reasoning about
it. At ninety minutes an iteration that trade inverts. One run produced 87
minutes of correct, coherent edits and lost every line because the agent's final
message was not parseable JSON. The code was fine; the envelope was not.

So `lmloop` makes three commitments:

**Nothing is ever discarded.** There is no `git reset --hard` in this codebase.
Every iteration that leaves a diff produces a commit, labelled with what
happened — `ok`, `timeout`, `stalled`, `thrashing`, `no-action`, `interrupted`,
`agent-error`. Failed work
becomes a labelled commit you can revert on your own schedule, not a hole in the
history.

**The handoff is a file, not a parsed message.** The agent writes `handoff.md`
with its own write tool. There is no envelope to fail to parse. If it never
writes one, the loop synthesises one from `git diff` and marks the iteration
degraded — never discarded. If the iteration overflowed its context, the loop
harvests the summary the agent wrote for itself on the way out and carries that
forward instead: an agent that ran out of room did write a handoff, just into
pi's event stream rather than to disk.

**Git is the only witness.** Not iteration counts, not the agent's summary, not
tool-call counts. An agent once reported twelve successful iterations across 479
tool calls and zero writes. Another edited files perfectly well through its bash
tool while the write-tool counter read zero. `git diff` against the run's base
commit is the only honest progress signal, and it is what the stop conditions
and the change guard read.

## Install

lmloop is not published on PyPI. Install the dashboard-capable command directly
from the public GitHub mirror:

```bash
pipx install 'lmloop[web] @ git+https://github.com/JPLeVangie/lmloop.git'
lmloop init                    # writes ~/.config/lmloop/config.toml
lmloop doctor                  # says what is still missing
```

Or clone it for development. Running directly keeps the dependency-free core;
an editable `.[web]` install adds OIDC and optional Web Push support:

```bash
git clone https://github.com/JPLeVangie/lmloop.git
cd lmloop
python3 lmloop.py doctor
python3 -m venv .venv
. .venv/bin/activate
python -m pip install -e '.[web]'
```

Optional files, all of which fall back to a working default:

```bash
cp model-budgets.example.json ~/.config/lmloop/model-budgets.json
cp web/deploy/web.env.example ~/.config/lmloop/web.env      # only for the dashboard
```

Nothing in the repo hardcodes an address for your machine: the defaults are
loopback.

Python 3.11+ (for `tomllib`), stdlib only for the core, no build step. The
`web` extra adds PyJWT and requests for OIDC plus pywebpush for optional Web
Push. Without an authentication mode the server refuses to bind anything but
loopback, which is the safe failure rather than an error.

### What else you need

- **[`pi`](https://github.com/badlogic/pi-mono) on `PATH`.** The loop drives the
  pi coding agent from that repository, not the unrelated `earendil-works`
  organization page. Install its current package with:
  `npm install -g --ignore-scripts @earendil-works/pi-coding-agent`. Everything
  about tools, skills and prompts is pi's business.
- **Somewhere to get a model.** Any provider pi can reach works, but the loop is
  built for a local one — `lmloop models` lists what pi can see. If you serve
  local models with [llama-swap](https://github.com/mostlygeek/llama-swap), point
  `[models] llama_swap_url` at it.
- **A model catalogue pi can read.** lmloop loads its tracked Pi catalogue
  from `web/deploy/model-catalog.js` with `~/.config/lmloop/pi-agent` as its
  isolated profile. Other Pi applications keep their own configuration;
  this catalogue registers local llama-swap models only. Local windows must be
  *declared* to pi,
  and declaring them wrong is the single most expensive mistake here: a router
  that advertised 1,000,000 context for a model actually loaded with
  `--ctx-size 65536` killed runs with HTTP 400 mid-iteration. `lmloop models
  --detect` measures the real figure off the running llama-server command line
  and caches it to `~/.config/lmloop/model-context.json`; how that window is
  split between prompt and output lives in
  `~/.config/lmloop/model-budgets.json` — copy
  [`model-budgets.example.json`](model-budgets.example.json) and edit it. See
  [docs/models.md](docs/models.md).
- **Optional, for the dashboard on a network:** PyJWT and requests, and an OIDC
  provider. Without them it binds loopback only.

## Use

```bash
cd ~/git/some-project
lmloop init --project                      # optional ./.lmloop.toml
lmloop run "the objective" --max-iterations 5
lmloop run "the objective" --model llama-swap/local-wide
lmloop models                              # what is loaded, measured, selectable
lmloop models --detect                     # measure the loaded model's real context
```

Stop a run, at the boundary or right now:

```bash
touch <worktree>/.lmloop/runs/<run-id>/STOP      # after this iteration
touch <worktree>/.lmloop/runs/<run-id>/STOP-NOW  # cut this iteration short
```

`STOP` lets the current iteration finish, so the boundary can do its work — gate,
checks, handoff, commit — and only then does the run exit. That is worth waiting
for: the handoff is what makes the hour reusable. `STOP-NOW` is for an iteration
that is visibly wasting its hour; pi is killed where it stands, the partial tree
is still committed, and the iteration is recorded as `interrupted`. Neither
discards anything. `SIGINT` means `STOP-NOW`, because that is what asking for
your terminal back means; a second `SIGINT` exits without committing.

## What a run leaves behind

A worktree at `<repo>/.worktrees/<run-id>/` on branch `lmloop/<run-id>`, which is
**never deleted automatically** — a run that produced nothing still has to be
diagnosable. Inside it:

```
.lmloop/runs/<run-id>/
    prompt.md                the objective, verbatim
    base-commit              what progress is measured against
    notes.md                 per-iteration log
    handoff.md               rewritten each iteration by the agent
    lmloop.log               JSONL event stream
    iteration-<n>.jsonl      raw pi events
    iteration-<n>-prompt.md  exactly what was sent
    gate-<n>.log             gate output
    sessions/                pi transcripts, replayable with `pi --session`
```

Review and merge: a run whose plan completes is reviewed by model personas
(`correctness` always; `security`, `performance`, `design` when the change
calls for them), then waits for you in the dashboard: approve (fast-forward
merge, closes the Beads issue), request changes, or reject. See
[docs/review.md](docs/review.md). By hand:

```bash
git log --oneline <base>..lmloop/<run-id>
git merge lmloop/<run-id>
```

## The dashboard

```bash
lmloop web            # http://127.0.0.1:8082
```

Start a run, watch it, pause it, stop it, and continue a finished one, across
every project under `LMLOOP_WEB_ROOTS` — without an ssh session or a tmux
attach. It is phone-first, installable to a home screen, and shows plan progress
rather than iteration counts, because "4 of 10 steps" is the honest measure of a
long run and "iteration 7 of 20" is not.

Two properties make it small. **Runs are controlled by files**, so pausing is
`touch PAUSE` and the dashboard never owns a run's lifecycle: it can crash, be
restarted, or be replaced mid-run and every control still works. And **the
present is a file too** — `status.json` — so it reads state instead of replaying
an append-only log to its end.

A run that dies is reported as `stale`, never as running. A crashed loop leaves
a `status.json` that still says `working`, so liveness comes from the age of that
file and from whether the log records a completion.

A finished run can be taken or put away from the same page. **Open pull
request** pushes `lmloop/<run-id>` and runs `gh pr create`. **Archive & remove
worktree** copies the run's record out to `~/lmloop-archive/`, *verifies the
copy file by file*, and only then removes the checkout — the branch and every
commit on it stay, and the run keeps its own group in the list, still readable.
Permanent deletion refuses to run on anything that has not been archived first,
so removing a worktree and losing a record are always two separate decisions.
That is what lets the UI offer removal at all without breaking the first
commitment above.

A live run also shows the model working it: which one (not always the configured
one -- planning and thrash retries escalate), at what thinking level, how fast it
is generating, and how much of its context window the current prompt is using.
The last of those is the one that predicts trouble: past about three quarters of
the window, the next tool result is what triggers a compaction, and a compaction
is where a slow run starts thrashing.

Without authentication configured the server binds loopback only. A dashboard
with a launch button does not belong on a network unauthenticated. For a remote
deployment, configure proxy or OIDC authentication, bind behind a TLS reverse
proxy, and use the reverse proxy's `https://` URL. lmloop's internal listener is
always plain HTTP; authentication does not add TLS. Install `.[web]` for PyJWT
and requests (OIDC) plus pywebpush (optional Web Push). The core remains
stdlib-only.

`web/deploy/lmloop-web.service` runs it under systemd.

## Android app

A native Android client lives in [`android/`](android/README.md). It embeds the
dashboard PWA and adds native foreground service "Live Activity" progress
tracking in the status bar/lock screen, plus background polling for finished runs.

Install and update via [Obtainium](https://github.com/ImranR98/Obtainium):

### Gitea

1. Add App URL: `https://git.levangie.dev/Pierce/lmloop`
2. Override Source: Select `Forgejo (Codeberg)`
3. Enter a read-only bot personal access token in the **ForgeJo access token** field. Do not embed credentials in URLs or exported configs.
4. Set **Release tag filter** to `^android-v`.

To switch an existing install from GitHub, edit the app source in Obtainium rather than uninstalling or clearing data. Updates require matching package signatures and a nondecreasing version code.


## Checks

Every iteration, whatever the project configured, the files git says changed are
checked for damage an *edit* does rather than for whether the program is
correct: a file that stops parsing (Python, JSON, TOML, CSS braces, JS), a
conflict marker, or a block of lines pasted twice. When something fails, repair
becomes the next iteration's job and the plan waits.

It is deliberately not a linter — style is the agent's business, and a noisy
check is one that gets ignored. Anything encoding what a *particular* repository
considers correct belongs in that repository's own `[gate] command`.

## Notifications

A run is unattended for hours by design, so the moment it ends is the moment
nobody is watching.

```toml
[notify]
url           = "https://ntfy.example.com"
topic         = "lmloop"
dashboard_url = "https://lmloop.example.com"   # taps through to the run
```

One push when the run stops, never per iteration — a notification every twenty
minutes for ten hours is a channel you learn to ignore. The title carries the
verdict, because that is what a lock screen shows: *"my-app: 9 commits"*,
or *"my-app: nothing committed"* at high priority, which is the case worth
interrupting someone for.

Empty `url` disables it, and it can never fail a run.

## Disk

A run costs about 86 MB, almost all of it pi's raw event stream, and a bytecode
cache that reached 105 MB on one repository. Left alone this fills a disk.

```bash
lmloop prune --dry-run          # what it would do
lmloop prune                    # this repo's finished runs
lmloop prune --roots ~/git --older-than 7
```

It **deletes no record**. Event streams are gzipped, which saves about 97% on a
file that is 88% single-token deltas, and everything reads either form. The only
thing removed is the bytecode cache, which is derived from source that is still
present and records nothing about what the agent did. A run still writing is
skipped.

On one repository: 313 MB → 8.6 MB, with every stream still readable.

This also runs automatically when a run ends — `[prune] after_run`, on by
default. A run is exactly when the space appears and when someone is watching,
which is easier to trust than a cron job quietly rewriting run directories at
3am. It prints one line saying what it freed.

## Configuration

`~/.config/lmloop/config.toml`, overridden by `.lmloop.toml` in the repo. See
`lmloop init` for a commented starting point. The parts that matter:

- `[iteration] timeout_seconds` — a backstop, in hours not minutes. The real
  protection is `stall_seconds`, which fires when the agent stops emitting
  anything. The stall clock does not start until the first event arrives,
  because llama-swap may legitimately spend minutes swapping models first.
- `[iteration] max_compactions` — give up on an iteration that has overflowed
  its context this many times without writing anything. An agent whose window is
  smaller than the codebase can spend the whole iteration reading a dozen files,
  overflowing, and reading them again: observed on a frontend project at six overflows
  in 69 minutes across 81 tool calls, none of them a write. Cutting it off is
  free, because whatever the iteration left behind is committed either way.
- `[worktree] link` — untracked paths symlinked from the repo into each new
  worktree, so the agent gets the environment and not just the source.
  `git worktree add` materialises tracked files only, which leaves a Python
  project without its virtualenv and a Node project without `node_modules`.
  Watched live: an agent spent an hour and 24 tool calls enumerating every
  `python3` on the box looking for one that could import Flask, while the
  project's own `.venv` sat unreachable in the repo above it. The linked names
  are added to the git exclude list, and the prompt names the interpreter — an
  agent that does not know the environment is there goes looking for it.
  `.env` is deliberately not a default: add it per project if the code needs
  it, rather than having the loop hand a model your secrets uninvited.
- `[agent] harness` — which agent does the typing: `pi` (the default, and
  anything pi discovers as an extension, including the npm package called
  oh-my-pi), `omp` ([can1357/oh-my-pi](https://github.com/can1357/oh-my-pi), a
  fork of pi with its own browser, task and LSP tools), or `opencode`.
  `--agent` overrides it for one run, and `lmloop resume --agent pi` rolls a run
  back mid-flight — nothing about a worktree, a plan or a handoff is
  agent-shaped. See `docs/operations.md` for installing omp beside pi, its tool
  allowlists, and how it is pointed at llama-swap and at a browser.
- `[agent] browser_cdp_url` — where omp's browser tool should attach, checked
  once when the run starts so an unreachable tab is one line at the top rather
  than an hour of the agent finding out. Only omp has a browser.
- `[agent] planner_model` — a different model for the iteration that writes the
  plan. Deciding the steps is a whole-repository question that happens once and
  wants the widest window; carrying one out happens every iteration and wants
  throughput. Empty uses `model` for both.
Mistakes in these files are reported rather than ignored. An unknown section or
key, or a value of the wrong shape, names the file, the setting and a likely
correction:

```
lmloop: config problems:
  .lmloop.toml: `[agent] modle` is not a setting; did you mean `model`?
  .lmloop.toml: unknown section `[stopp]`; did you mean `stop`?
  .lmloop.toml: `[iteration] timeout_seconds` expects a whole number, got a string in quotes ('900')
```

Starting a run refuses; reading one that is already going (`list`, `status`)
warns and carries on, because the run in progress never saw the setting you
mistyped.

- `[gate] command` — run after every iteration, **with the worktree as its
  working directory**, so a path that only exists in the main checkout has to be
  absolute or listed under `[worktree] link`. It is also run once before the
  first iteration: a gate that cannot be executed at all (`rc=127`) stops the run
  there rather than recording the same failure every hour, and a gate that
  already fails on the base commit is reported as the repository's failure, in
  the log and in the agent's prompt. `blocks_commit = false` records the result
  in the commit message and the next iteration's prompt but commits regardless,
  which is usually what you want; a gate that could not be run never blocks a
  commit, whatever that setting says.
Before a first run, or after changing anything:

```bash
lmloop doctor
```

```
  ok    python        3.11.9
  ok    git           repository at /home/you/project
  ok    config        /home/you/.config/lmloop/config.toml, .lmloop.toml
  ok    harness       pi at /home/you/.local/bin/pi
  ok    model         llama-swap/Qwen3.8-27B: 106496 context + 24576 output
  ok    model server  http://127.0.0.1:8080: Qwen3.8-27B already loaded
  ok    worktrees     /home/you/project/.worktrees
  ok    gate          make test
  ok    notify        disabled

  ready
```

It exits non-zero on a `FAIL`. Every check it makes is one that otherwise
surfaces several minutes into a run, in a message about something else: a
missing binary raised `FileNotFoundError` from inside the driver loop, a model
nobody set failed at preflight, a gate that cannot execute was discovered by
running it. It also catches the case `preflight` cannot — a model name the
server has never heard of, which `preflight` optimistically reports as a swap
it would attempt.

- `[agent] model` — **required; there is no default.** It shipped as
  `llama-swap/local-fast`, which is one person's model name on one person's
  server, so the default could only fail — and it failed after a worktree, a
  branch and a preflight had already been built. `lmloop models` lists what
  your agent can reach.

- **Secrets are pointed at, not pasted in.** Any value that carries a
  credential — `[notify] token` today — accepts a reference instead:

  ```toml
  [notify]
  token = "env:NTFY_TOKEN"        # from the environment
  token = "file:~/.config/ntfy"   # from a file
  token = "!pass show ntfy"       # from a command's output
  ```

  A literal still works. This matters more than it looks: the config file gets
  copied into a repo and is readable by the agent the loop is driving — an
  agent whose whole job is reading files and which will happily quote one back.
  An unresolvable reference reads as empty rather than being sent as the
  literal string.

- `[iteration] tool_seconds` — cut an iteration short when a single tool call
  has run this long without returning. A different question from
  `stall_seconds`, which asks how long the *agent* may be silent and is
  routinely raised into the hours for a slow model — so it is the wrong clock
  for a subprocess that will never return.

  This is not hypothetical: an agent started a headless Chrome inside its bash
  tool for a frontend objective and the browser never exited. That blocked the
  tool call, which blocked the agent, which went silent; the repository had
  `stall_seconds = 3600`, so the run sat idle for 38 minutes. Generous by
  default (30m), because a real build or test suite is a tool call too and
  killing one of those is worse than waiting. `0` disables it.

  Only agents that announce a call starting and finishing separately can be
  watched this way — pi and omp do; opencode reports a call and its result as
  one event, so it has no in-flight state to offer and the check stays off.

- `[models] local_providers` — which model-id prefixes mean "served by the
  local llama-swap", and so get a real *measured* window instead of whatever
  the agent's catalogue claims. Defaults to what
  `~/.config/lmloop/model-budgets.json` says, and a repo can override it like
  any other setting. An empty list turns the local path off entirely — no
  preflight, no measured window — which is the setting for a machine with no
  local server.

  Point your agents **directly** at llama-swap rather than through a router. A
  router reports what a model advertises, not how the weights were loaded: one
  reported 262144 for a model actually running with `--ctx-size 131072`, and
  the agent compacts against its own number. `docs/operations.md` has the
  provider block for each bundled agent.

- `[env] inherit`, `pass`, `block` — what the agent and the gate see of your
  environment. The default is `inherit = "allowlist"`: enough to run a process
  and a build (a PATH, a HOME, a locale, TLS trust, proxies, the usual
  toolchain variables), plus the harness's own namespace — `PI_*` for pi,
  `OMP_*` as well for omp, `OPENCODE_*` for opencode. Everything else is left
  behind, including every credential in your shell, because an agent runs
  arbitrary commands and commits files and there is no reading under which it
  needs your cloud session token. On top of that, a name that *looks* like a
  credential is dropped even where a prefix rule allowed it, so `NODE_*` can be
  inherited without `NODE_AUTH_TOKEN` going with it.

  If a harness genuinely needs one in the environment rather than in its own
  config file, name it exactly: `pass = ["ANTHROPIC_API_KEY"]`. The exemption
  takes the exact name — a `pass` entry ending in `*` still adds variables, but
  never opts a credential in, because nobody typing `AWS_*` for `AWS_REGION`
  means to include the secret key. `block` wins over everything.
  `inherit = "all"` restores the pre-allowlist behaviour wholesale.

  A run says at the start which credential-shaped variables it is withholding —
  names only, never values — so a harness that needed one fails in the first
  line of the run rather than as an unexplained auth error an hour in.
- `[planning] pre_write_file_limit` and `steps_per_iteration` control work-unit
  size. Defaults remain conservative (three files, one plan step), but a model
  with a measured large context can be allowed broader coherent turns without
  changing lmloop's safety limits.
- `[stop] budget_follows_plan` recomputes the active budget from turns spent,
  remaining steps, and `retry_allowance`.
- `[stop] initial_turns` is the starting budget; `hard_turn_ceiling` is the
  absolute limit. `max_iterations` remains a legacy alias for both. `resume
  --iterations N` explicitly extends the saved ceiling by N turns; it no longer
  gains an implicit fresh config-sized allowance. Note that these two have to
  *differ* for `budget_follows_plan` to buy anything: set equal — which is what
  the legacy alias does, and what the shipped defaults do — the budget is
  pinned and a growing plan cannot reach past it. That is the shape of the run
  that finished 54 of 62 steps.
- `[stop] max_wall_hours` is now a whole-run budget, carried across resumes in
  `run-state.json` rather than restarting with each segment. A long-lived run
  can therefore exceed a limit that no single segment came near: one here spent
  16.5 hours of model time across six segments, none of them longer than seven.
- Terminal `status.json` uses `phase = "completed"` only for a completed plan.
  Other exits use `phase = "stopped"` and carry `stop_reason`; a ceiling exit is
  reported as `turn ceiling hit`.
- A run also stops when every plan step is checked (`plan complete (15/15)`).
  That is the one stop condition here that trusts a self-report, and it is the
  safe direction to trust — the failure is a run that stops early leaving its
  commits behind, which shows as a checked plan and can be continued. The
  dangerous direction, a self-report that keeps a run *alive*, stays guarded by
  the next line.
- `[stop] no_diff_iterations` — stop after N iterations that git says changed
  nothing. This is the guard that catches an agent confidently going nowhere,
  and the one the budget cannot outrun: plan progress deliberately does not
  reset the streak. Only iterations the agent actually ran to the end of count
  — `ok`, `no-action`, `truncated`. A broken stream, a stall, a thrash or an
  operator interrupt is a failure the loop answers elsewhere, and counting it
  here stopped a healthy run once on three iterations that were agent-error,
  interrupted, agent-error: not one completed attempt among them.

## Notes on local models

- Use `llama-swap/<model>`, not a router alias. A router reports model metadata,
  not how the weights were loaded; one advertised 1,000,000 context for a model
  running with `--ctx-size 65536`, and declaring that killed runs with HTTP 400
  mid-iteration.
- `lmloop models --detect` reads the real `--ctx-size` off the running
  llama-server command line and caches it to
  `~/.config/lmloop/model-context.json`. It only measures what is already
  loaded: llama-swap holds one model at a time, so probing an unloaded model
  *causes* the swap it was meant to observe.
- `[agent] tools` trims pi's tool allowlist. This matters more than it looks —
  a trivial prompt with only `read` enabled still costs ~7.5K input tokens of
  system prompt and extension overhead, which is 13% of a 57K declared window
  before any work happens.

## Running it as an attachable screen

A run is a foreground process with a live status line, not a background job:

```
  iteration 2: local-fast already loaded
  iter 2/3  41m18s  17 tools  3.5 tok/s  12043 out  read
```

The bottom line keeps moving so an attached terminal never looks dead. It shows
the tool in flight, or how long the agent has been quiet. A model load is
reported once when it finishes (`model ready in 3m55s`) rather than repeated.

**The status line is always trimmed to one terminal line, and that is a
correctness requirement rather than tidiness.** A line wider than the terminal
wraps; `\r` then returns only to the start of the last visual line, so the clear
leaves the remnant behind and every refresh scrolls a new line. An overlong
status line does not look slightly wrong -- it turns the display into a spam
log. Segments are dropped by priority as the terminal narrows, so a phone
terminal degrades to what matters:

```
 60 |  2/3  41m18s  read  17 tools  3.5 tok/s  12043 out|
 44 |  2/3  41m18s  read  3.5 tok/s  [STOP]|
 32 |  2/3  41m18s  read  [STOP]|
 24 |  2/3  read  [STOP]|
```

`<run-dir>/status.json` carries the same state as a small, atomically-replaced
document, so a phone script, a status bar, or a web wrapper can read what a run
is doing without parsing a megabyte event log:

```bash
lmloop status            # the most recent run
lmloop status --json     # the raw document
```

Controls are files first, keystrokes second — a file works from ssh, a phone, or
another script, and survives the terminal going away:

| key | file | effect |
|---|---|---|
| `p` | `touch <run-dir>/PAUSE` | hold after the current iteration |
| `r` | `rm <run-dir>/PAUSE` | carry on |
| `q` | `touch <run-dir>/STOP` | finish the current iteration, commit, exit |
| `Q` | `touch <run-dir>/STOP-NOW` | cut the current iteration short, commit, exit |

Pausing mid-iteration is deliberately not offered: the model is mid-generation
and there is nothing honest to freeze. The pause lands at the iteration
boundary, where the tree is committed and the handoff is written.

Stopping mid-iteration *is* offered, because the two stops answer different
questions. `q` is "I am done with this run" and waits for the boundary, where an
hour of work becomes a commit and a handoff. `Q` is "this iteration is wasting
its hour" and does not wait. The keys are separate so that neither is a surprise.

### From Paseo

Paseo workspace terminals host this directly, with no workspace registration:

```bash
paseo terminal create --cwd ~/git/some-project --name lmloop --json
paseo terminal send-keys <id> "lmloop run 'the objective' --max-iterations 3" Enter
paseo terminal capture   <id> --json      # what it is doing now
paseo terminal send-keys <id> p           # pause
paseo terminal send-keys <id> r           # resume
paseo terminal send-keys <id> q           # halt after this iteration
```

`capture` without `--json` renders only the visible region, which for a mostly
idle screen looks blank; `--json` returns the lines.

### From pi

`~/.pi/agent/prompts/lmloop.md` gives pi a `/lmloop <objective>` slash command.
It sharpens the objective and starts the run with `--detach`, returning the run
id immediately rather than blocking the session for hours. Use this to kick a
run off from a pi session; use a terminal when you want the screen.
