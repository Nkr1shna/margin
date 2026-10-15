const FRAG = `#version 300 es
precision highp float;
out vec4 fragColor;
uniform vec2 uRes, uC;
uniform float uR, uT, uL, uDark, uAir, uPulse, uQuiet, uDock, uBreath, uK, uSpin, uRip, uPh;
uniform vec4 uW;          // idle, listening, thinking, speaking
uniform vec3 uAcc, uBg;

vec3 g3(vec3 p) { p = fract(p * vec3(0.1031, 0.1030, 0.0973)); p += dot(p, p.yxz + 33.33); return fract((p.xxy + p.yxx) * p.zyx) * 2.0 - 1.0; }
float n3(vec3 p) { vec3 i = floor(p), f = fract(p), u = f * f * (3.0 - 2.0 * f);
  return mix(mix(mix(dot(g3(i), f), dot(g3(i + vec3(1, 0, 0)), f - vec3(1, 0, 0)), u.x),
                 mix(dot(g3(i + vec3(0, 1, 0)), f - vec3(0, 1, 0)), dot(g3(i + vec3(1, 1, 0)), f - vec3(1, 1, 0)), u.x), u.y),
             mix(mix(dot(g3(i + vec3(0, 0, 1)), f - vec3(0, 0, 1)), dot(g3(i + vec3(1, 0, 1)), f - vec3(1, 0, 1)), u.x),
                 mix(dot(g3(i + vec3(0, 1, 1)), f - vec3(0, 1, 1)), dot(g3(i + vec3(1, 1, 1)), f - vec3(1, 1, 1)), u.x), u.y), u.z); }
mat2 rot(float a) { float c = cos(a), s = sin(a); return mat2(c, s, -s, c); }
const vec3 YOU = vec3(0.0, -0.75, 0.66);   // the point on the pearl that faces you, below and in front

// The surface. Positive values pull it in, negative push it out.
float disp(vec3 p) {
  float lis = uW.y, thk = uW.z, spk = uW.w;
  vec3 dir = normalize(p);
  // folds that turn around the pearl; thinking turns them faster and folds deeper
  vec3 q = p;
  q.xz = rot(q.y * (1.15 + 0.8 * thk) + uSpin) * q.xz;
  q.xy = rot(0.42) * q.xy;
  float folds = sin(atan(q.z, q.x) * 3.0 + q.y * 2.1 + uT * 0.33 + 1.2 * n3(p * 0.8 + uT * 0.06));
  float flow = n3(p * 1.1 + vec3(0.0, uT * 0.12, uT * 0.08));
  float d = folds * (0.05 + 0.02 * uBreath * uW.x + 0.055 * thk) + flow * (0.045 + 0.025 * thk);
  // idle: a slow breath
  d -= 0.02 * uBreath * uW.x;
  // listening: ripples drawn in toward you, and a fine shimmer that follows your voice
  float rr = acos(clamp(dot(dir, normalize(YOU)), -1.0, 1.0));
  d += lis * (sin(rr * 9.0 + uRip) * exp(-rr * 0.55) * (0.014 + 0.026 * uL) + n3(p * 7.0 + vec3(uT * 2.2, -uT * 1.7, uT * 1.3)) * 0.014 * uL);
  // thinking: it draws in
  d += 0.04 * thk;
  // speaking: pulses leave the centre and travel outward, and it bulges on each syllable
  float rf = acos(clamp(dir.z, -1.0, 1.0));
  d += spk * (sin(rf * 6.0 - uPh) * exp(-rf * 0.35) * (0.01 + 0.03 * uL) - 0.065 * uL);
  return d;
}
float map(vec3 p) { return (length(p) - 1.0 + disp(p)) * 0.62; }
vec3 nrm(vec3 p) { const vec2 e = vec2(0.003, -0.003);
  return normalize(e.xyy * map(p + e.xyy) + e.yyx * map(p + e.yyx) + e.yxy * map(p + e.yxy) + e.xxx * map(p + e.xxx)); }

// a soft studio: pale sky, two softboxes, and a bounce from below that takes the accent
vec3 env(vec3 d) {
  vec3 skyL = mix(vec3(0.88, 0.885, 0.893), vec3(1.0), smoothstep(-0.3, 0.7, d.y));
  vec3 skyD = mix(vec3(0.045, 0.047, 0.052), vec3(0.24, 0.245, 0.26), smoothstep(-0.2, 0.9, d.y));
  vec3 c = mix(skyL, skyD, uDark);
  c = mix(c, mix(vec3(0.7, 0.71, 0.73), uAcc, uK) * mix(1.0, 0.8, uDark), smoothstep(0.05, -0.75, d.y) * 0.7);
  c += 1.7 * smoothstep(0.88, 0.985, dot(d, normalize(vec3(-0.55, 0.62, 0.55))));
  c += 0.75 * smoothstep(0.92, 0.995, dot(d, normalize(vec3(0.75, 0.2, 0.62))));
  return c;
}
vec3 shade(vec3 p, vec3 rd) {
  vec3 n = nrm(p);
  float ndv = clamp(dot(n, -rd), 0.0, 1.0);
  float fr = 0.04 + 0.96 * pow(1.0 - ndv, 5.0);
  vec3 rf = reflect(rd, n);
  vec3 refl = env(rf);
  vec3 tr = vec3(env(refract(rd, n, 1.0 / 1.26)).r, env(refract(rd, n, 1.0 / 1.33)).g, env(refract(rd, n, 1.0 / 1.41)).b);
  // caustic veins; thinking swirls them
  vec3 cp = p * 1.25 + refract(rd, n, 0.75) * 0.8;
  cp.xz = rot(uSpin * 1.6) * cp.xz;
  float v = n3(cp + vec3(0.0, uT * 0.18, uT * 0.1));
  float caus = clamp(pow(max(1.0 - abs(v) * 1.8, 0.0), 10.0), 0.0, 1.0);
  // silver glass: milky, clear at the rim
  vec3 milk = mix(vec3(0.99, 0.988, 0.984), vec3(0.6, 0.61, 0.63), uDark);
  vec3 silver = mix(tr, milk, 0.5);
  // ruby glass: light through coloured glass deepens with thickness, a glow scatters inside,
  // and a broad soft highlight sits in the upper half where the light comes in
  // the glass gets deeper as the pearl gets more active, and darker toward the rim where light travels further
  float thick = (0.75 + 0.9 * (1.0 - ndv)) * (0.6 + 1.1 * uK);
  vec3 ruby = tr * pow(uAcc, vec3(thick)) * 0.98 + uAcc * (0.05 + 0.14 * uK + uDark * (0.3 + 0.25 * uK)) * pow(ndv, 1.3);
  ruby += mix(uAcc, vec3(1.0), 0.6) * 0.3 * smoothstep(0.15, 0.85, n.y) * smoothstep(0.25, 0.8, ndv);
  ruby += mix(uAcc, vec3(1.0, 0.7, 0.65), 0.35) * 0.22 * smoothstep(-0.2, -0.9, n.y) * (1.0 - ndv);
  vec3 body = mix(silver, ruby, smoothstep(0.0, 0.45, uK));
  body += caus * mix(vec3(1.0), mix(uAcc, vec3(1.0), 0.45), uK) * (0.14 + 0.32 * uW.z + 0.3 * uW.w * uL);
  vec3 film = 0.5 + 0.5 * cos(6.2831 * (vec3(0.0, 0.33, 0.67) + (1.0 - ndv) * 1.4 + n3(p * 2.0 + uT * 0.1) * 0.6));
  vec3 col = mix(body, refl, fr * (1.0 - 0.35 * uK));
  col += film * pow(1.0 - ndv, 2.4) * 0.3 * (1.0 - uK);
  col += mix(vec3(1.0), vec3(1.0, 0.8, 0.76), uK) * pow(1.0 - ndv, 7.0) * 0.55;          // thin bright rim
  col += pow(max(dot(rf, normalize(vec3(-0.55, 0.62, 0.55))), 0.0), 140.0) * 1.8;
  col += pow(max(dot(rf, normalize(vec3(0.7, 0.25, 0.65))), 0.0), 50.0) * 0.35;
  col += uAcc * uW.w * uL * 0.12 * ndv;                                                    // speaking glows
  col *= 1.0 + 0.22 * uPulse;
  return mix(col, 1.0 - exp(-col * 1.25), 0.45);
}

void main() {
  vec2 frag = vec2(gl_FragCoord.x, uRes.y - gl_FragCoord.y);
  vec2 uv = (frag - uC) / uR; uv.y = -uv.y;
  float d = length(uv);
  vec3 col = uBg;
  // The field: an organic sheet of the same glass. Listening, it leans down toward you;
  // thinking, it draws in and turns; speaking, it swells on the voice and light radiates from it.
  vec2 fc = uv - vec2(0.0, -0.24 * uW.y);
  float fd = length(fc), a = atan(fc.y, fc.x), ph = uSpin * 0.7;
  float n1 = 0.5 + 0.2 * sin(3.0 * a + uT * 0.5 + ph) + 0.13 * sin(5.0 * a - uT * 0.7 + 1.3 - ph) + 0.1 * sin(a + uT * 0.31 + 2.1) + 0.05 * sin(7.0 * a + uT * 0.9 + uRip * 0.2);
  float rF = uAir * (1.5 + (n1 - 0.5) * (0.36 + 0.4 * uW.y * (0.4 + uL)) - 0.12 * uW.z + 0.22 * uW.w * uL);
  float f = fd - rF;
  float inside = smoothstep(0.035, -0.035, f);
  float depth = clamp(-f / 0.6, 0.0, 1.0);
  float up = clamp(fc.y / rF, -1.0, 1.0);
  vec3 silverF = mix(mix(vec3(0.84, 0.845, 0.855), vec3(0.97), depth), mix(vec3(0.16, 0.165, 0.175), vec3(0.27, 0.275, 0.29), depth), uDark);
  vec3 rubyF = mix(uAcc, uAcc * 0.82, smoothstep(0.0, 0.9, depth));
  rubyF += vec3(1.0, 0.86, 0.84) * 0.22 * smoothstep(0.1, 0.75, up) * smoothstep(0.05, 0.45, depth);
  vec3 fcol = mix(silverF, rubyF, uK);
  float rim = exp(-pow(f / 0.03, 2.0)) * (0.3 + 0.55 * smoothstep(0.1, -0.9, up));
  fcol += mix(vec3(1.0), vec3(1.0, 0.82, 0.78), uK) * rim;
  float fa = uQuiet * (0.22 + 0.16 * uK) * (0.75 + 0.25 * depth);
  col = mix(col, fcol, inside * fa);
  // a soft glow around the field; speaking sends rings of light outward through it
  float out_ = max(f, 0.0);
  float rings = 0.55 + 0.45 * sin(out_ * 13.0 - uPh * 1.2);
  float glow = exp(-out_ * 3.6) * (1.0 - inside) * uK * uQuiet * (0.14 + 0.22 * uW.w * uL * rings);
  col = mix(col, uAcc, glow);
  // a soft contact shadow, tinted where light comes through coloured glass
  float sh = exp(-pow(uv.x / 0.9, 2.0) - pow((uv.y + 1.2) / 0.12, 2.0));
  vec3 shc = mix(col * mix(0.76, 0.55, uDark), col * mix(vec3(1.0), uAcc, 0.55), 0.5 * uK);
  col = mix(col, shc, sh * 0.6 * (1.0 - uDock));
  if (d < 1.32) {
    vec3 ro = vec3(uv, 1.6), rd = vec3(0.0, 0.0, -1.0);
    float t = 0.0, mh = 1e3; bool hit = false;
    for (int i = 0; i < 72; i++) {
      float h = map(ro + rd * t); mh = min(mh, h);
      if (h < 0.001) { hit = true; break; }
      t += h; if (t > 3.2) break;
    }
    float aa = 1.2 / uR;
    if (hit) col = shade(ro + rd * t, rd);
    else if (mh < aa) col = mix(col, mix(vec3(1.0), mix(uAcc, vec3(1.0), 0.4), uK), (1.0 - mh / aa) * mix(0.5, 0.35, uDark));
  }
  fragColor = vec4(clamp(col, 0.0, 1.0), 1.0);
}`;
