// Critically-or-under-damped springs, described the way Apple does:
// a perceptual duration (seconds) and a bounce (0 = no overshoot).
// Integrated with fixed 1/240 s sub-steps so the feel is identical at 60 and 120 Hz.

const H = 1 / 240;

export class Spring {
  constructor(value = 0, duration = 0.5, bounce = 0) {
    this.x = value;
    this.v = 0;
    this.target = value;
    this.configure(duration, bounce);
  }

  configure(duration, bounce = 0) {
    const w = (2 * Math.PI) / Math.max(0.05, duration);
    this.k = w * w;
    const zeta = bounce >= 0 ? 1 - bounce : 1 / (1 + bounce);
    this.c = 2 * zeta * w;
    return this;
  }

  step(dt) {
    let t = dt;
    while (t > 1e-6) {
      const h = t > H ? H : t;
      const a = -this.k * (this.x - this.target) - this.c * this.v;
      this.v += a * h;
      this.x += this.v * h;
      t -= h;
    }
    return this.x;
  }

  snap(value = this.target) {
    this.x = this.target = value;
    this.v = 0;
  }

  get resting() {
    return Math.abs(this.x - this.target) < 1e-4 && Math.abs(this.v) < 1e-3;
  }
}

export const clamp = (x, a, b) => (x < a ? a : x > b ? b : x);
export const lerp = (a, b, t) => a + (b - a) * t;

// mm:ss for timecodes.
export function timecode(seconds) {
  const s = Math.max(0, seconds);
  const m = Math.floor(s / 60);
  const sec = Math.floor(s % 60);
  return { mmss: `${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}` };
}
