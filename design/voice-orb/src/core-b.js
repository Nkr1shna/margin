// ====================================================== core, part two ====
const sound = new Sound(PAGE.voicing);
const live = $('#live');
const WORDS = { idle: 'Idle', listening: 'Listening', thinking: 'Thinking', speaking: 'Speaking' };
function setMode(i) {
  S.i = ((i % 4) + 4) % 4; S.mode = MODES[S.i]; S.held = 0;
  live.textContent = WORDS[S.mode];
  const map = { idle: 'idle', listening: 'listen', thinking: 'think', speaking: 'speak' };
  sound.chord(map[S.mode], S.mode === 'thinking' ? 3 : 2.2);
  if (S.mode === 'listening') sound.touch(0.3, 0.22, -0.3);
}
// a simulated voice: syllables inside phrases, with breaths between them
function voice(t, who) {
  const syl = Math.abs(Math.sin(t * (who === 'agent' ? 10.5 : 13.2) + Math.sin(t * 2.3)));
  const phrase = 0.55 + 0.45 * Math.sin(t * 1.6 + Math.sin(t * 0.55) * 2);
  const grain = 0.5 + 0.5 * Math.sin(t * 37.3) * Math.sin(t * 21.7);
  const gate = clamp((Math.sin(t * 1.15 + Math.sin(t * 0.43) * 1.8) + 0.55) * 2.2, 0, 1);
  return clamp(0.1 + syl * phrase * 0.78 + grain * 0.12, 0, 1) * gate;
}
function resize() {
  S.dpr = Math.min(devicePixelRatio || 1, 2);
  const w = innerWidth, h = innerHeight, W = Math.round(w * S.dpr), H = Math.round(h * S.dpr);
  if (canvas.width !== W || canvas.height !== H) { canvas.width = W; canvas.height = H; }
  S.W = W; S.H = H; S.x = w / 2; S.y = h / 2;
  S.r = clamp(Math.min(w, h) * (PAGE.scale || 0.165), 56, 170);
}
let last = performance.now();
function frame(now) {
  const dt = Math.min(0.05, (now - last) / 1000); last = now;
  const slow = reduce ? 0.15 : 1;
  S.clock += dt; S.held += dt;
  if (S.held > HOLD[S.mode] * (reduce ? 1.4 : 1)) setMode(S.i + 1);
  for (const m of MODES) S.w[m] = lerp(S.w[m], S.mode === m ? 1 : 0, 1 - Math.exp(-dt * 2.6));
  let target = 0;
  if (S.mode === 'listening') target = voice(S.held + 0.4, 'you');
  else if (S.mode === 'speaking') target = voice(S.held + 3.1, 'agent');
  else if (S.mode === 'thinking') target = 0.18 + 0.06 * Math.sin(S.clock * 2.1);
  S.level = lerp(S.level, target, 1 - Math.exp(-dt * (target > S.level ? 18 : 7)));
  if (reduce) S.level *= 0.5;
  const w = S.w, L = S.level;
  const kT = INTENSITY.idle * w.idle + INTENSITY.listening * w.listening + INTENSITY.thinking * w.thinking + INTENSITY.speaking * w.speaking;
  S.K = lerp(S.K, kT, 1 - Math.exp(-dt * 3));
  // each motion has its own clock, integrated so speeds can change without jumps
  S.T += dt * slow * (0.8 + 0.3 * w.thinking);
  S.spin += dt * slow * (0.12 + 0.9 * w.thinking);
  S.rip += dt * slow * (2.4 + 9 * L) * (0.3 + w.listening);
  S.ph += dt * slow * (1.5 + 11 * L) * (0.3 + w.speaking);
  S.breathT += dt * slow * (0.9 + w.thinking * 0.8); S.breath = Math.sin(S.breathT * 1.25) * 0.5 + 0.5;
  S.pulse = Math.max(0, S.pulse - dt * 1.4);
  const bg = BG[isDark() ? 'dark' : 'light'], kb = 1 - Math.exp(-dt * 4);
  for (let i = 0; i < 3; i++) S.bg[i] = lerp(S.bg[i], bg[i], kb);
  S.dark = lerp(S.dark, isDark() ? 1 : 0, kb);
  if (sound.ctx) sound.brightness(S.mode === 'listening' ? 0.22 + L * 0.7 : S.mode === 'speaking' ? 0.42 + L * 0.35 : S.mode === 'thinking' ? 0.34 : 0.2, S.mode === 'listening' ? 0.06 : 0.3);
  resize();
  gl.viewport(0, 0, S.W, S.H);
  gl.clearColor(S.bg[0], S.bg[1], S.bg[2], 1); gl.clear(gl.COLOR_BUFFER_BIT);
  PAGE.draw(dt);
  requestAnimationFrame(frame);
}

// --------------------------------------------------------------- input ----
// Sound starts on the first tap or key press and fades in.
let soundOn = false;
function startSound() { if (soundOn) return; soundOn = true; sound.start(); sound.chord('idle', 0.1); sound.setEnabled(true, 6); }
canvas.addEventListener('pointerdown', (e) => {
  startSound();
  const dx = e.clientX - S.x, dy = e.clientY - S.y;
  if (Math.hypot(dx, dy) < S.r * 1.9) { setMode(S.i + 1); S.pulse = 1; sound.touch(0.5, 0.4, clamp(dx / S.r / 3, -1, 1)); }
});
addEventListener('keydown', (e) => {
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  startSound();
  if (e.key === 't' || e.key === 'T') { root.dataset.theme = isDark() ? 'light' : 'dark'; return; }
  const n = '1234'.indexOf(e.key);
  if (n >= 0) { const here = Math.max(0, PAGES.findIndex((p) => location.pathname.endsWith('/' + p))); if (n !== here) location.href = PAGES[n] + (root.dataset.theme ? '#' + root.dataset.theme : ''); }
});
document.addEventListener('visibilitychange', () => sound.hidden(document.hidden));
PAGE.setup();
setMode(0);
requestAnimationFrame(frame);
