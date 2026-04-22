// WebGL2 timeline renderer.
//
// Data model:
//   - One big instance buffer per category (KERNEL/MEMCPY/API) per GPU.
//     Each instance is 5 floats = 20 bytes: [start, width, lane, color, corr].
//     10M instances = 200MB; we keep a soft cap of 1.5M instances in the
//     live buffer by relying on server-side tile decimation.
//   - One float-pair buffer for each metric line; decimated to ~tile count.
//
// Coordinate system:
//   - Time is passed to the GPU as f32 "ns since baseline" where baseline =
//     viewport.start_ns rounded down to a safe multiple. This gives us ~6µs
//     precision per f32 at 1hr viewports, which is under one pixel at any
//     realistic screen width.
//   - Y origin is the top of the canvas.

import { VERT, FRAG, METRIC_VERT, METRIC_FRAG } from "./shaders";

const CORNERS = new Float32Array([
  0, 0,  1, 0,  0, 1,
  1, 0,  1, 1,  0, 1,
]);

export type ColorId = 0 | 1 | 2 | 3 | 4 | 5;

export interface Viewport {
  startNs: bigint;
  endNs: bigint;
  widthPx: number;
  heightPx: number;
}

export interface LaneLayout {
  /** Index of this lane in vertical ordering (0 = topmost). */
  index: number;
  /** Height in CSS px. */
  heightPx: number;
  kind: "activity" | "metric";
  label: string;
}

export interface InstanceBatch {
  lane: number;
  colorId: ColorId;
  starts: Float64Array;   // in ns
  widths: Float64Array;   // in ns
  corrLo: Float32Array;   // low 32 bits of correlation_id as float
}

export interface MetricSeries {
  lane: number;
  color: [number, number, number];
  points: Float64Array;   // interleaved (ts_ns, value_norm) pairs
}

export class TimelineRenderer {
  private gl: WebGL2RenderingContext;
  private prog: WebGLProgram;
  private metricProg: WebGLProgram;
  private cornerBuf: WebGLBuffer;
  private instanceBuf: WebGLBuffer;
  private metricBuf: WebGLBuffer;
  private vao: WebGLVertexArrayObject;
  private metricVao: WebGLVertexArrayObject;

  private uniforms = {
    viewRangeNs: 0 as WebGLUniformLocation | null,
    laneHeightPx: 0 as WebGLUniformLocation | null,
    viewportPx: 0 as WebGLUniformLocation | null,
    highlightCorrLo: 0 as WebGLUniformLocation | null,
    metric_viewRangeNs: 0 as WebGLUniformLocation | null,
    metric_laneY: 0 as WebGLUniformLocation | null,
    metric_viewportPx: 0 as WebGLUniformLocation | null,
    metric_color: 0 as WebGLUniformLocation | null,
  };

  private instanceCount = 0;
  private currentBatches: InstanceBatch[] = [];
  private currentMetrics: MetricSeries[] = [];
  private lanes: LaneLayout[] = [];
  private highlightCorrLo = 0;
  private baselineNs = 0n;
  /** DPR used when the instance buffer was last uploaded. The activity
   *  instance format bakes `yTopDevicePx` into the buffer, so a DPR change
   *  (e.g. user drags the window to a retina display, or hot-swaps an
   *  external monitor) invalidates the whole buffer. We track it here and
   *  re-upload from `resize()` when it flips. */
  private lastUploadDpr = 0;

  constructor(canvas: HTMLCanvasElement) {
    const gl = canvas.getContext("webgl2", { antialias: false, alpha: false });
    if (!gl) throw new Error("WebGL2 is required. Update your browser or enable hardware acceleration.");
    this.gl = gl;

    this.prog = compileProgram(gl, VERT, FRAG);
    this.metricProg = compileProgram(gl, METRIC_VERT, METRIC_FRAG);

    // Uniforms
    gl.useProgram(this.prog);
    this.uniforms.viewRangeNs = gl.getUniformLocation(this.prog, "u_view_range_ns");
    this.uniforms.laneHeightPx = gl.getUniformLocation(this.prog, "u_lane_height_px");
    this.uniforms.viewportPx = gl.getUniformLocation(this.prog, "u_viewport_px");
    this.uniforms.highlightCorrLo = gl.getUniformLocation(this.prog, "u_highlight_corr_lo");

    gl.useProgram(this.metricProg);
    this.uniforms.metric_viewRangeNs = gl.getUniformLocation(this.metricProg, "u_view_range_ns");
    this.uniforms.metric_laneY = gl.getUniformLocation(this.metricProg, "u_lane_y_range");
    this.uniforms.metric_viewportPx = gl.getUniformLocation(this.metricProg, "u_viewport_px");
    this.uniforms.metric_color = gl.getUniformLocation(this.metricProg, "u_color");

    this.cornerBuf = mustBuf(gl);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.cornerBuf);
    gl.bufferData(gl.ARRAY_BUFFER, CORNERS, gl.STATIC_DRAW);

