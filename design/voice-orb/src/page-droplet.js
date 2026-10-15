// ============================================================ Droplet ====
// A ruby-glass droplet that is never quite one thing: beads split from it and flow back in.
// Idle: one drop, a bead slowly parting and rejoining. Listening: beads rise from below, toward
// you, and are drawn into the body, faster with the voice. Thinking: the beads circle close and
// bridge to the body in a slow swirl. Speaking: on each phrase beads are thrown outward and
// shrink back into the body as new ones bud.
const NB = 7;
const FRAG = `#version 300 es
precision highp float;
out vec4 fragColor;
uniform vec2 uRes, uC;
uniform float uR, uT, uL, uDark, uK, uSpin, uPulse, uBodyR, uSmooth;
uniform vec4 uW;
uniform vec3 uAcc, uBg;
uniform vec4 uB[${NB}];
${GLASS}
float smin(float a, float b, float k) { float h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0); return mix(b, a, h) - k * h * (1.0 - h); }
float map(vec3 p) {
  float d = length(p) - uBodyR + 0.035 * n3(p * 1.8 + vec3(0.0, uT * 0.3, uT * 0.2));
  for (int i = 0; i < ${NB}; i++) { if (uB[i].w > 0.005) d = smin(d, length(p - uB[i].xyz) - uB[i].w, uSmooth); }
  return d;
}
vec3 nrm(vec3 p) { const vec2 e = vec2(0.002, -0.002);
  return normalize(e.xyy * map(p + e.xyy) + e.yyx * map(p + e.yyx) + e.yxy * map(p + e.yxy) + e.xxx * map(p + e.xxx)); }
void main() {
  vec2 frag = vec2(gl_FragCoord.x, uRes.y - gl_FragCoord.y);
  vec2 uv = (frag - uC) / uR; uv.y = -uv.y;
  vec3 col = uBg;
  // a soft shadow under the body, tinted where light passes through coloured glass
  float sh = exp(-pow(uv.x / 1.05, 2.0) - pow((uv.y + 1.45) / 0.13, 2.0));
  col = mix(col, mix(col * mix(0.76, 0.55, uDark), col * mix(vec3(1.0), uAcc, 0.55), 0.5 * uK), sh * 0.55);
  // and a faint glow of the accent around it while it speaks
  col = mix(col, uAcc, exp(-max(length(uv) - 1.0, 0.0) * 2.5) * uK * uW.w * (0.08 + 0.12 * uL));
  if (length(uv) < 2.4) {
    vec3 ro = vec3(uv, 3.0), rd = vec3(0.0, 0.0, -1.0);
    float t = 0.0, mh = 1e3; bool hit = false;
    for (int i = 0; i < 80; i++) {
      float h = map(ro + rd * t); mh = min(mh, h);
      if (h < 0.001) { hit = true; break; }
      t += h * 0.9; if (t > 6.0) break;
    }
    float aa = 1.2 / uR;
    if (hit) { vec3 p = ro + rd * t; col = glassShade(p, nrm(p), rd, 0.14 + 0.3 * uW.z + 0.3 * uW.w * uL); }
    else if (mh < aa) col = mix(col, mix(vec3(1.0), mix(uAcc, vec3(1.0), 0.4), uK), (1.0 - mh / aa) * mix(0.45, 0.3, uDark));
  }
  fragColor = vec4(clamp(col, 0.0, 1.0), 1.0);
}`;
// a stable direction per bead, spread over the sphere
const DIRS = Array.from({ length: NB }, (_, i) => { const y = 1 - (2 * (i + 0.5)) / NB, r = Math.sqrt(1 - y * y), a = i * 2.39996; return [Math.cos(a) * r, y * 0.8, Math.abs(Math.sin(a) * r) * 0.8 + 0.25]; });
const RAD = Array.from({ length: NB }, (_, i) => 0.2 + 0.14 * ((i * 0.618) % 1));
const ease = (t) => t * t * (3 - 2 * t);
function beads() {
  const w = S.w, out = new Float32Array(NB * 4);
  for (let i = 0; i < NB; i++) {
    const acc = [0, 0, 0, 0], add = (wt, x, y, z, r) => { acc[0] += wt * x; acc[1] += wt * y; acc[2] += wt * z; acc[3] += wt * r; };
    const d = DIRS[i], r0 = RAD[i];
    // idle: beads rest inside the surface as soft bumps; two of them slowly part and rejoin
    { const a = i * 2.4 + S.T * 0.12, part = i < 2 ? Math.pow(0.5 - 0.5 * Math.cos((S.T * 0.11 + i * 0.5) * Math.PI * 2), 3) : 0;
      const rho = 0.6 + 0.62 * part; add(w.idle, Math.cos(a) * rho * 0.95, d[1] * rho * 0.9, Math.sin(a) * rho * 0.6 + 0.1, r0 * (1 - 0.25 * part)); }
    // listening: beads rise from below, toward you, and are drawn into the body
    { const u = (S.rip * 0.06 + i / NB) % 1, e = ease(u);
      const sx = Math.sin(i * 1.7) * 0.7;
      add(w.listening, lerp(sx, sx * 0.15, e), lerp(-2.05, -0.3, e), lerp(0.9, 0.2, e), r0 * (0.55 + 0.6 * e) * clamp(u / 0.12, 0, 1)); }
    // thinking: beads circle close on a tilted ring and bridge to the body
    { const a = S.spin * 1.7 + (i / NB) * Math.PI * 2, rho = 1.02 + 0.07 * Math.sin(S.T * 1.3 + i);
      const x = Math.cos(a) * rho, z = Math.sin(a) * rho, tilt = 0.5;
      add(w.thinking, x, z * Math.sin(tilt) + 0.08 * Math.sin(a * 2), z * Math.cos(tilt), r0 * 0.85); }
    // speaking: thrown outward on the phrase, shrinking back as new beads bud
    { const u = (S.ph * 0.085 + i / NB) % 1, e = 1 - Math.pow(1 - u, 2.2), rho = 0.62 + 1.0 * e;
      add(w.speaking, d[0] * rho, d[1] * rho, d[2] * rho * 0.7, r0 * (1.05 - 0.75 * u) * clamp(u / 0.08, 0, 1)); }
    out.set(acc, i * 4);
  }
  return out;
}
const PAGE = {
  scale: 0.15,
  voicing: {
    chords: { idle: [43, 55, 62, 66, 69], listen: [40, 52, 59, 62, 67], think: [48, 55, 62, 66, 71], speak: [50, 57, 64, 66, 71], pad: [47, 54, 59, 62, 66] }, // G
    scale: [67, 69, 71, 74, 76, 79, 81, 83, 86, 88, 91],
    partials: [['sine', 1, 1], ['sine', 2.002, 0.1], ['triangle', 0.997, 0.2]],
    bell: [[1, 1, 1], [2, 0.18, 0.4], [4.07, 0.04, 0.15]], answer: 0,
    level: 0.38, filterBase: 340, filterRange: 2600, wet: 0.95, room: 3.8, sparkleEvery: 3600, home: [79, 83, 86],
  },
  setup() { this.fs = fullscreen(FRAG); },
  draw() {
    const w = S.w, L = S.level;
    const bodyR = 0.8 * (1 + 0.035 * S.breath * w.idle + 0.07 * w.speaking * L + 0.03 * w.listening * L - 0.07 * w.thinking + 0.04 * S.pulse);
    const B = beads();
    this.fs.draw((U) => {
      gl.uniform1f(U.uBodyR, bodyR);
      gl.uniform1f(U.uSmooth, 0.22 + 0.1 * w.thinking + 0.04 * w.listening);
      gl.uniform4fv(U.uB, B);
    });
  },
};
