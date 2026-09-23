/* Pure logic, kept apart from the DOM so it can be tested.
 *
 * Everything here is a function of its arguments: formatting, colour
 * arithmetic, outlier detection, ordering. The modules that touch the document
 * import from here rather than reimplementing any of it.
 */

/* ── Formatting ──────────────────────────────────────────────────────── */

export const fmtPct = (v) => (v >= 10 ? v.toFixed(0) : v.toFixed(1));

export function fmtBytes(b) {
  if (b >= 1024 ** 3) return { value: (b / 1024 ** 3).toFixed(1), unit: "GB" };
  if (b >= 1024 ** 2) return { value: (b / 1024 ** 2).toFixed(0), unit: "MB" };
  return { value: (b / 1024).toFixed(0), unit: "KB" };
}

export function bytesText(b) {
  const { value, unit } = fmtBytes(b);
  return `${value} ${unit}`;
}

export const rateText = (kb) =>
  kb >= 1024 ? `${(kb / 1024).toFixed(1)} MB/s` : `${Math.round(kb)} KB/s`;

/** Describes the gap between stored samples, pluralised properly. */
export function describeStep(rows) {
  if (rows.length < 2) return "sample";
  const step = rows[1].t - rows[0].t;
  const unit = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
  if (step < 60) return unit(step, "second");
  if (step < 3600) return unit(Math.round(step / 60), "minute");
  return unit(Math.round(step / 3600), "hour");
}

export function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

/* Mirrors the model's rule: a substituted fraction under a twentieth is
   smaller than the rounding, so it earns no qualifier. */
export const isApproximate = (u) => u.mem_unmeasured * 20 > u.mem_pss;

/* ── Colour ──────────────────────────────────────────────────────────── */