    this.instanceBuf = mustBuf(gl);
    this.metricBuf = mustBuf(gl);

    this.vao = mustVao(gl);
    gl.bindVertexArray(this.vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.cornerBuf);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.instanceBuf);
    // layout: start, width, lane, color, corr (5 floats, 20 bytes)
    const STRIDE = 20;
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 2, gl.FLOAT, false, STRIDE, 0);
    gl.vertexAttribDivisor(1, 1);
    gl.enableVertexAttribArray(2);
    gl.vertexAttribPointer(2, 1, gl.FLOAT, false, STRIDE, 8);
    gl.vertexAttribDivisor(2, 1);
    gl.enableVertexAttribArray(3);
    gl.vertexAttribPointer(3, 1, gl.FLOAT, false, STRIDE, 12);
    gl.vertexAttribDivisor(3, 1);
    gl.enableVertexAttribArray(4);
    gl.vertexAttribPointer(4, 1, gl.FLOAT, false, STRIDE, 16);
    gl.vertexAttribDivisor(4, 1);

    this.metricVao = mustVao(gl);
    gl.bindVertexArray(this.metricVao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.metricBuf);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);

    gl.bindVertexArray(null);

    gl.clearColor(0.043, 0.051, 0.063, 1.0);
  }

  setLanes(lanes: LaneLayout[]) { this.lanes = lanes.slice(); }
  setHighlightCorrLo(lo: number) { this.highlightCorrLo = lo | 0; }
  setBaseline(baselineNs: bigint) { this.baselineNs = baselineNs; }

  setActivity(batches: InstanceBatch[]) {
    this.currentBatches = batches;
    // Concatenate all batches into one instance buffer. Each instance is 5
    // floats = 20 bytes. The "lane" slot carries the pre-multiplied
    // yTopPx (in *device* pixels) so the shader can handle mixed lane
    // heights (metric lanes are tall, activity lanes are short).
    let total = 0;
    for (const b of batches) total += b.starts.length;
    const data = new Float32Array(total * 5);
    let o = 0;
    const baselineF = Number(this.baselineNs);
    const dpr = this.dpr();
    for (const b of batches) {
      const yTopDevicePx = laneTopPx(this.lanes, b.lane) * dpr;
      for (let i = 0; i < b.starts.length; i++) {
        data[o++] = (b.starts[i] - baselineF);
        data[o++] = b.widths[i];
        data[o++] = yTopDevicePx;
        data[o++] = b.colorId;
        data[o++] = b.corrLo[i];
      }
    }
    this.instanceCount = total;
    this.lastUploadDpr = dpr;
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.instanceBuf);
    gl.bufferData(gl.ARRAY_BUFFER, data, gl.DYNAMIC_DRAW);
  }

  setMetrics(series: MetricSeries[]) { this.currentMetrics = series; }

  resize(widthPx: number, heightPx: number, dpr: number) {
    const gl = this.gl;
    const canvas = gl.canvas as HTMLCanvasElement;
    const pw = Math.max(1, Math.floor(widthPx * dpr));
    const ph = Math.max(1, Math.floor(heightPx * dpr));
    if (canvas.width !== pw || canvas.height !== ph) {
      canvas.width = pw;
      canvas.height = ph;
    }
    gl.viewport(0, 0, pw, ph);
    // DPR flipped (external monitor hot-swap / zoom). The activity buffer
    // has `yTopDevicePx` baked in, so re-upload from the cached batches.
    // If we skip this, bars shift vertically and miss their lanes until
    // the next setActivity() call.
    if (this.lastUploadDpr !== 0 && Math.abs(this.lastUploadDpr - dpr) > 1e-3 && this.currentBatches.length > 0) {
      this.setActivity(this.currentBatches);
    }
  }

  render(viewport: Viewport) {
    const gl = this.gl;
    gl.clear(gl.COLOR_BUFFER_BIT);

    const baseline = Number(this.baselineNs);
    const startF = Number(viewport.startNs) - baseline;
    const endF = Number(viewport.endNs) - baseline;

    // Activity
    if (this.instanceCount > 0) {
      gl.useProgram(this.prog);
      gl.uniform2f(this.uniforms.viewRangeNs, startF, endF);
      gl.uniform2f(this.uniforms.viewportPx, gl.drawingBufferWidth, gl.drawingBufferHeight);
      const laneHeight = this.dpr() * activityLaneHeight(this.lanes);
      gl.uniform1f(this.uniforms.laneHeightPx, laneHeight);
      gl.uniform1f(this.uniforms.highlightCorrLo, this.highlightCorrLo);

      gl.bindVertexArray(this.vao);
      gl.drawArraysInstanced(gl.TRIANGLES, 0, 6, this.instanceCount);
    }

    // Metric lanes
    if (this.currentMetrics.length > 0) {
      gl.useProgram(this.metricProg);
      gl.uniform2f(this.uniforms.metric_viewRangeNs, startF, endF);
      gl.uniform2f(this.uniforms.metric_viewportPx, gl.drawingBufferWidth, gl.drawingBufferHeight);

      for (const m of this.currentMetrics) {
        const lane = this.lanes[m.lane];
        if (!lane) continue;
        const yTop = laneTopPx(this.lanes, m.lane) * this.dpr();
        const yBot = (laneTopPx(this.lanes, m.lane) + lane.heightPx) * this.dpr();
        gl.uniform2f(this.uniforms.metric_laneY, yTop + 4, yBot - 4);
        gl.uniform3f(this.uniforms.metric_color, m.color[0], m.color[1], m.color[2]);
        // Upload this series and draw as line strip.
        const pts = new Float32Array(m.points.length);
        for (let i = 0; i < m.points.length; i += 2) {
          pts[i] = m.points[i] - baseline;
          pts[i + 1] = m.points[i + 1];
        }
        gl.bindBuffer(gl.ARRAY_BUFFER, this.metricBuf);
        gl.bufferData(gl.ARRAY_BUFFER, pts, gl.DYNAMIC_DRAW);
        gl.bindVertexArray(this.metricVao);
        gl.drawArrays(gl.LINE_STRIP, 0, pts.length / 2);
      }
    }
  }

  /** CPU-side hit test for hover / click. Returns first matching instance. */
  pick(viewport: Viewport, xPx: number, yPx: number): { lane: number; index: number; batch: number } | null {
    const baseline = Number(this.baselineNs);
    const startF = Number(viewport.startNs) - baseline;
    const endF = Number(viewport.endNs) - baseline;
    const range = endF - startF;
    const tsHit = startF + (xPx / viewport.widthPx) * range;

    for (let bi = 0; bi < this.currentBatches.length; bi++) {
      const b = this.currentBatches[bi];
      const lane = this.lanes[b.lane];
      if (!lane) continue;
      const laneTop = laneTopPx(this.lanes, b.lane);
      const laneBottom = laneTop + lane.heightPx;
      if (yPx < laneTop || yPx > laneBottom) continue;
      // Linear scan — fine for our typical ~200k visible instances. Tile
      // decimation keeps this well below 1M at the finest zoom.
      for (let i = 0; i < b.starts.length; i++) {
        const s = b.starts[i] - baseline;
        const w = b.widths[i];
        if (tsHit >= s && tsHit <= s + w) {
          return { lane: b.lane, index: i, batch: bi };
        }
      }
    }
    return null;
  }

  private dpr() { return (this.gl.canvas as HTMLCanvasElement).width / (this.gl.canvas as HTMLCanvasElement).clientWidth; }
}

