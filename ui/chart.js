/* Time-series charts.
 *
 * Deliberately plain: a 2px line over a 10%-opacity wash, hairline gridlines
 * that stay out of the way, and a crosshair with a tooltip because an HTML
 * chart that cannot be interrogated is a picture, not an instrument.
 *
 * One rule enforced structurally: every series in a chart shares one y-axis.
 * Two scales stacked in one frame can be made to show any relationship you
 * like, which is why they are never used here.
 */

const PAD = { top: 10, right: 12, bottom: 20, left: 46 };

/** Rounds an axis maximum up to a clean 1 / 2 / 5 × 10ⁿ step. */
function niceTicks(max, count = 4) {
  if (!(max > 0)) return { max: 1, step: 1 };
  const rough = max / count;
  const mag = 10 ** Math.floor(Math.log10(rough));
  const norm = rough / mag;
  const step = (norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 5 ? 5 : 10) * mag;
  return { max: Math.ceil(max / step) * step, step };
}

function timeLabel(t, span) {
  const d = new Date(t * 1000);
  const pad = (n) => String(n).padStart(2, "0");
  // Below two days, the clock is what matters; beyond it, the date is.
  if (span <= 48 * 3600) return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return `${d.getDate()}/${d.getMonth() + 1}`;
}

/**
 * host: element to draw into.
 * opts: { series: [{name, color, points: [[t, v], …]}], yFormat, yMin, yMax,
 *         height, empty }
 */
export function drawChart(host, opts) {
  const { series = [], yFormat = String, height = 150, empty = "No history yet" } = opts;
  const width = host.clientWidth;
  if (width < 40) return;

  const all = series.flatMap((s) => s.points);
  if (all.length < 2) {
    host.innerHTML = `<div class="chart-empty">${empty}</div>`;
    return;
  }

  const t0 = Math.min(...all.map((p) => p[0]));
  const t1 = Math.max(...all.map((p) => p[0]));
  const span = Math.max(1, t1 - t0);
  const peak = Math.max(...all.map((p) => p[1]), opts.yMin ?? 0);
  // An explicit ceiling is a real quantity — installed memory, 100% — and must
  // be used exactly. Rounding it up the way an automatic axis is rounded puts
  // a 31 GB machine on a 40 GB scale and wastes a quarter of the frame.
  const { max: autoMax, step } = niceTicks(opts.yMax ?? peak);
  const yMax = opts.yMax ?? autoMax;

  const w = width - PAD.left - PAD.right;
  const h = height - PAD.top - PAD.bottom;
  const x = (t) => PAD.left + ((t - t0) / span) * w;
  const y = (v) => PAD.top + h - (Math.min(v, yMax) / yMax) * h;

  const parts = [];

  // Gridlines and y labels first, so every mark sits above them.
  for (let v = 0; v <= yMax + 1e-9; v += step) {
    if (v > yMax) break;
    const gy = y(v).toFixed(1);
    parts.push(
      `<line class="grid" x1="${PAD.left}" y1="${gy}" x2="${PAD.left + w}" y2="${gy}"/>`,
      `<text class="tick" x="${PAD.left - 8}" y="${gy}" text-anchor="end" dominant-baseline="middle">${yFormat(v)}</text>`,
    );
  }

  // X labels at the ends and the middle — enough to orient, few enough to read.
  for (const frac of [0, 0.5, 1]) {
    const t = t0 + span * frac;
    parts.push(
      `<text class="tick" x="${x(t).toFixed(1)}" y="${height - 6}" text-anchor="${
        frac === 0 ? "start" : frac === 1 ? "end" : "middle"
      }">${timeLabel(t, span)}</text>`,
    );
  }

  for (const s of series) {
    const pts = s.points.filter((p) => Number.isFinite(p[1]));
    if (pts.length < 2) continue;
    const d = pts.map((p, i) => `${i ? "L" : "M"}${x(p[0]).toFixed(1)},${y(p[1]).toFixed(1)}`).join(" ");
    // The wash is a fill under the line, never a saturated block.
    const base = (PAD.top + h).toFixed(1);
    parts.push(
      `<path d="${d} L${x(pts.at(-1)[0]).toFixed(1)},${base} L${x(pts[0][0]).toFixed(1)},${base} Z" fill="${s.color}" opacity="0.10"/>`,
      `<path d="${d}" fill="none" stroke="${s.color}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>`,
    );
  }

  parts.push(
    `<line class="crosshair" x1="0" y1="${PAD.top}" x2="0" y2="${PAD.top + h}" opacity="0"/>`,
  );

  host.innerHTML =
    `<svg class="chart-svg" width="${width}" height="${height}" role="img">${parts.join("")}</svg>`;

  attachHover(host, { series, x, y, t0, t1, width, height, yFormat });
}