export function relativeLuminance(hex) {
  const m = hex.replace("#", "");
  const n = m.length === 3 ? m.split("").map((c) => c + c).join("") : m;
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(n.slice(i, i + 2), 16) / 255);
  const lin = (c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

export function contrastRatio(a, b) {
  const [la, lb] = [relativeLuminance(a), relativeLuminance(b)];
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

/* Picks the label colour with the better contrast against a fill.
 *
 * An earlier version thresholded on the fill's luminance instead, and got
 * every palette colour wrong: white on the light green measured 2.82:1, under
 * the 4.5:1 a label needs, where dark ink gives 6.99:1. A mid-luminance fill
 * is exactly where a threshold guesses and a comparison does not have to. */
export function inkOn(hex) {
  // Pure black rather than the page's near-black ink token: on the lightest
  // blue the token reaches only 4.46:1, just under what a label needs, and
  // black reaches 4.76:1. A label set inside a coloured fill is the one place
  // the palette's text colour gives way to whatever actually clears contrast.
  const ink = "#000000";
  const white = "#ffffff";
  return contrastRatio(hex, ink) >= contrastRatio(hex, white) ? ink : white;
}

/* ── Outliers ────────────────────────────────────────────────────────── */

/* Peaks that stand clearly above the usual level.
 *
 * Uses a median and a median-to-quartile spread rather than a mean and a
 * standard deviation: the spikes themselves inflate both of those and end up
 * hiding the smaller ones. A run of consecutive points over the threshold
 * collapses to its peak, so one burst produces one mark. */
export function findSpikes(points, pick, { limit = 6 } = {}) {
  if (points.length < 12) return [];
  const sorted = points.map(pick).sort((a, b) => a - b);
  const median = sorted[Math.floor(sorted.length / 2)];
  const q3 = sorted[Math.floor(sorted.length * 0.75)];
  const largest = sorted[sorted.length - 1] || 1;
  const spread = Math.max(q3 - median, largest / 20, 0.5);
  const threshold = Math.max(median + spread * 3, largest * 0.25);

  const peaks = [];
  let run = null;
  for (const p of points) {
    if (pick(p) >= threshold && pick(p) > 0) {
      if (!run || pick(p) > pick(run)) run = p;
    } else if (run) {
      peaks.push(run);
      run = null;
    }
  }
  if (run) peaks.push(run);

  return peaks
    .sort((a, b) => pick(b) - pick(a))
    .slice(0, limit)
    .sort((a, b) => a.t - b.t);
}

/* The largest jumps upward, for quantities where the level is uninteresting
   but the moment it changed is not — memory being the case in point. */
export function findGrowth(points, pick, { limit = 5, floor = 0.1 } = {}) {
  if (points.length < 4) return [];
  const jumps = [];
  for (let i = 1; i < points.length; i += 1) {
    const growth = pick(points[i]) - pick(points[i - 1]);
    if (growth > 0) jumps.push({ t: points[i].t, growth });
  }
  if (!jumps.length) return [];
  const largest = Math.max(...jumps.map((j) => j.growth));
  const cutoff = Math.max(largest / 3, floor);
  return jumps
    .filter((j) => j.growth >= cutoff)
    .sort((a, b) => b.growth - a.growth)
    .slice(0, limit)
    .sort((a, b) => a.t - b.t);
}

/* ── Ordering ────────────────────────────────────────────────────────── */

/* Filters and orders the application list.
 *
 * `heldOrder` pins a previous arrangement: sorting by processor use
 * resequences every second, so without it the row being reached for slides out
 * from under the cursor. Anything new while the order is held goes to the end
 * rather than pushing existing rows around. */
export function arrangeApps(apps, { query = "", sort = "cpu", heldOrder = null } = {}) {
  const q = query.trim().toLowerCase();
  let out = apps;
  if (q) {
    out = out.filter(
      (a) =>
        a.name.toLowerCase().includes(q) ||
        a.id.toLowerCase().includes(q) ||
        (a.windows || []).some((w) => w.toLowerCase().includes(q)),
    );
  }

  if (heldOrder) {
    const rank = new Map(heldOrder.map((id, i) => [id, i]));
    return [...out].sort(
      (a, b) => (rank.get(a.id) ?? Infinity) - (rank.get(b.id) ?? Infinity),
    );
  }

  const by = {
    cpu: (a, b) => b.totals.cpu_cores - a.totals.cpu_cores,
    mem: (a, b) => b.totals.mem_pss - a.totals.mem_pss,
    name: (a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }),
  }[sort];

  return [...out].sort(by);
}

/* Filters a process tree, keeping any branch that leads to a match.
 *
 * A process whose own name does not match is still worth showing when
 * something beneath it does — otherwise filtering hides the structure that
 * explains the result. */
export function filterTree(nodes, query) {
  const q = query.trim().toLowerCase();
  if (!q) return nodes;
  const keep = (n) => {
    const children = n.children.map(keep).filter(Boolean);
    const hit =
      n.comm.toLowerCase().includes(q) ||
      (n.window_title || "").toLowerCase().includes(q) ||
      n.role.label.toLowerCase().includes(q) ||
      String(n.pid).includes(q);
    return hit || children.length ? { ...n, children } : null;
  };
  return nodes.map(keep).filter(Boolean);
}

/* ── Gaps in a series ────────────────────────────────────────────────── */

/* Splits a series wherever samples are missing.
 *
 * A line chart joins consecutive points, which quietly asserts that the value
 * moved smoothly between them. When the machine was switched off there are no
 * samples at all, and joining across that hole drew a clean diagonal from the
 * last reading before shutdown to the first after boot — a graph showing the
 * processor climbing steadily overnight on a machine that was unplugged.
 *
 * The data was right; the drawing invented the part in between. Splitting on
 * gaps leaves the hole empty, which is what actually happened.
 *
 * The expected spacing is taken from the median of the gaps rather than the
 * first one, so a single missing sample at the start cannot set the scale. */
export function segmentByGaps(points, { tolerance = 2.5 } = {}) {
  if (points.length < 2) return points.length ? [points] : [];

  const steps = [];
  for (let i = 1; i < points.length; i += 1) steps.push(points[i][0] - points[i - 1][0]);
  const sorted = [...steps].sort((a, b) => a - b);
  const expected = sorted[Math.floor(sorted.length / 2)] || 0;
  if (expected <= 0) return [points];

  const limit = expected * tolerance;
  const out = [];
  let run = [points[0]];
  for (let i = 1; i < points.length; i += 1) {
    if (points[i][0] - points[i - 1][0] > limit) {
      out.push(run);
      run = [];
    }
    run.push(points[i]);
  }
  if (run.length) out.push(run);
  return out;
}

/* Width to reserve for the y-axis labels.
 *
 * A fixed gutter clipped wide values: a disk axis topping out at 195.3 MB/s
 * rendered as "95.3", and the axis then read as non-monotonic, which is worse
 * than useless. Sized from the longest label the axis will actually draw. */
export function axisGutter(labels, { charWidth = 6.2, padding = 14, min = 34 } = {}) {
  const longest = labels.reduce((n, l) => Math.max(n, String(l).length), 0);
  return Math.max(min, Math.ceil(longest * charWidth + padding));
}
