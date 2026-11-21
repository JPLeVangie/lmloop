# Anti-pattern register

- **Surface:** lmloop operator console
- **Revision / date:** rev 1, 2026-11-20

## Explicit exclusions

| Exclusion | Why it harms this surface | Review signal / replacement |
|---|---|---|
| HTML-string sinks (`innerHTML`, `insertAdjacentHTML`, `document.write`, `eval`) | worker output, notes and logs are untrusted terminal text | grep gate in `func.mjs`; use `textContent` |
| Remote fonts, CDNs, analytics | CSP is `'self'` only; the page must work offline on loopback | grep for `https?://` in html/css/js returns nothing; system font fallbacks |
| Inventing an objective, ETA or progress the API does not give | an operator could trust an invented fact | Objective row says "Not exposed by the operator API in this milestone." |
| Hiding unavailable actions | the operator loses the reason a control is refused | `aria-disabled` dashed button plus server reason text |
| Native `disabled` on buttons | removes focus and the reason from keyboard users | `aria-disabled` plus click guard |
| One-click destructive actions | Stop now abandons an in-flight task | `<dialog>` confirm plus acknowledgement checkbox |
| Optimistic UI | claiming "stopped" before the engine acts | flash text shows the server's receipt/outcome; phase changes only on poll |
| A global "busy" lock | would block Stop while a Resume request is open | in-flight tracked per `run:action`; only the two decisions exclude each other |
| Rebuilding the DOM on every poll | steals focus and scroll | `rebuild(box, sig, build)` with signature and `data-fk` focus restore |
| Decorative effects (grain, crosshair, parallax, marquee, scramble, glass) | compete with status information | removed on purpose |
| Colour-only status | fails for colour-blind operators | every badge has text; unavailable has dashes and a prefix |
| Auth, PWA, Android parity claims | not in this milestone | banner says fixture evidence only; no service worker or manifest |

## Coherence checks

- [x] One clear focal point exists at mobile and desktop (run id plus phase, seen in `shots-4/desktop-live.png`, `shots-4/mobile-live.png`).
- [x] Decorative effects do not compete with task content (only the faint grid remains).
- [x] Components share tokens and state rules.
- [x] Copy, icons, imagery, and motion belong to the same visual language (no icons or imagery at all).
- [ ] The page does not resemble a default template after removing color: not judged separately; not claimed.

## Review prompts

- What is the first thing a user notices? The run id and phase, then the black/orange control buttons. Intentional.
- Which element is visually loud without carrying user value? The solid burnt-orange "Stop now" is loud on purpose, but it is the most dangerous action and it is available whenever Stop is, so it draws the eye more than Pause. Consider switching it to ink with a flare border.
- What breaks at the narrowest supported width? Checked 390px only; no narrower viewport was tested.
- Which choice is a one-off? The two-column tasks/evidence split leaves a tall empty area beside a short task list (visible in `shots-4/desktop-failed-log.png`).
