// ============================================================== core ====
const root = document.documentElement;
const $ = (s) => document.querySelector(s);
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const lerp = (a, b, t) => a + (b - a) * t;
const h2r = (h) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16) / 255);

// The one accent. Change this line to recolour the object.
const ACCENT = '#c41a1e';
const ACC = h2r(ACCENT);
const BG = { light: h2r('#ebebea'), dark: h2r('#111214') };
// key 1 is the main page: index.html on disk, the page's own root once published
const PAGES = [location.protocol === 'file:' ? 'index.html' : './', 'option-droplet.html', 'option-medusa.html', 'option-silk.html'];

const reduceMQ = matchMedia('(prefers-reduced-motion: reduce)');
let reduce = reduceMQ.matches; reduceMQ.addEventListener('change', (e) => { reduce = e.matches; });
const darkMQ = matchMedia('(prefers-color-scheme: dark)');
if (location.hash === '#dark' || location.hash === '#light') root.dataset.theme = location.hash.slice(1);
const isDark = () => (root.dataset.theme ? root.dataset.theme === 'dark' : darkMQ.matches);

// ---------------------------------------------------------------- state ----
const MODES = ['idle', 'listening', 'thinking', 'speaking'];
const HOLD = { idle: 5.5, listening: 6.5, thinking: 5, speaking: 6.5 };   // seconds in each mode
const INTENSITY = { idle: 0, listening: 0.62, thinking: 0.78, speaking: 1 }; // how much of the accent it holds
const S = {
  i: 0, mode: 'idle', held: 0, w: { idle: 1, listening: 0, thinking: 0, speaking: 0 },
  level: 0, K: 0, T: 0, spin: 0, rip: 0, ph: 0, breath: 0, breathT: 0, pulse: 0, clock: 0,
  bg: [...BG[isDark() ? 'dark' : 'light']], dark: isDark() ? 1 : 0,
  x: 0, y: 0, r: 100, dpr: 1, W: 1, H: 1,
};

// ------------------------------------------------------------------ GL ----
const canvas = $('#stage');
const gl = canvas.getContext('webgl2', { antialias: true, alpha: false, powerPreference: 'high-performance' });
function program(vs, fs) {
  const sh = (type, src) => { const s = gl.createShader(type); gl.shaderSource(s, src); gl.compileShader(s); if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) console.error(gl.getShaderInfoLog(s)); return s; };
  const p = gl.createProgram(); gl.attachShader(p, sh(gl.VERTEX_SHADER, vs)); gl.attachShader(p, sh(gl.FRAGMENT_SHADER, fs)); gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) console.error(gl.getProgramInfoLog(p));
  return p;
}
function uniformsOf(p) {
  const U = {}, n = gl.getProgramParameter(p, gl.ACTIVE_UNIFORMS);
  for (let i = 0; i < n; i++) { const name = gl.getActiveUniform(p, i).name; U[name.replace(/\[0\]$/, '')] = gl.getUniformLocation(p, name); }
  return U;
}
// the uniforms every study can read
function setCommon(U) {
  const f = (k, v) => { if (U[k]) gl.uniform1f(U[k], v); };
  if (U.uRes) gl.uniform2f(U.uRes, S.W, S.H);
  if (U.uC) gl.uniform2f(U.uC, S.x * S.dpr, S.y * S.dpr);
  f('uR', S.r * S.dpr); f('uT', S.T); f('uL', S.level); f('uDark', S.dark); f('uK', S.K);
  f('uSpin', S.spin); f('uRip', S.rip); f('uPh', S.ph); f('uBreath', S.breath); f('uPulse', S.pulse); f('uClock', S.clock);
  if (U.uW) gl.uniform4f(U.uW, S.w.idle, S.w.listening, S.w.thinking, S.w.speaking);
  if (U.uAcc) gl.uniform3f(U.uAcc, ACC[0], ACC[1], ACC[2]);
  if (U.uBg) gl.uniform3f(U.uBg, S.bg[0], S.bg[1], S.bg[2]);
}
const FS_VERT = `#version 300 es
in vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }`;
// a full-screen pass for a fragment shader
function fullscreen(frag) {
  const prog = program(FS_VERT, frag), U = uniformsOf(prog);
  const vao = gl.createVertexArray(); gl.bindVertexArray(vao);
  const buf = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, buf); gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
  const loc = gl.getAttribLocation(prog, 'p'); gl.enableVertexAttribArray(loc); gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
  gl.bindVertexArray(null);
  return { prog, U, draw(extra) { gl.useProgram(prog); gl.bindVertexArray(vao); setCommon(U); if (extra) extra(U); gl.drawArrays(gl.TRIANGLES, 0, 3); gl.bindVertexArray(null); } };
}
