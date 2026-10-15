// Shared glass shading (the pearl's): a soft studio environment, reflection and refraction with a
// little dispersion, silver glass at rest and ruby glass (Beer–Lambert colour, inner glow, a soft
// highlight band, a bright rim) as the accent intensity uK rises.
const GLASS = `
vec3 g3(vec3 p) { p = fract(p * vec3(0.1031, 0.1030, 0.0973)); p += dot(p, p.yxz + 33.33); return fract((p.xxy + p.yxx) * p.zyx) * 2.0 - 1.0; }
float n3(vec3 p) { vec3 i = floor(p), f = fract(p), u = f * f * (3.0 - 2.0 * f);
  return mix(mix(mix(dot(g3(i), f), dot(g3(i + vec3(1, 0, 0)), f - vec3(1, 0, 0)), u.x),
                 mix(dot(g3(i + vec3(0, 1, 0)), f - vec3(0, 1, 0)), dot(g3(i + vec3(1, 1, 0)), f - vec3(1, 1, 0)), u.x), u.y),
             mix(mix(dot(g3(i + vec3(0, 0, 1)), f - vec3(0, 0, 1)), dot(g3(i + vec3(1, 0, 1)), f - vec3(1, 0, 1)), u.x),
                 mix(dot(g3(i + vec3(0, 1, 1)), f - vec3(0, 1, 1)), dot(g3(i + vec3(1, 1, 1)), f - vec3(1, 1, 1)), u.x), u.y), u.z); }
mat2 rot(float a) { float c = cos(a), s = sin(a); return mat2(c, s, -s, c); }
vec3 env(vec3 d) {
  vec3 skyL = mix(vec3(0.88, 0.885, 0.893), vec3(1.0), smoothstep(-0.3, 0.7, d.y));
  vec3 skyD = mix(vec3(0.045, 0.047, 0.052), vec3(0.24, 0.245, 0.26), smoothstep(-0.2, 0.9, d.y));
  vec3 c = mix(skyL, skyD, uDark);
  c = mix(c, mix(vec3(0.7, 0.71, 0.73), uAcc, uK) * mix(1.0, 0.8, uDark), smoothstep(0.05, -0.75, d.y) * 0.7);
  c += 1.7 * smoothstep(0.88, 0.985, dot(d, normalize(vec3(-0.55, 0.62, 0.55))));
  c += 0.75 * smoothstep(0.92, 0.995, dot(d, normalize(vec3(0.75, 0.2, 0.62))));
  return c;
}
vec3 glassShade(vec3 p, vec3 n, vec3 rd, float causAmt) {
  float ndv = clamp(dot(n, -rd), 0.0, 1.0);
  float fr = 0.04 + 0.96 * pow(1.0 - ndv, 5.0);
  vec3 rf = reflect(rd, n);
  vec3 refl = env(rf);
  vec3 tr = vec3(env(refract(rd, n, 1.0 / 1.26)).r, env(refract(rd, n, 1.0 / 1.33)).g, env(refract(rd, n, 1.0 / 1.41)).b);
  vec3 cp = p * 1.25 + refract(rd, n, 0.75) * 0.8; cp.xz = rot(uSpin * 1.6) * cp.xz;
  float v = n3(cp + vec3(0.0, uT * 0.18, uT * 0.1));
  float caus = clamp(pow(max(1.0 - abs(v) * 1.8, 0.0), 10.0), 0.0, 1.0);
  vec3 milk = mix(vec3(0.99, 0.988, 0.984), vec3(0.6, 0.61, 0.63), uDark);
  vec3 silver = mix(tr, milk, 0.5);
  float thick = (0.75 + 0.9 * (1.0 - ndv)) * (0.6 + 1.1 * uK);
  vec3 ruby = tr * pow(uAcc, vec3(thick)) * 0.98 + uAcc * (0.05 + 0.14 * uK + uDark * (0.3 + 0.25 * uK)) * pow(ndv, 1.3);
  ruby += mix(uAcc, vec3(1.0), 0.6) * 0.3 * smoothstep(0.15, 0.85, n.y) * smoothstep(0.25, 0.8, ndv);
  ruby += mix(uAcc, vec3(1.0, 0.7, 0.65), 0.35) * 0.22 * smoothstep(-0.2, -0.9, n.y) * (1.0 - ndv);
  vec3 body = mix(silver, ruby, smoothstep(0.0, 0.45, uK));
  body += caus * mix(vec3(1.0), mix(uAcc, vec3(1.0), 0.45), uK) * causAmt;
  vec3 film = 0.5 + 0.5 * cos(6.2831 * (vec3(0.0, 0.33, 0.67) + (1.0 - ndv) * 1.4 + n3(p * 2.0 + uT * 0.1) * 0.6));
  vec3 col = mix(body, refl, fr * (1.0 - 0.35 * uK));
  col += film * pow(1.0 - ndv, 2.4) * 0.3 * (1.0 - uK);
  col += mix(vec3(1.0), vec3(1.0, 0.8, 0.76), uK) * pow(1.0 - ndv, 7.0) * 0.55;
  col += pow(max(dot(rf, normalize(vec3(-0.55, 0.62, 0.55))), 0.0), 140.0) * 1.8;
  col += pow(max(dot(rf, normalize(vec3(0.7, 0.25, 0.65))), 0.0), 50.0) * 0.35;
  col *= 1.0 + 0.22 * uPulse;
  return mix(col, 1.0 - exp(-col * 1.25), 0.45);
}`;
