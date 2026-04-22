// Top-level layout. Owns:
//   - viewport (shared between Timeline and SidePanel)
//   - selection / measurement state
//   - the spike markers fetched by the SidePanel, which are also rendered
//     as vertical pins on the Timeline for at-a-glance "look here" cues
//   - Live mode: when on, the viewport auto-advances every 1s and the
//     Timeline draws metric data from the client-side live ring instead
//     of Arrow tiles. Spike pins also flow through the SSE stream.
//
// The `?synthetic` query string forces the Week 1 gate mode (10M events,
// no backend required).

import { useCallback, useEffect, useState } from "react";
import { Timeline, type Selection } from "./timeline/Timeline";
import { SidePanel, type Measurement, type SpikeMarker } from "./panels/SidePanel";
import { SpikeDrilldown } from "./panels/SpikeDrilldown";
import { importNsys } from "./api-imports";
import { useLiveStream } from "./live";

/** Width of the follow window in Live mode. 60s matches the spike
 *  detector's median window and gives a few cycles of context. */
const LIVE_WINDOW_NS = 60_000_000_000n;

export function App() {
  const urlParams = new URLSearchParams(window.location.search);
  const useSynthetic = urlParams.has("synthetic");

  const [live, setLive] = useState(!useSynthetic);
  const liveStream = useLiveStream(live);

  const [selection, setSelection] = useState<Selection | null>(null);
  const [measurement, setMeasurement] = useState<Measurement | null>(null);
  // In Live mode the Timeline owns a RAF-driven virtual clock so the tape
  // scrolls smoothly between 1 Hz samples. We only seed the viewport width
  // here; the actual start/end used for rendering is derived inside Timeline
  // from that virtual clock. When the user pans/zooms, onViewportChange
  // fires and we drop out of live mode.
  const [viewport, setViewport] = useState<{ startNs: bigint; endNs: bigint }>(() => {
    const now = BigInt(Date.now()) * 1_000_000n;
    return { startNs: now - LIVE_WINDOW_NS, endNs: now };
  });
  const [spikes, setSpikes] = useState<SpikeMarker[]>([]);
  const [focusedSpike, setFocusedSpike] = useState<SpikeMarker | null>(null);
  const [drilldownSpike, setDrilldownSpike] = useState<SpikeMarker | null>(null);

  // Clicking a spike pin focuses its card in the side panel *and* opens
  // the drilldown drawer. The two are deliberately coupled: the card and
  // the drawer answer different granularities of the same question.
  const onSpikeClick = useCallback((s: SpikeMarker) => {
    setFocusedSpike(s);
    setDrilldownSpike(s);
  }, []);

  const onMeasure = useCallback((a: bigint, b: bigint) => {
    setMeasurement({
      startNs: a < b ? a : b,
      endNs: a < b ? b : a,
    });
  }, []);

  const onDragOver = (e: React.DragEvent) => { e.preventDefault(); };
  const onDrop = async (e: React.DragEvent) => {
    e.preventDefault();
    for (const f of Array.from(e.dataTransfer.files)) {
      if (f.name.endsWith(".nsys-rep")) {
        await importNsys(f);
      }
    }
  };

  useEffect(() => {
    document.addEventListener("dragover", (e) => e.preventDefault());
    document.addEventListener("drop", (e) => e.preventDefault());
  }, []);

  // Any viewport change comes from user interaction (pan, zoom, double-click).
  // The Timeline does NOT call this on every RAF frame while live — that part
  // is internal — so this is safe to bounce us out of live mode.
  const onViewportChange = useCallback(
    (v: { startNs: bigint; endNs: bigint }) => {
      setViewport(v);
      if (live) setLive(false);
    },
    [live],
  );

  const modeLabel = useSynthetic
    ? "mode: synthetic (10M events)"
    : live
      ? liveStream.connected
        ? "mode: live · following now"
        : "mode: live · connecting…"
      : "mode: paused";

  return (
    <div className="app" onDragOver={onDragOver} onDrop={onDrop}>
      <div className="topbar">
        <div className="brand">perf<b>weave</b></div>
        <div className="status">{modeLabel}</div>
        <button
          className={`live-pill ${live ? "on" : "off"} ${liveStream.connected ? "ok" : "warn"}`}
          onClick={() => setLive((v) => !v)}
          title={live ? "Pause (scrolling also pauses)" : "Follow live data"}
          disabled={useSynthetic}
        >
          {live ? "LIVE" : "LIVE OFF"}
        </button>
        <div className="zoom-hint">
          scroll/drag = pan · ctrl+scroll = zoom · double-click = now · shift-click = measure
        </div>
      </div>
      <Timeline
        onSelect={setSelection}
        onMeasure={onMeasure}
        useSynthetic={useSynthetic}
        viewport={viewport}
        onViewportChange={onViewportChange}
        spikes={spikes}
        onSpikeClick={onSpikeClick}
        liveSeries={live ? liveStream.series : null}
        liveKernels={live ? liveStream.kernels : null}
        liveKernelsTick={liveStream.kernelsTick}
        liveLastTsNs={liveStream.lastTsNs}
        live={live}
        skewNs={liveStream.skewNs}
        lastSampleAgeMs={liveStream.lastSampleAgeMs}
        metricRateHz={liveStream.metricRateHz}
        kernelRateHz={liveStream.kernelRateHz}
        connected={liveStream.connected}
      />
      <SidePanel
        selection={selection}
        viewport={viewport}
        measurement={measurement}
        useSynthetic={useSynthetic}
        onSpikesChanged={setSpikes}
        focusedSpike={focusedSpike}
      />
      {drilldownSpike && (
        <SpikeDrilldown
          spike={drilldownSpike}
          onClose={() => setDrilldownSpike(null)}
        />
      )}
    </div>
  );
}