/* Crosshair and readout. The hit area is the whole plot, not the 2px line —
   asking someone to hover a hairline is asking them not to bother. */
function attachHover(host, ctx) {
  const svg = host.querySelector(".chart-svg");
  const cross = host.querySelector(".crosshair");
  const tip = document.getElementById("tip");
  if (!svg || !tip) return;

  svg.addEventListener("mousemove", (ev) => {
    const box = svg.getBoundingClientRect();
    const px = ev.clientX - box.left;
    const frac = (px - PAD.left) / (ctx.width - PAD.left - PAD.right);
    const t = ctx.t0 + Math.max(0, Math.min(1, frac)) * (ctx.t1 - ctx.t0);

    cross.setAttribute("x1", ctx.x(t).toFixed(1));
    cross.setAttribute("x2", ctx.x(t).toFixed(1));
    cross.setAttribute("opacity", "1");

    const rows = ctx.series
      .map((s) => {
        const near = nearest(s.points, t);
        if (!near) return "";
        return `<div class="tip-row"><span class="tip-key" style="background:${s.color}"></span>${s.name} — ${ctx.yFormat(near[1], true)}</div>`;
      })
      .join("");

    const when = new Date(t * 1000);
    tip.innerHTML = `<div class="tip-title">${when.toLocaleString()}</div>${rows}`;
    tip.dataset.show = "true";

    const r = tip.getBoundingClientRect();
    let left = ev.clientX + 14;
    if (left + r.width > window.innerWidth - 8) left = ev.clientX - r.width - 14;
    tip.style.left = `${Math.max(8, left)}px`;
    tip.style.top = `${Math.max(8, box.top - r.height - 8)}px`;
  });

  svg.addEventListener("mouseleave", () => {
    cross.setAttribute("opacity", "0");
    tip.dataset.show = "false";
  });
}

function nearest(points, t) {
  let best = null;
  let bestGap = Infinity;
  for (const p of points) {
    const gap = Math.abs(p[0] - t);
    if (gap < bestGap) {
      bestGap = gap;
      best = p;
    }
  }
  return best;
}

/** A bare trend line with no axes, for sitting inside a row or a panel. */
export function drawSparkline(host, points, color, height = 34) {
  const width = host.clientWidth;
  if (width < 20 || points.length < 2) {
    host.innerHTML = "";
    return;
  }
  const t0 = points[0][0];
  const t1 = points.at(-1)[0];
  const span = Math.max(1, t1 - t0);
  const peak = Math.max(...points.map((p) => p[1]), 1);
  const x = (t) => ((t - t0) / span) * width;
  const y = (v) => height - 2 - (v / peak) * (height - 4);
  const d = points.map((p, i) => `${i ? "L" : "M"}${x(p[0]).toFixed(1)},${y(p[1]).toFixed(1)}`).join(" ");
  host.innerHTML = `<svg width="${width}" height="${height}">
    <path d="${d} L${width},${height} L0,${height} Z" fill="${color}" opacity="0.10"/>
    <path d="${d}" fill="none" stroke="${color}" stroke-width="2" stroke-linejoin="round"/>
  </svg>`;
}
