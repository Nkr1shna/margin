// Generative sound, live Web Audio (no files): a five-voice pad whose chord glides between modes,
// a filter that opens with the voice, soft bells, and music-box notes from the chord.
const hz = (n) => 440 * Math.pow(2, (n - 69) / 12);
class Sound {
  constructor(v) { this.v = v; this.ctx = null; this.enabled = false; this.chordName = 'idle'; this.voices = []; this.last = 0; this.bright = -1; this.timer = 0; }
  get running() { return !!this.ctx && this.ctx.state === 'running'; }
  start() {
    if (this.ctx) { this.ctx.resume(); return; }
    const ctx = (this.ctx = new (window.AudioContext || window.webkitAudioContext)());
    const comp = ctx.createDynamicsCompressor(); comp.threshold.value = -18; comp.ratio.value = 3; comp.connect(ctx.destination);
    this.master = ctx.createGain(); this.master.gain.value = 0; this.master.connect(comp);
    const verb = ctx.createConvolver(); verb.buffer = room(ctx, this.v.room, 2.4);
    const wet = ctx.createGain(); wet.gain.value = this.v.wet; verb.connect(wet).connect(this.master);
    this.send = ctx.createGain(); this.send.connect(verb);
    this.filter = ctx.createBiquadFilter(); this.filter.type = 'lowpass'; this.filter.frequency.value = this.v.filterBase; this.filter.Q.value = 0.65;
    const dry = ctx.createGain(); dry.gain.value = 0.52; this.filter.connect(dry).connect(this.master); this.filter.connect(this.send);
    this.v.chords[this.chordName].forEach((note, i) => {
      const g = ctx.createGain(); g.gain.value = i === 0 ? 0.11 : 0.072; const oscs = [];
      for (const [type, det, lvl] of this.v.partials) {
        const o = ctx.createOscillator(); o.type = type; o.frequency.value = hz(note) * det;
        const gg = ctx.createGain(); gg.gain.value = lvl; o.connect(gg).connect(g); o.start(); oscs.push([o, det]);
      }
      const lfo = ctx.createOscillator(); lfo.frequency.value = 0.04 + i * 0.019;
      const depth = ctx.createGain(); depth.gain.value = 0.03; lfo.connect(depth).connect(g.gain); lfo.start();
      g.connect(this.filter); this.voices.push(oscs);
    });
    this.sparkle();
  }
  setEnabled(on, fade = 6) {
    this.enabled = on; if (!this.ctx) return; if (on) this.ctx.resume();
    const t = this.ctx.currentTime, g = this.master.gain;
    g.cancelScheduledValues(t); g.setValueAtTime(g.value, t); g.linearRampToValueAtTime(on ? this.v.level : 0, t + (on ? fade : 0.35));
    clearTimeout(this.sus); if (!on) { const ctx = this.ctx; this.sus = setTimeout(() => { if (!this.enabled) ctx.suspend(); }, 500); }
  }
  hidden(h) { if (!this.ctx) return; if (h) this.ctx.suspend(); else if (this.enabled) this.ctx.resume(); }
  chord(name, glide = 2.2) {
    if (this.chordName === name) return; this.chordName = name; if (!this.ctx) return;
    const t = this.ctx.currentTime;
    this.v.chords[name].forEach((n, i) => this.voices[i].forEach(([o, det]) => o.frequency.setTargetAtTime(hz(n) * det, t, glide / 3)));
  }
  brightness(v, tc = 0.25) {
    if (!this.ctx) return; v = clamp(v, 0, 1); if (Math.abs(v - this.bright) < 0.008) return; this.bright = v;
    this.filter.frequency.setTargetAtTime(this.v.filterBase + v * v * this.v.filterRange, this.ctx.currentTime, tc);
  }
  out(node, pan, dryAmt = 0.48) { const p = this.ctx.createStereoPanner(); p.pan.value = clamp(pan, -1, 1); node.connect(p); const d = this.ctx.createGain(); d.gain.value = dryAmt; p.connect(d).connect(this.master); p.connect(this.send); }
  bell(f, vel = 0.5, pan = 0, gap = 0.05, decay = 2.6) {
    if (!this.ctx || !this.enabled) return; const ctx = this.ctx, t = ctx.currentTime;
    if (t - this.last < gap) return; this.last = t;
    const env = ctx.createGain(); env.gain.setValueAtTime(0, t); env.gain.linearRampToValueAtTime(0.09 * clamp(vel, 0.05, 1), t + 0.008); env.gain.exponentialRampToValueAtTime(0.0001, t + decay);
    for (const [r, l, d] of this.v.bell) {
      const o = ctx.createOscillator(); o.type = 'sine'; o.frequency.value = f * r;
      const g = ctx.createGain(); g.gain.setValueAtTime(l, t); g.gain.exponentialRampToValueAtTime(0.001, t + decay * d);
      o.connect(g).connect(env); o.start(t); o.stop(t + decay + 0.1);
    }
    this.out(env, pan);
  }
  touch(pos, vel = 0.4, pan = 0) { const s = this.v.scale; this.bell(hz(s[clamp(Math.floor(pos * s.length), 0, s.length - 1)]), vel, pan); }
  chordNote(step, vel = 0.4, pan = 0) {
    const c = this.v.chords[this.chordName]; const n = c[1 + (step % 4)] + 24;
    this.bell(hz(n), vel, pan, 0);
    if (this.v.answer) setTimeout(() => this.bell(hz(n + this.v.answer), vel * 0.55, -pan, 0), 130);
  }
  sparkle() {
    clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      if (this.enabled && this.ctx && Math.random() < 0.75) {
        const c = this.v.chords[this.chordName]; const pick = () => hz(c[1 + Math.floor(Math.random() * 4)] + 24);
        this.bell(pick(), 0.12 + Math.random() * 0.1, Math.random() * 1.2 - 0.6, 0, 3);
        if (Math.random() < 0.3) setTimeout(() => this.bell(pick(), 0.09, Math.random() - 0.5, 0, 3), 260 + Math.random() * 240);
      }
      this.sparkle();
    }, this.v.sparkleEvery * (0.7 + Math.random() * 0.9));
  }
  // air moving: a filtered breath of noise, rising or falling
  breath(strength = 0.5, up = true, q = 0.8) {
    if (!this.ctx || !this.enabled) return; const ctx = this.ctx, t = ctx.currentTime;
    const src = ctx.createBufferSource(); src.buffer = noise(ctx, 2.6);
    const bp = ctx.createBiquadFilter(); bp.type = 'bandpass'; bp.Q.value = q;
    bp.frequency.setValueAtTime(up ? 480 : 2400, t); bp.frequency.exponentialRampToValueAtTime(up ? 2400 : 480, t + 1.1);
    const env = ctx.createGain(); env.gain.setValueAtTime(0, t); env.gain.linearRampToValueAtTime(0.032 * strength, t + 0.7); env.gain.exponentialRampToValueAtTime(0.0001, t + 2.3);
    src.connect(bp).connect(env); env.connect(this.send); env.connect(this.master); src.start(t);
  }
  // a water drop: a sine that falls quickly in pitch
  drop(vel = 0.5, pan = 0, pitch = 1) {
    if (!this.ctx || !this.enabled) return; const ctx = this.ctx, t = ctx.currentTime;
    const o = ctx.createOscillator(); o.type = 'sine';
    o.frequency.setValueAtTime(1500 * pitch, t); o.frequency.exponentialRampToValueAtTime(420 * pitch, t + 0.09); o.frequency.exponentialRampToValueAtTime(700 * pitch, t + 0.16);
    const env = ctx.createGain(); env.gain.setValueAtTime(0, t); env.gain.linearRampToValueAtTime(0.1 * vel, t + 0.005); env.gain.exponentialRampToValueAtTime(0.0001, t + 0.22);
    o.connect(env); o.start(t); o.stop(t + 0.3); this.out(env, pan, 0.6);
  }
  // the chord comes home with a short falling-into-place phrase
  resolve() {
    this.chord('idle', 1.4);
    this.v.home.forEach((n, i) => setTimeout(() => this.bell(hz(n), 0.3 - i * 0.05, (i - 1) * 0.35, 0, 3), 260 + i * 150));
  }
}
function noise(ctx, s) { const b = ctx.createBuffer(1, Math.floor(ctx.sampleRate * s), ctx.sampleRate); const d = b.getChannelData(0); for (let i = 0; i < d.length; i++) d[i] = Math.random() * 2 - 1; return b; }
function room(ctx, s, decay) { const n = Math.floor(ctx.sampleRate * s), b = ctx.createBuffer(2, n, ctx.sampleRate); for (let c = 0; c < 2; c++) { const d = b.getChannelData(c); for (let i = 0; i < n; i++) d[i] = (Math.random() * 2 - 1) * Math.pow(1 - i / n, decay); } return b; }
