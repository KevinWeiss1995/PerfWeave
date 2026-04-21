// Shaders for the timeline. We draw one instanced quad per event/tile;
// per-instance attributes are (x_start_ns, width_ns, lane, color_id,
// correlation_id_lo32). The fragment shader colors the quad based on
// color_id, optionally highlighting when correlation_id matches the
// currently selected correlation.

export const VERT = `#version 300 es
precision highp float;

// Per-vertex (0..1 quad corners)
layout(location = 0) in vec2 a_corner;

// Per-instance
layout(location = 1) in vec2 a_range_ns;     // (start_ns, width_ns) packed as floats (64-bit ns → f32 via baseline subtraction)
layout(location = 2) in float a_lane;         // lane index (per GPU × category)
layout(location = 3) in float a_color_id;
layout(location = 4) in float a_corr_lo;      // low 32 bits of correlation_id as float (for highlight compare)

// Uniforms
uniform vec2 u_view_range_ns;   // start_ns - baseline, end_ns - baseline, as f32
uniform float u_lane_height_px;
uniform vec2 u_viewport_px;
uniform float u_highlight_corr_lo;

out vec3 v_color;
out float v_highlight;

const vec3 PALETTE[6] = vec3[6](
  vec3(0.29, 0.64, 1.00),   // kernel
  vec3(0.39, 0.83, 0.65),   // memcpy
  vec3(0.72, 0.55, 1.00),   // api
  vec3(1.00, 0.72, 0.29),   // metric
  vec3(1.00, 0.29, 0.42),   // spike
  vec3(0.53, 0.58, 0.64)    // other
);

void main() {
  float start = a_range_ns.x;
  float width = max(a_range_ns.y, 0.0);
  float x_ns = start + a_corner.x * width;

  float range = u_view_range_ns.y - u_view_range_ns.x;
  float x_norm = (x_ns - u_view_range_ns.x) / range;      // 0..1 in viewport
  float x_clip = x_norm * 2.0 - 1.0;

  float y_top_px = a_lane * u_lane_height_px;
  float y_px = y_top_px + a_corner.y * (u_lane_height_px - 2.0);
  float y_norm = y_px / u_viewport_px.y;
  float y_clip = 1.0 - y_norm * 2.0;                      // top-origin

  gl_Position = vec4(x_clip, y_clip, 0.0, 1.0);

  int idx = int(clamp(a_color_id, 0.0, 5.0));
  v_color = PALETTE[idx];
  v_highlight = (abs(a_corr_lo - u_highlight_corr_lo) < 0.5 && u_highlight_corr_lo > 0.5) ? 1.0 : 0.0;
}
`;

export const FRAG = `#version 300 es
precision highp float;

in vec3 v_color;
in float v_highlight;
out vec4 outColor;

void main() {
  vec3 c = v_color;
  if (v_highlight > 0.5) {
    // Crank saturation and lift to white for the correlated peers.
    c = mix(c, vec3(1.0, 1.0, 1.0), 0.55);
  }
  outColor = vec4(c, 1.0);
}
`;

// A second program draws metric lanes as a line strip from (x, y=value_norm).
// We keep the shader trivial because metric lanes render tens of thousands of
// points, not millions.
export const METRIC_VERT = `#version 300 es
precision highp float;
layout(location = 0) in vec2 a_point;   // (ts_ns_f32, value_norm)
uniform vec2 u_view_range_ns;
uniform vec2 u_lane_y_range;            // (top_px, bottom_px) in viewport pixels
uniform vec2 u_viewport_px;
out float v_value;
void main() {
  float range = u_view_range_ns.y - u_view_range_ns.x;
  float x_norm = (a_point.x - u_view_range_ns.x) / range;
  float x_clip = x_norm * 2.0 - 1.0;
  float y_px = mix(u_lane_y_range.y, u_lane_y_range.x, clamp(a_point.y, 0.0, 1.0));
  float y_norm = y_px / u_viewport_px.y;
  float y_clip = 1.0 - y_norm * 2.0;
  gl_Position = vec4(x_clip, y_clip, 0.0, 1.0);
  v_value = a_point.y;
}
`;

export const METRIC_FRAG = `#version 300 es
precision highp float;
in float v_value;
out vec4 outColor;
uniform vec3 u_color;
void main() { outColor = vec4(u_color, 1.0); }
`;
