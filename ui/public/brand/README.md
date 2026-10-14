# OpenProfiler identity

An open ring represents the **O** and open-source access. Three sample bars inside
the ring connect the mark to CPU samples, allocations, and profiling timelines.

- `openprofiler-mark.svg`: editable vector master, used by the React UI.
- `openprofiler-logo.svg`: horizontal logo with an editable OpenProfiler wordmark.
- `../favicon.svg`: dark rounded tile for tabs and app icons.
- `../favicon-32x32.png`, `../favicon.ico`: browser fallbacks.
- `../apple-touch-icon.png`: 180 × 180 touch icon.
- `../../src-tauri/icons/`: native desktop icons (PNG, ICO, ICNS).

The primary brand color is orange (`#ff7a3d`) on ink (`#151820`). The UI mark
inherits the theme accent, including yellow in high-contrast mode. The wordmark
uses the UI font with regular “Open” and bold “Profiler”. The standalone SVG
wordmark uses Inter when available and falls back to system sans-serif fonts.

After editing the vector master, regenerate the logo and all icon formats from
the `ui` directory with the existing Tauri dependency:

```sh
node scripts/generate-brand.mjs
```
