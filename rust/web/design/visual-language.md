# Visual language

> **Superseded:** The initial Editorial Blueprint direction below was a visual brief, not the design authority. The implemented Rust dashboard retains the original Python dashboard identity in `web/static/style.css`, with restrained Impeccable polish. This is specific to the Rust dashboard, not a global lmloop design rule. The historical brief is preserved below as evidence of the original proposal; do not use it as current guidance.

## Historical proposal (superseded, 2026-11-20)

- **Direction name:** Editorial Blueprint, restrained operator edition.
- **Reference qualities proposed:** warm drafting-paper surface with a faint 44px grid, 1px hairline rules, zero border-radius, hard offset shadows on hover and on the dark terminals, carbon-ink telemetry blocks, numbered section markers (`00`–`05`).
- **Qualities explicitly excluded:** remote Google Fonts (CSP and no-remote-assets rule), grain overlay, cursor crosshair, mouse parallax, marquee, text scramble, rounded SaaS cards.
- **Version / date:** rev 1, 2026-11-20. Tokens copied from `editorial-blueprint/tokens.css`.

## Hierarchy

- Focal point: run id (display, up to 3rem) next to its phase badge, over the progress bar.
- Reading order: masthead, controls, tasks and evidence (two columns from 1180px), gate/logs, events.
- Primary / secondary / tertiary emphasis: ink-filled buttons and coloured badges; hairline tables; soft mono captions.
- Density target: dashboard-dense but with 44px controls; rows separated by hairlines, not boxes.
- Responsive priority changes: below 960px the runs pane and the detail are separate views (`body[data-view]`, hash route `#/run/<id>`); controls stack one per row with the reason beside the button; below 520px the `/operator` brand suffix is hidden and the events grid collapses.

## Typography

System fallbacks only. Because the web fonts are not loaded, the stacks below resolve to the fallbacks. The families are named so a local install is used if present.

| Role | Family | Size / line-height | Weight | Tracking | Use |
|---|---|---|---|---|---|
| Display | `'Bricolage Grotesque', system-ui…` | clamp(1.7rem, 4.4vw, 3rem) / .98 | 800 | -.045em | run id |
| Heading | same | clamp(1.35rem, 2.2vw, 1.8rem) / 1.04 | 800 | -.035em | section titles |
| Body | `'Newsreader', Georgia…` | clamp(16px, 1.05vw, 18px) / 1.55 | 300 | 0 | page default (little prose is shown) |
| Label / caption | `'JetBrains Mono', ui-monospace…` | 10–13px / 1.5 | 400–700 | .04–.16em uppercase for labels | badges, buttons, facts, logs, events |

## Color roles

| Role | Token | Value | Contrast / usage |
|---|---|---|---|
| Canvas | `--paper` | #F3F0E7 | page, with 5% ink grid |
| Surface | `--card` | #FBF9F3 | panels, selected run, dialog |
| Text | `--ink` | #111317 | body, filled buttons |
| Muted text | `--soft` | #585E6A | captions; used for reasons on disabled controls |
| Accent | `--blue` #2646C6, `--flare` #FF4A1C | | live phase, links/focus; flare only as rule/marker/shadow |
| Small flare text | `--flare-ink` | #B82E08 | replaces `--flare` for text and borders, because #FF4A1C on `--card` is about 3:1 (estimated, not measured) |
| Danger / success / warning | `--flare-ink` / `--mint` #4CC995 / `--amber` #F5B94A | | badges always carry a text label, never colour alone |

No automated contrast measurement was run. The values above are estimates from the palette.

## Spatial and shape language

- Spacing scale: rem multiples of .3 / .6 / .9 / 1.4; container `--max: 1340px`, gutter `clamp(1rem, 4vw, 3rem)`.
- Container and grid rules: shell grid `minmax(280px,340px) 1fr` from 960px; controls 3×2 matrix from 720px; tasks/evidence two columns from 1180px.
- Radius rules: 0 everywhere except status dots and the loading spinner (50%).
- Border / divider rules: 1px `--rule` hairlines; 1.5px ink rule on top of lists; dashed borders mean "unavailable / informational".
- Shadow / elevation rules: hard offsets only (`4px 4px 0` on button hover, `8px 8px 0` on terminals and dialog); no blur shadows.
- Image, icon, and illustration treatment: none; `data:,` favicon.

## Responsive rules

- Mobile viewport and behavior: 390×844, single column, back link on the detail view.
- Desktop viewport and behavior: 1440×900, sticky runs pane at left, detail at right.
- Breakpoint decisions: 520 / 720 / 960 / 1180.
- Content that reorders, collapses, or disappears: runs pane becomes its own view below 960px; `.brand em` hidden below 520px.
- No-overflow rule: every long token (`run_id`, log names, JSON event data) uses `overflow-wrap: anywhere`. Measured `scrollWidth - innerWidth = 0` for desktop, mobile list and mobile detail.

## Review questions

- Focal point obvious without decoration? Yes in the final captures: the display-size run id and phase badge lead.
- Which rule prevents the page from becoming generic? Hairline matrices, numbered sections, dashed-for-unavailable, and hard shadows.
