# Voice orb study

A prototype of Margin's voice agent as a single liquid-glass object, kept for the voice UI work (slice S6/S7 in `docs/ROADMAP.md`). It is a design reference, not app code: plain HTML and WebGL2, no build step needed to view.

Open `index.html` in a browser. The object cycles idle, listening, thinking and speaking on its own; a tap moves to the next mode, `t` switches light and dark, and the number keys switch forms:

1. Pearl (`index.html`): the chosen orb, a raymarched glass sphere that turns ruby as it gets more active.
2. Droplet (`option-droplet.html`): metaball glass that buds and reabsorbs beads.
3. Medusa (`option-medusa.html`): a translucent bell with trailing filaments.
4. Silk (`option-silk.html`): fine threads that gather into a sphere when it speaks.

The one accent colour is `ACCENT = '#c41a1e'` in `src/core-a.js`. Sound is a generative Web Audio pad that starts on the first tap. To rebuild the pages after editing `src/`, run `python3 -I src/build.py`.

`shots/` has a still of each form, idle in light mode and speaking in dark mode.
