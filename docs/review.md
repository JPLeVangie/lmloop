# Review and approval

Status: built (bead `lm-pv8`). Code: `review.py` (selection, parsing),
`Run._review_gate` / `_drift_check` / `_review_once` in `loop.py`,
`service.approval` in `web/service.py`. Tests: `tests/test_review.py`.

## Settings

Three layers, each overriding the last: `~/.config/lmloop/config.toml`,
the project's `.lmloop.toml`, then the run.

```toml
[review]
max_rounds = 2     # 0 = no review (approval is still required)
every      = 5     # drift check every N working iterations; 0 = off
personas   = []    # empty = automatic
[review.models]
design = "llama-swap/Qwen3.8-27B"
```

Per run: `lmloop run ... --review-rounds N --review-every N
--review-personas a,b`, or the "Review (this run)" fields in the dashboard's
New Run form. A run's settings are saved in `run-state.json` and survive
`resume`.

A run that finishes its plan is not done. It is reviewed by model personas,
then waits for the operator. Nothing merges without the operator.

```
working ──plan complete──▶ review ──all approve──▶ awaiting approval ──approve──▶ merged
   ▲                         │                          │
   └──── findings become ◀───┘ changes requested        └──reject──▶ working (with note)
         plan items             (≤ max_review_rounds)                or abandoned
```

## Review iterations

A review is an iteration with role `review`, run through the same harness,
model loading and pause-on-provider-loss logic as working iterations.

The reviewer receives, via the appended system prompt, its persona brief and
the review rules; via the prompt, the objective, `plan.md`, the gate output
and `git diff <base>...<branch>`.

It writes `review/<round>-<persona>.md`:

```
verdict: APPROVED | CHANGES_REQUESTED
- [file:line] finding, stated as the change to make
```

Rules the harness enforces, not the prompt:

- **Read-only.** If the worktree differs after a review iteration, the
  changes are stashed to `review/<round>-<persona>.patch` and the verdict is
  discarded. The reviewer may run tests and the gate; it may not fix things.
- **Missing or unparsable verdict** counts as a failed iteration and is retried
  once, then treated as `CHANGES_REQUESTED` with "reviewer produced no verdict".
- **Findings become plan items.** Each finding is appended to `plan.md` as an
  unchecked item tagged `(review r<round>/<persona>)`, and working resumes.
  Findings are the plan, not a side channel, so the existing loop does the fixes.
- **Bounded.** After `max_review_rounds` (default 2) rounds that still request
  changes, the run goes to awaiting approval anyway, flagged
  "review unresolved", with the last findings shown. The operator decides;
  two local models do not argue all night. A persona that never returned a
  verdict in any round is named in the flag (`...; design never got a
  verdict`): a missing verdict is not a review, so an awaiting-approval run
  shows exactly what was never reviewed.

## Personas

Built in, in `web/skills/review/<name>.md`:

| persona | brief | selected when |
|---|---|---|
| correctness | objective met, tests exist and pass, edge cases, no dead code | always |
| security | trust boundaries, input validation, secrets, injection, subprocess/SQL/path handling, new dependencies | diff touches server/auth/API code, subprocess or shell calls, dependency manifests, or config with credentials |
| design | render-first: builds, screenshots at phone and desktop widths, checks against the repo's design notes | diff touches `*.html`, `*.css`, `*.jsx`, `*.tsx`, `*.vue`, `*.svelte`, `static/`, or `assets/` |
| performance | hot paths, N+1s, unbounded loops or memory, blocking I/O, bundle size | objective or plan mentions speed, latency, memory, performance, or the diff touches files a gate benchmark covers |

Selection is deterministic (paths and keywords), so it can be tested and explained.
The dashboard shows why each persona was chosen. It can be overridden per repo
in `.lmloop.toml`:

(`[review] personas` in any layer; see Settings.)

Personas run one after another in a fixed order: correctness, security,
performance, design. The GPU is single, and cheaper checks should fail first.
Every selected persona runs every round, even after an earlier one requests
changes; all findings join the plan together. Skipping later personas meant
design never ran, because correctness nearly always finds something.

Operator-written personas: any `~/.config/lmloop/personas/<name>.md` is
selectable by name in `.lmloop.toml`. No automatic selection for them.

## Drift checks

Every `every` working iterations (default 5), one `correctness` review runs with
a narrower brief: is the work still aimed at the objective, and is the plan
still right? It never blocks. Findings are appended to the plan the same way,
and the round does not count toward `max_review_rounds`.

## Awaiting approval

A new terminal-but-resumable state. The dashboard's run page shows:

- the diff (existing diff view)
- the latest gate result
- each persona's verdict and findings, plus the drift-check history
- a flag if review was unresolved

Actions:

- **Approve** fast-forward merges into the base branch. It refuses if the base
  moved; rebasing is a working iteration, not a button. Then it closes the
  run's Beads issue and records `approval:approved` in `events.jsonl`.
- **Request changes** takes the operator's text, appends it to the plan as
  `(operator)` items, and resumes the run. This round does not count toward
  `max_review_rounds`.
- **Reject** abandons the run. The branch and worktree are kept. The Beads
  issue is unclaimed and left open, with the operator's reason as a note.

Approval uses the existing CSRF-protected session. There is no bypass flag.
The older "Merge to main" button still exists for runs from before review
and for runs you stopped yourself.

## What changes where

- `loop.py`: `review` role and the state machine above, the read-only check,
  findings merged into `plan.md`, a new `awaiting_approval` stop reason, and
  the issue closed on approve instead of on plan complete.
- `web/skills/review/*.md`: the four persona briefs and a shared rules file.
- `review.py` (new, small): persona selection and verdict parsing. These are
  pure functions, which makes them easy to unit test.
- `web/service.py`, `web/server.py`: `POST /api/runs/<id>/approval`
  `{action, note}`.
- Dashboard: an approval panel on the run page, and an "Awaiting approval"
  filter on the runs list. Notifications reuse the existing run-finished path.

## Not doing

- Parallel personas: one GPU.
- Auto-merge: the operator chose board approval for every run.
- Reviewers that edit code: fixes go through the plan so there is one writer.
- Scores or weights: a verdict is binary, and findings are the payload.
