// Top-level layout. Owns:
//   - viewport (shared between Timeline and SidePanel so the SidePanel
//     shows context for whatever is currently on screen)
//   - selection / measurement state
//   - the spike markers fetched by the SidePanel, which are also rendered
//     as vertical pins on the Timeline for at-a-glance "look here" cues
//
// The `?synthetic` query string forces the Week 1 gate mode (10M events,
// no backend required).

import { useCallback, useEffect, useState } from "react";
import { Timeline, type Selection } from "./timeline/Timeline";
import { SidePanel, type Measurement, type SpikeMarker } from "./panels/SidePanel";
import { importNsys } from "./api-imports";

export function App() {
  const urlParams = new URLSearchParams(window.location.search);
  const useSynthetic = urlParams.has("synthetic");

  const [selection, setSelection] = useState<Selection | null>(null);
  const [measurement, setMeasurement] = useState<Measurement | null>(null);
  const [viewport, setViewport] = useState<{ startNs: bigint; endNs: bigint }>(() => {
    const now = BigInt(Date.now()) * 1_000_000n;
    return { startNs: now - 60_000_000_000n, endNs: now };
  });
  const [spikes, setSpikes] = useState<SpikeMarker[]>([]);
  const [focusedSpike, setFocusedSpike] = useState<SpikeMarker | null>(null);

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

  return (
    <div className="app" onDragOver={onDragOver} onDrop={onDrop}>
      <div className="topbar">
        <div className="brand">perf<b>weave</b></div>
        <div className="status">
          {useSynthetic ? "mode: synthetic (10M events)" : "mode: live"}
        </div>
        <div className="zoom-hint">scroll to zoom · drag to pan · click to drilldown · shift-click to measure</div>
      </div>
      <Timeline
        onSelect={setSelection}
        onMeasure={onMeasure}
        useSynthetic={useSynthetic}
        viewport={viewport}
        onViewportChange={setViewport}
        spikes={spikes}
        onSpikeClick={setFocusedSpike}
      />
      <SidePanel
        selection={selection}
        viewport={viewport}
        measurement={measurement}
        useSynthetic={useSynthetic}
        onSpikesChanged={setSpikes}
        focusedSpike={focusedSpike}
      />
    </div>
  );
}
