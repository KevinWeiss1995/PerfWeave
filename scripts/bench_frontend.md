# Frontend gate — 10M events at 60 FPS

Run this benchmark on the target hardware (the workstation the user runs
`perfweave start` on). It validates the Week 1 gate from the architecture
plan. Anything below 60 FPS here blocks MVP.

## How

```bash
cd web
npm install
npm run dev
```

Open `http://localhost:5173/?synthetic` in Chrome (not Safari — WebGL2
perf on Safari is ~30% slower).

The URL query string `?synthetic` tells `App.tsx` to push 10M synthetic
events into the renderer. The HUD (bottom-left of the timeline) reports:

```
<fps> fps · <count> events · <lanes> lanes · <window>
```

## Gate

- FPS during steady pan/zoom must be **>= 60** on a modern laptop GPU
  (M1 or Intel Iris Xe class) at 1920x1080 viewport.
- Memory footprint (Chrome DevTools → Memory → heap snapshot) under **500 MB**.
- Initial render (time from page load to first frame) under **1500 ms**.

## What to measure

1. Open Chrome DevTools → Performance → Record.
2. Hold mouse and drag left/right for 3 seconds to pan.
3. Scroll the wheel continuously for 3 seconds to zoom.
4. Stop and inspect:
   - Frame times should be 16.67 ms (60 fps) or better.
   - `drawArraysInstanced` calls should be one per (gpu, category) batch.
   - No layout thrash in the main thread.

## If the gate fails

1. First suspect: tile decimation. The server-side tile queries should
   reduce instance counts to ~1.5M max at finest zoom; the 10M synthetic
   dataset is the stress case. If the real `activity_to_arrow` path is
   emitting 10M instances, it means tiles are not being merged — check
   `tiles_activity` materialized views are populated.
2. Second suspect: single-instance buffer churn. If the HUD reports <60
   FPS even when panning (not fetching), the `setActivity` rebuild is
   the bottleneck. Profile `bufferData` — if it dominates, switch to
   `bufferSubData` with a ring-allocated buffer twice the live size.
3. Third suspect: Arrow decode on the main thread. If `tableFromIPC` is
   taking >50ms, move it to a Web Worker.

## Historical results

| date       | device               | result     |
|------------|----------------------|------------|
| TODO       | M1 MacBook Pro       | n/a        |
| TODO       | RTX 4090 / Chrome    | n/a        |
