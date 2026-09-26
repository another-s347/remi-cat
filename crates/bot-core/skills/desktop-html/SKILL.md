---
name: desktop-html
description: "Desktop-only protocol for self-contained HTML visualizations. Load only when the current remi-output capability list contains html and a visual or interactive presentation would materially help. Do not use for ordinary prose or code examples."
---

# Desktop HTML

Use this skill only when the current `<remi-output>` context has `html` in
`caps`. If it does not, answer with ordinary Markdown. Prefer Markdown and
native math for normal explanations; use HTML for diagrams, dashboards,
simulations, or interactions that genuinely benefit from a visual surface.

Emit the visualization in one fenced block:

````markdown
```remi-html
<main class="canvas" aria-label="Description of the visualization">
  <!-- self-contained HTML -->
</main>
<style>
  /* scoped presentation */
</style>
<script>
  // optional local interaction
</script>
```
````

## Contract

- Make the block self-contained. Use inline HTML, CSS, SVG, and optional plain JavaScript.
- Do not use remote scripts, styles, fonts, images, network requests, frames, navigation, popups, downloads, or local file URLs.
- Do not rely on Desktop application DOM, globals, storage, cookies, or native APIs.
- Scope CSS beneath one root class; do not style `html`, `body`, or global selectors.
- Use responsive layout, readable contrast, semantic labels, and keyboard-accessible controls.
- Put a short Markdown explanation outside the block when the visual needs context.
- Never wrap an ordinary HTML code sample in `remi-html`; use a normal `html` fence for source code.
