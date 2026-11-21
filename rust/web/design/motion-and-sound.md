# Motion and sound policy

- **Surface:** lmloop operator console
- **Revision / date:** rev 1, 2026-11-20
- **Motion personality in one sentence:** quiet instrument-panel feedback: small mechanical lifts on hover and slow pulses only where something is live.
- **Sound present:** no

## Motion principles

- Purpose of motion: show liveness (sync dot, running task pip), acknowledge a hover/press, show an in-flight request (spinner).
- What must remain still: all text, logs, events and layout. Data updates never animate and never move scroll position or focus.
- Enter / exit behavior: none; the dialog uses the native `<dialog>` with no transition.
- Feedback behavior: button hover lifts 2px with a hard shadow; press cancels the lift; busy buttons change label and cursor.
- Timing range: .2–.3s for hover; 1s linear spinner; 2.2–2.6s pulses.
- Easing: `cubic-bezier(.16, 1, .3, 1)` (design-system `--ease`).
- Stagger or sequencing rule: none.
- Performance ceiling: only `transform`, `box-shadow`, `background` and `box-shadow` pulses; polling every 5s (backs off to 30s, paused when the tab is hidden).

## Reduced motion

Under `prefers-reduced-motion: reduce`, `animation` and `transition` are `none !important` for all elements, and `scroll-behavior` is `auto`:

- [x] Nonessential movement removed (hover lift disabled).
- [x] Essential state changes remain perceivable without animation: liveness is the text "Live updated HH:MM:SS", running is a "RUNNING" badge, in-flight is the "Resuming…" label and banner text.
- [x] Autoplay, looping, and parallax: no autoplay; loops (pulse, spinner) are removed; there is no parallax.

## Sound

Not applicable. There is no audio.

## Verification

- [x] Default motion exercised in the rendered UI: the script found running animations with normal motion.
- [x] Reduced-motion mode exercised: the script found zero running animations on a `reducedMotion: "reduce"` context.
- [x] No motion obscures focus, content, or error feedback: the transient mid-transition capture in `func-3/desktop-pause-pending.png` shows the Pause button between its dark and unavailable styles; the "Unavailable: PAUSE already requested" text is already correct.
- [x] Sound policy marked not applicable.
