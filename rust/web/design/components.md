# Component and state contract

- **Surface:** lmloop operator console
- **Design-language revision:** rev 1
- **Implemented paths:** `rust/web/index.html`, `rust/web/app.css`, `rust/web/app.js`

## Component inventory

| Component | Purpose | Anatomy | Variants | Responsive change |
|---|---|---|---|---|
| Sync indicator `#sync` | tell the operator whether the data is fresh | dot, message, time | loading, live, offline (stale since…) | wraps as one nowrap pair; brand suffix hidden under 520px |
| Runs list `#runs` | pick a run | link row: id, phase badge, task count, lease, pending controls, failure (2-line clamp) | selected (`aria-current`, flare inset bar), owner-lost badge | left pane ≥960px, own view below |
| Masthead | identity and progress | meta strip, run id h2, phase and pending badges, progress, failure/lost/warning alerts | per phase badge | h2 scales with viewport |
| Controls matrix `#acts` | steer the run | button plus the server `reason` text, `aria-describedby` | enabled, enabled-risk (Stop now), ghost (Request changes), `aria-disabled` (dashed), busy | 3×2 from 720px; one per row below |
| Tasks | per-task state | index, pip, id, state badge, commit, "Open gate log" | committed, running (pulse), failed, pending | single column; two-column with evidence ≥1180px |
| Facts (`dl`) | evidence and decision | hairline label/value rows | dim for absent data | label column shrinks |
| Logs | bounded tail of gate/worker logs | chips (`aria-pressed`), tail-size select, reload, info line, `pre` | loading, error, truncated note, empty | chips wrap, max-height with scroll |
| Events terminal | last 50 events, newest first | seq, time, kind (+task), JSON data | warn tint for fail/error/refus/stop kinds | grid collapses under 520px |
| Confirm dialog `#dlg` | explicit confirmation | title, run id, server reason plus short warning, optional note, optional ack checkbox, error line, Cancel/Confirm | pause/stop/resume (plain), stop now / approve (ack), request changes (ack plus required note) | full width under 420px buttons |
| State box `.state` | loading / empty / error | text, optional Retry | loading (spinner), empty (dashed), error (flare bar) | – |

## State matrix

| Component | Default | Hover | Focus-visible | Active | Disabled | Loading | Error / empty |
|---|---|---|---|---|---|---|---|
| Action button | ink fill | flare-ink fill, lift 2px, hard shadow | 3px blue outline, 2px offset | no lift | `aria-disabled=true`: dashed paper-2, stays focusable, reason shown; click ignored | busy label ("Resuming…", "Sending…") plus `aria-busy` and banner | flash `bad` with kind and message |
| Run row | transparent | card tint | blue outline | – | – | list state box | empty state box; list error with Retry |
| Chip | card, rule border | ink border | blue outline | – | – | info line "Loading…" | info line in flare-ink bold |
| Dialog confirm | ink fill | as button | blue outline | – | dashed until note/ack satisfied; also while sending | "Sending…" | inline `role=alert` line |
| Sync | grey dot | – | – | – | – | "Connecting…" | flare "Offline · retrying, stale since HH:MM:SS" |

## Contracts

- **Semantic HTML and accessible name:** native `<button>`, `<dialog>` (`showModal`), `<progress>` with `aria-labelledby`, `<dl>` for facts, `<ol>` for tasks and events, landmarks `header`, `aside`, `main`. Each control has `aria-describedby` pointing at its reason. Status regions use `role=status`, errors use `role=alert`.
- **Keyboard operation and focus order:** skip link first; header, runs, detail in DOM order. Dialog opens with focus on the note, else the checkbox, else Confirm; Escape cancels (blocked while a send is in flight); focus returns to the invoking button, or to the run heading if it was rebuilt. A poll rebuild restores focus by `data-fk`.
- **Touch target and pointer behavior:** buttons min 44px; chips, selects and small buttons 36px; link-buttons 32px (smallest; acceptable only as a secondary control).
- **Content overflow and long-label behavior:** `overflow-wrap: anywhere` on ids, failures, log names and event JSON; runs list failure text clamps to 2 lines.
- **Contrast and non-color cues:** every badge has a text label; unavailable is dashed plus "Unavailable:" prefix; selected run has a bar plus `aria-current`.
- **Reduced-motion behavior:** all animation and transition disabled via `prefers-reduced-motion`; states remain readable as text.
- **Data or event boundary:** all API strings go through `textContent` / `createTextNode`. No HTML-string sink exists in `app.js` (grep gate in the functional script). Actions POST JSON to `/api/runs/<id>/{control,decision,resume}`; the server stays authoritative and the page also re-checks the latest poll before sending.

## Verification ledger

- [x] Representative component rendered with real fixture content (see visual-review.md).
- [x] Mobile and desktop states captured.
- [x] Image review found no unresolved layout-breaking defect (open polish items are listed in visual-review.md).
- [x] Keyboard and focus states exercised by script (Enter on button, Space on checkbox, Escape, focus return, focus kept across two polls).
- [x] Loading, error, empty and disabled behavior checked: error/offline/unknown-run/disabled/in-flight exercised and photographed; the initial loading state was seen only transiently and is not captured as a PNG; the empty runs list was not exercised.
