# Design brief

- **Surface:** lmloop Rust operator console, `GET /` in `rust/src/web.rs` with assets in `rust/web/`.
- **Owner / date:** Designer subagent, 2026-11-20 (lm-1x4).
- **Primary user:** operator monitoring a long local run, checking its state, available controls and evidence. The same controls must remain usable on a phone.

## Outcome

The page presents run phase, lease state, task progress and the controls the server accepts, including its reason when an action is unavailable. Supporting sections show tasks, evidence, gate and log output, and events. The objective is absent from the operator API and must not be invented.

## Constraints

- Vanilla HTML/CSS/JS, no build step or remote assets. CSP allows only self-hosted resources (plus data images); no inline scripts or styles.
- Required states include loading, empty, error, offline/stale, in-flight action, disabled action, decided and owner lost.
- Render API data as text nodes or with `textContent`.
- Keyboard access, visible focus, 44px control targets and reduced-motion support.
- Mobile-first; runs and detail use separate views below 960px, with a two-pane layout above.
- **Visual direction:** retain the original Python dashboard identity in `web/static/style.css`. Apply restrained Impeccable polish within that identity. These choices are specific to this Rust dashboard, not a global lmloop design rule. No new functionality or behavior is part of the visual direction.
- No claims of authentication, PWA, Android parity or production verification. The banner identifies fixture evidence and loopback use.

## Verified outcome

- [x] Phase, progress, lease, failure, tasks, evidence, logs and events are available for a run.
- [x] Actions require explicit confirmation; Stop now, Approve and Request changes also require acknowledgement, and Request changes requires a note.
- [x] Stop remains usable while a Resume request is open.
- [x] No horizontal overflow at 1440px or 390px.

Visual inspection and verification evidence are recorded in `visual-review.md`. Component behavior is described in `components.md`; `visual-language.md` and other earlier Editorial Blueprint briefs are superseded by the direction above.
