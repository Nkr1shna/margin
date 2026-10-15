// =============================================================== Silk ====
// Several hundred fine silk threads, each an arc around an unseen sphere, lit like hair.
// Idle: a loose, slowly drifting cloud of silver threads. Listening: the threads stream toward
// you and tremble with your voice. Thinking: they draw in and wind into rings that turn on one
// tilted axis. Speaking: they gather tight into a ruby sphere that swells and sends pulses
// outward on each syllable. Every mode is its own shape; the threads glide from one to the next.
const NT = 560, NP = 44;
const VERT = `#version 300 es
precision highp float;
in vec2 a;                       // thread index, position along the thread (0..1)
uniform vec2 uRes, uC;
uniform float uR, uT, uL, uSpin, uRip, uPh, uBreath, uPulse, uClock;
uniform vec4 uW;
out float vS, vDepth, vSpec, vDiff;
float h1(float n) { return fract(sin(n * 127.1 + 0.3) * 43758.5453); }
vec3 rotAx(vec3 v, vec3 k, float a) { return v * cos(a) + cross(k, v) * sin(a) + k * dot(k, v) * (1.0 - cos(a)); }
const vec3 YOU = vec3(0.0, -0.72, 0.69);
vec3 pos(float id, float s) {
  float u1 = h1(id), u2 = h1(id + 17.0), u3 = h1(id + 41.0);
  // a start direction and a turning axis for this thread
  float z = 1.0 - 2.0 * fract(id * 0.618034 + 0.1), r = sqrt(1.0 - z * z), ph = id * 2.39996;
  vec3 d0 = vec3(cos(ph) * r, z, sin(ph) * r);
  vec3 ax = normalize(cross(d0, normalize(vec3(u1 - 0.5, u2 - 0.5, u3 - 0.5) + vec3(0.0, 0.0, 0.001))));
  float len = 0.9 + 1.3 * u2;
  float sc = (s - 0.5) * len;
  // idle: a loose cloud, each thread drifting on its own arc
  vec3 pI = rotAx(d0, ax, sc + uT * (0.08 + 0.1 * u1));
  pI *= 1.08 + 0.28 * u3 + 0.07 * sin(uT * 0.7 + id) + 0.05 * uBreath;
  pI += 0.045 * vec3(sin(s * 5.0 + uT + id), cos(s * 4.0 - uT * 0.8 + id * 1.3), sin(s * 3.0 + uT * 0.6 + id * 0.7));
  // listening: threads flow along great circles that all pass through the point facing you
  vec3 axL = normalize(cross(YOU, d0 + vec3(0.0, 0.0, 0.0001)));
  float fl = fract(u1 + uRip * 0.05);
  vec3 pL = rotAx(YOU, axL, mix(2.6, 0.15, fl) + (s - 0.5) * len * 0.55);
  pL *= 1.0 + 0.06 * sin(uRip * 0.4 + id) + uL * 0.05 * sin(s * 40.0 + uClock * 19.0 + id * 3.0);
  // thinking: rings on one tilted axis, turning, drawn in
  vec3 axT = normalize(vec3(0.35, 1.0, 0.2));
  vec3 base = normalize(d0 - axT * dot(d0, axT) * 0.55);
  vec3 pT = rotAx(base, axT, sc * 1.6 + uSpin * (1.3 + 0.8 * u1)) * (0.78 + 0.1 * u3);
  // speaking: gathered into a sphere, swelling, with pulses travelling outward from the front
  vec3 pS = rotAx(d0, ax, sc * 0.9 + uT * 0.05);
  float fromFront = acos(clamp(pS.z, -1.0, 1.0));
  pS *= 0.9 + 0.1 * uL + 0.035 * sin(fromFront * 5.0 - uPh) * (0.4 + uL) + 0.02 * uPulse;
  return pI * uW.x + pL * uW.y + pT * uW.z + pS * uW.w;
}
vec2 project(vec3 p) {
  // a slow turn of the whole form, and a little perspective
  float a = uT * 0.07; p.xz = mat2(cos(a), sin(a), -sin(a), cos(a)) * p.xz;
  float k = 1.0 / (1.0 - p.z * 0.16);
  vec2 px = uC + vec2(p.x, -p.y) * uR * k;
  return px / uRes * 2.0 - 1.0;
}
void main() {
  float id = a.x, s = a.y;
  vec3 p = pos(id, s), q = pos(id, s + 0.01);
  vec3 T = normalize(q - p + 1e-5);
  vec2 c = project(p);
  gl_Position = vec4(c.x, -c.y, 0.0, 1.0);
  vS = s;
  vDepth = clamp(0.5 + 0.5 * p.z, 0.0, 1.0);
  // hair lighting: brightest where the thread runs across the light
  vec3 H = normalize(normalize(vec3(-0.5, 0.65, 0.6)) + vec3(0.0, 0.0, 1.0));
  float th = dot(T, H);
  vSpec = pow(sqrt(max(0.0, 1.0 - th * th)), 60.0);
  float tl = dot(T, normalize(vec3(-0.5, 0.65, 0.6)));
  vDiff = sqrt(max(0.0, 1.0 - tl * tl));
}`;
const FRAG_LINES = `#version 300 es
precision highp float;
in float vS, vDepth, vSpec, vDiff;
uniform float uK, uDark;
uniform vec3 uAcc;
out vec4 o;
void main() {
  float ends = smoothstep(0.0, 0.12, vS) * smoothstep(1.0, 0.88, vS);
  vec3 silver = mix(vec3(0.56, 0.58, 0.62), vec3(0.78, 0.8, 0.84), uDark);
  vec3 ruby = mix(uAcc * 0.75, uAcc * 1.15, uDark);
  vec3 c = mix(silver, ruby, uK) * (0.55 + 0.45 * vDiff);
  c += mix(vec3(1.0), mix(uAcc, vec3(1.0), 0.6), uK) * vSpec * mix(0.55, 0.9, uDark);
  float a = ends * mix(0.3, 0.95, vDepth) * mix(mix(0.42, 0.6, uK), 0.42, uDark);
  o = vec4(c * a, a);   // premultiplied
}`;
// a soft glow and contact shadow under the form
const FRAG_BG = `#version 300 es
precision highp float;
out vec4 fragColor;
uniform vec2 uRes, uC;
uniform float uR, uK, uDark, uL;
uniform vec4 uW;
uniform vec3 uAcc, uBg;
void main() {
  vec2 frag = vec2(gl_FragCoord.x, uRes.y - gl_FragCoord.y);
  vec2 uv = (frag - uC) / uR; uv.y = -uv.y;
  vec3 col = uBg;
  float d = length(uv);
  col = mix(col, uAcc, exp(-d * d * 1.6) * uK * (0.07 + 0.08 * uW.w * uL) * (1.0 + 0.6 * uDark));
  float sh = exp(-pow(uv.x / 1.0, 2.0) - pow((uv.y + 1.55) / 0.11, 2.0));
  col = mix(col, col * mix(vec3(0.8), mix(vec3(0.85), uAcc, 0.35), uK) * mix(1.0, 0.7, uDark), sh * 0.5);
  fragColor = vec4(col, 1.0);
}`;
const PAGE = {
  scale: 0.2,
  voicing: {
    chords: { idle: [40, 52, 59, 63, 66], listen: [45, 52, 57, 61, 66], think: [42, 54, 58, 61, 66], speak: [47, 54, 59, 63, 68], pad: [44, 51, 56, 59, 63] }, // E, airy
    scale: [64, 66, 68, 71, 73, 76, 78, 80, 83, 85, 88],
    partials: [['sine', 1, 1], ['triangle', 1.005, 0.2], ['sine', 2.002, 0.08]],
    bell: [[1, 1, 1], [2.76, 0.16, 0.35], [5.4, 0.05, 0.12]], answer: 7,
    level: 0.36, filterBase: 520, filterRange: 3600, wet: 0.95, room: 3.6, sparkleEvery: 3000, home: [76, 80, 83, 88],
  },
  setup() {
    this.bg = fullscreen(FRAG_BG);
    this.prog = program(VERT, FRAG_LINES); this.U = uniformsOf(this.prog);
    const v = new Float32Array(NT * NP * 2), idx = new Uint32Array(NT * (NP - 1) * 2);
    let k = 0, m = 0;
    for (let i = 0; i < NT; i++) for (let j = 0; j < NP; j++) { v[k++] = i; v[k++] = j / (NP - 1); if (j < NP - 1) { idx[m++] = i * NP + j; idx[m++] = i * NP + j + 1; } }
    this.vao = gl.createVertexArray(); gl.bindVertexArray(this.vao);
    const b = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, b); gl.bufferData(gl.ARRAY_BUFFER, v, gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(this.prog, 'a'); gl.enableVertexAttribArray(loc); gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    const ib = gl.createBuffer(); gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, ib); gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, idx, gl.STATIC_DRAW);
    gl.bindVertexArray(null); this.count = idx.length;
  },
  draw() {
    this.bg.draw();
    gl.enable(gl.BLEND);
    // threads add light on a dark ground, and lay over a light one
    if (S.dark > 0.5) gl.blendFunc(gl.ONE, gl.ONE); else gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    gl.useProgram(this.prog); gl.bindVertexArray(this.vao); setCommon(this.U);
    gl.drawElements(gl.LINES, this.count, gl.UNSIGNED_INT, 0);
    gl.bindVertexArray(null); gl.disable(gl.BLEND);
  },
};