function compileProgram(gl: WebGL2RenderingContext, vert: string, frag: string): WebGLProgram {
  const v = gl.createShader(gl.VERTEX_SHADER)!;
  gl.shaderSource(v, vert); gl.compileShader(v);
  if (!gl.getShaderParameter(v, gl.COMPILE_STATUS)) throw new Error("vert: " + gl.getShaderInfoLog(v));
  const f = gl.createShader(gl.FRAGMENT_SHADER)!;
  gl.shaderSource(f, frag); gl.compileShader(f);
  if (!gl.getShaderParameter(f, gl.COMPILE_STATUS)) throw new Error("frag: " + gl.getShaderInfoLog(f));
  const p = gl.createProgram()!;
  gl.attachShader(p, v); gl.attachShader(p, f); gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error("link: " + gl.getProgramInfoLog(p));
  return p;
}

function mustBuf(gl: WebGL2RenderingContext): WebGLBuffer {
  const b = gl.createBuffer();
  if (!b) throw new Error("could not allocate WebGL buffer");
  return b;
}
function mustVao(gl: WebGL2RenderingContext): WebGLVertexArrayObject {
  const v = gl.createVertexArray();
  if (!v) throw new Error("could not allocate WebGL VAO");
  return v;
}

function activityLaneHeight(lanes: LaneLayout[]): number {
  // Uniform height for activity lanes; for MVP. Metric lanes can differ.
  const activity = lanes.find((l) => l.kind === "activity");
  return activity?.heightPx ?? 22;
}

export function laneTopPx(lanes: LaneLayout[], laneIndex: number): number {
  let y = 0;
  for (let i = 0; i < laneIndex; i++) y += lanes[i]?.heightPx ?? 22;
  return y;
}
