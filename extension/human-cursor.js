// Human-like mouse trajectories: a Bézier curve through a few random knots
// around the straight line, light vertical jitter, and an easing function that
// spaces the samples so the pointer accelerates and settles like a hand does.
//
// Ported (trimmed, to plain JS) from human-cursor's curve generator —
// https://github.com/CloverLabsAI/human-cursor (ISC), itself built on Xetera's
// ghost-cursor (MIT, © 2021 Xetera) and a port of the Python `humancursor`.
// Only the path math is taken; the Playwright driver is replaced by our CDP one.

const TWEENS = [
  (t) => t,
  (t) => t * t,
  (t) => t * (2 - t),
  (t) => (t < 0.5 ? 2 * t * t : -1 + (4 - 2 * t) * t),
  (t) => t * t * t,
  (t) => (t - 1) ** 3 + 1,
  (t) => (t < 0.5 ? 4 * t * t * t : (t - 1) * (2 * t - 2) * (2 * t - 2) + 1),
];

const randInt = (min, max) => Math.floor(Math.random() * (max - min + 1)) + min;

function weighted(items, weights) {
  let r = Math.random() * weights.reduce((a, b) => a + b, 0);
  for (let i = 0; i < items.length; i++) {
    r -= weights[i];
    if (r <= 0) return items[i];
  }
  return items[items.length - 1];
}

function randomNormal(mean, sd) {
  const u1 = Math.random() || Number.EPSILON;
  const u2 = Math.random();
  return Math.sqrt(-2 * Math.log(u1)) * Math.cos(2 * Math.PI * u2) * sd + mean;
}

/// Randomised curve parameters, distributed as in humancursor's web profile.
function curveParams(from, to) {
  const range = () => weighted([[20, 44], [45, 74], [75, 99]], [0.2, 0.65, 15]);
  const distance = Math.hypot(to.x - from.x, to.y - from.y);
  const minBoundary = Math.max(30, distance * 0.15);
  const [bx0, bx1] = range();
  const [by0, by1] = range();
  const [p0, p1] = weighted([[35, 44], [45, 59], [60, 79]], [0.53, 0.32, 0.15]);
  return {
    offsetX: Math.max(randInt(bx0, bx1), minBoundary),
    offsetY: Math.max(randInt(by0, by1), minBoundary),
    knots: Math.max(
      2,
      weighted([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], [0.15, 0.36, 0.17, 0.12, 0.08, 0.04, 0.03, 0.02, 0.015, 0.005]),
    ),
    distortionMean: randInt(80, 109) / 100,
    distortionSd: randInt(85, 109) / 100,
    distortionFreq: randInt(25, 69) / 100,
    tween: TWEENS[Math.floor(Math.random() * TWEENS.length)],
    targetPoints: randInt(p0, p1),
  };
}

function binomial(n, k) {
  let r = 1;
  for (let i = 1; i <= k; i++) r = (r * (n - k + i)) / i;
  return r;
}

function bezier(count, control) {
  const n = control.length - 1;
  const out = [];
  for (let i = 0; i < count; i++) {
    if (i === 0) { out.push({ ...control[0] }); continue; }
    if (i === count - 1) { out.push({ ...control[n] }); continue; }
    const t = i / (count - 1);
    let x = 0, y = 0;
    for (let j = 0; j <= n; j++) {
      const b = binomial(n, j) * t ** j * (1 - t) ** (n - j);
      x += control[j].x * b;
      y += control[j].y * b;
    }
    out.push({ x, y });
  }
  return out;
}

/// Points from `from` to `to` (inclusive), viewport CSS pixels.
export function humanPath(from, to) {
  const p = curveParams(from, to);
  const left = Math.min(from.x, to.x) - p.offsetX;
  const right = Math.max(from.x, to.x) + p.offsetX;
  const down = Math.min(from.y, to.y) - p.offsetY;
  const up = Math.max(from.y, to.y) + p.offsetY;
  const knots = Array.from({ length: p.knots }, () => ({
    x: randInt(Math.floor(left), Math.floor(right)),
    y: randInt(Math.floor(down), Math.floor(up)),
  }));

  const distance = Math.hypot(to.x - from.x, to.y - from.y);
  let pts = bezier(Math.max(Math.floor(distance), 50), [from, ...knots, to]);

  // Jitter: nudge some interior points vertically.
  pts = pts.map((pt, i) =>
    i === 0 || i === pts.length - 1 || Math.random() >= p.distortionFreq
      ? pt
      : { x: pt.x, y: pt.y + randomNormal(p.distortionMean, p.distortionSd) },
  );

  // Resample to the target count along the easing curve.
  if (pts.length <= p.targetPoints) return pts;
  const res = [];
  for (let i = 0; i < p.targetPoints; i++) {
    if (i === 0) { res.push({ ...pts[0] }); continue; }
    if (i === p.targetPoints - 1) { res.push({ ...pts[pts.length - 1] }); continue; }
    const ci = p.tween(i / (p.targetPoints - 1)) * (pts.length - 1);
    const lo = Math.floor(ci), hi = Math.min(lo + 1, pts.length - 1), f = ci - lo;
    res.push({ x: pts[lo].x + (pts[hi].x - pts[lo].x) * f, y: pts[lo].y + (pts[hi].y - pts[lo].y) * f });
  }
  return res;
}

/// How long a person takes to reach a target `distance` px away that is
/// `width` px across (Fitts's law, with some spread), in ms.
export function movementMs(distance, width) {
  const id = Math.log2(distance / Math.max(width, 8) + 1);
  return Math.round((90 + 140 * id) * (0.85 + Math.random() * 0.3));
}

/// A point inside `box`, biased toward the middle, never in the outer 15%.
export function pointInBox(box) {
  const pick = (start, size) => {
    const inset = size * 0.15;
    const v = start + size / 2 + randomNormal(0, size / 6);
    return Math.min(start + size - inset, Math.max(start + inset, v));
  };
  return { x: pick(box.x, box.width), y: pick(box.y, box.height) };
}

export { randInt, randomNormal };
