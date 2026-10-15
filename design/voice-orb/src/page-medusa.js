// ============================================================= Medusa ====
// A translucent glass bell with trailing filaments, like a jellyfish seen in still water.
// Idle: a slow pulse, the filaments drifting. Listening: the bell tips toward you and the
// filaments sway closer, trembling with your voice. Thinking: the bell draws in and turns, and
// the filaments wind into a slow spiral. Speaking: it contracts on each syllable and pulses of
// light run down the filaments. The membrane holds more of the accent the more active it is.
const FRAG = `#version 300 es
precision highp float;
out vec4 fragColor;
uniform vec2 uRes, uC;
uniform float uR, uT, uL, uDark, uK, uSpin, uPh, uRip, uPulse, uQ, uTilt, uBob, uClockF;
uniform vec4 uW;
uniform vec3 uAcc, uBg;
${GLASS}
float hash1(float n) { return fract(sin(n * 127.1) * 43758.5453); }
// the bell: a hollow, slightly scalloped dome; uQ squeezes it (a contraction)
vec3 bellSpace(vec3 p) {
  p.y -= 0.3 + uBob;
  p.yz = rot(uTilt) * p.yz;
  p.xz = rot(uSpin * 0.6) * p.xz;
  return p;
}
float ell(vec3 p, vec3 r) { return (length(p / r) - 1.0) * min(r.x, min(r.y, r.z)); }
float bell(vec3 p) {
  vec3 q = bellSpace(p);
  float sx = 1.0 - 0.17 * uQ, sy = 0.78 + 0.12 * uQ;
  float a = atan(q.z, q.x);
  float outer = ell(q, vec3(sx, sy, sx) * (1.0 + 0.012 * sin(a * 16.0)));
  float inner = ell(q + vec3(0.0, 0.2, 0.0), vec3(sx, sy, sx) * 0.88);
  float rim = -0.46 * sy + 0.05 * sin(a * 8.0 + uT * 0.7) + 0.025 * sin(a * 21.0 - uT * 0.5);
  return max(max(outer, -inner), rim - q.y);
}
vec3 bellN(vec3 p) { const vec2 e = vec2(0.002, -0.002);
  return normalize(e.xyy * bell(p + e.xyy) + e.yyx * bell(p + e.yyx) + e.yxy * bell(p + e.yxy) + e.xxx * bell(p + e.xxx)); }

// one filament, hanging from the rim: distance from the pixel to it, and how far along it is
vec2 filament(vec2 uv, float k, float n, float y0, float sx, float arm) {
  float fx = (k / (n - 1.0) * 2.0 - 1.0) * 0.78 * sx * (arm > 0.5 ? 0.25 : 1.0);
  float len = (arm > 0.5 ? 1.05 : 1.1) + 0.8 * hash1(k * 1.37 + arm * 7.0);
  float s = y0 - uv.y;
  if (s < 0.0 || s > len) return vec2(1e3, s / len);
  float sway = (0.06 + 0.05 * hash1(k * 2.3)) * sin(s * (1.6 + hash1(k)) - uT * 1.1 + k * 1.7) * s;
  sway += uW.y * (0.06 * s * sin(uT * 0.8 + k) + 0.035 * uL * sin(s * 24.0 - uClockF * 11.0 + k * 3.0) * s);
  float x = mix(fx * (1.0 - 0.18 * s), fx * cos(uSpin * 1.8 + s * 2.6 + k) * (1.0 - 0.1 * s), uW.z) + sway;
  return vec2(abs(uv.x - x), s / len);
}
void main() {
  vec2 frag = vec2(gl_FragCoord.x, uRes.y - gl_FragCoord.y);
  vec2 uv = (frag - uC) / uR; uv.y = -uv.y;
  vec3 col = uBg;
  float sx = 1.0 - 0.17 * uQ, sy = 0.78 + 0.12 * uQ;
  float y0 = 0.3 + uBob - 0.12 * sy;   // filaments start inside the bell, seen through it
  // a faint glow of the accent where it is warm
  col = mix(col, uAcc, exp(-length(uv - vec2(0.0, 0.2)) * 2.2) * uK * (0.06 + 0.12 * uW.w * uL) * (1.0 - 0.5 * uDark));
  // filaments: fine glassy threads, and three wider frilled arms in the middle
  float px = 1.0 / uR;
  for (int i = 0; i < 19; i++) {
    float arm = i < 16 ? 0.0 : 1.0;
    float k = arm > 0.5 ? float(i - 16) : float(i);
    vec2 f = filament(uv, k, arm > 0.5 ? 3.0 : 16.0, y0, sx, arm);
    if (f.x > 0.2) continue;
    float s = f.y;
    float wdt = arm > 0.5 ? (0.045 + 0.025 * sin(s * 16.0 - uT * 1.5 + k)) * (1.0 - 0.55 * s) : (0.006 + 0.006 * hash1(k * 3.1) + 0.007 * (1.0 - s));
    float a = smoothstep(wdt + px * 1.2, wdt - px * 0.6, f.x) * (1.0 - smoothstep(0.55, 1.0, s)) * smoothstep(0.0, 0.12, s);
    float core = smoothstep(wdt * 0.6, 0.0, f.x);
    float travel = pow(0.5 + 0.5 * sin(s * 9.0 - uPh * 1.3 + k), 6.0) * uW.w * (0.4 + 1.2 * uL);
    vec3 silverC = mix(vec3(0.62, 0.64, 0.68), vec3(0.75, 0.77, 0.8), uDark);
    vec3 c = mix(silverC, uAcc * (arm > 0.5 ? 0.85 : 1.0), uK);
    c += mix(vec3(1.0), mix(uAcc, vec3(1.0), 0.5), uK) * (0.35 * core + travel);
    col = mix(col, c, a * (arm > 0.5 ? 0.55 : 0.75));
  }
  // the bell, raymarched: a translucent membrane, brighter at grazing angles, with radial canals
  if (length(uv - vec2(0.0, 0.3 + uBob)) < 1.35) {
    vec3 ro = vec3(uv, 3.0), rd = vec3(0.0, 0.0, -1.0);
    float t = 0.0, mh = 1e3; bool hit = false;
    for (int i = 0; i < 72; i++) { float h = bell(ro + rd * t); mh = min(mh, h); if (h < 0.001) { hit = true; break; } t += h * 0.9; if (t > 6.0) break; }
    if (hit) {
      vec3 p = ro + rd * t, n = bellN(p), q = bellSpace(p);
      float ndv = clamp(dot(n, -rd), 0.0, 1.0);
      vec3 g = glassShade(p, n, rd, 0.1 + 0.25 * uW.z + 0.25 * uW.w * uL);
      float a = atan(q.z, q.x);
      float canals = pow(abs(cos(a * 4.0)), 60.0) * smoothstep(-0.3, 0.6, q.y) * 0.5;
      float rimGlow = exp(-pow((q.y + 0.28 * sy) / 0.05, 2.0));
      float alpha = clamp(0.26 + 0.62 * pow(1.0 - ndv, 1.8) + 0.42 * uK * (1.0 - 0.3 * uDark), 0.0, 0.95);
      vec3 c = g + mix(vec3(1.0), mix(uAcc, vec3(1.0), 0.5), uK) * (canals + 0.5 * rimGlow) * 0.6;
      // the inner glow: the bell is lit from inside as it speaks
      c += uAcc * uK * (0.12 + 0.35 * uW.w * uL) * ndv;
      col = mix(col, c, alpha);
    } else if (mh < 1.2 / uR) col = mix(col, vec3(1.0), (1.0 - mh * uR / 1.2) * 0.25);
  }
  fragColor = vec4(clamp(col, 0.0, 1.0), 1.0);
}`;
let q = 0;
const PAGE = {
  scale: 0.19,
  voicing: {
    chords: { idle: [38, 50, 57, 64, 66], listen: [35, 47, 54, 59, 62], think: [43, 50, 57, 62, 64], speak: [45, 52, 57, 61, 64], pad: [40, 52, 55, 59, 62] }, // D, a slow sea
    scale: [62, 64, 66, 69, 71, 74, 76, 78, 81, 83],
    partials: [['sine', 1, 1], ['sine', 2.001, 0.12], ['triangle', 0.998, 0.18]],
    bell: [[1, 1, 1], [2.76, 0.14, 0.35], [5.4, 0.04, 0.12]], answer: 7,
    level: 0.4, filterBase: 300, filterRange: 2300, wet: 1, room: 4.4, sparkleEvery: 5000, home: [74, 78, 81],
  },
  setup() { this.fs = fullscreen(FRAG); },
  draw(dt) {
    const w = S.w, L = S.level;
    // the contraction: a slow pulse at rest, held in while thinking, on every syllable while speaking
    const idleQ = Math.pow(0.5 - 0.5 * Math.cos(S.T * 1.3), 3) * 0.7;
    const target = idleQ * (w.idle + 0.5 * w.listening) + 0.45 * w.thinking + (0.15 + 0.75 * L) * w.speaking + 0.25 * S.pulse;
    q = lerp(q, target, 1 - Math.exp(-dt * (target > q ? 10 : 3)));
    this.fs.draw((U) => {
      gl.uniform1f(U.uQ, q);
      gl.uniform1f(U.uTilt, 0.28 + 0.3 * w.listening - 0.1 * w.thinking);
      gl.uniform1f(U.uBob, 0.06 * q - 0.04 * w.listening);
      gl.uniform1f(U.uClockF, S.clock);
    });
  },
};
