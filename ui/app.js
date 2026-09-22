import { squarify, foldTail } from "./treemap.js";
import { drawChart, drawSparkline } from "./chart.js";

/* ==================================================================
   State
   ================================================================== */

const state = {
  model: null,
  cpuCount: 1,
  metric: "cpu",     // "cpu" | "mem"
  drilledApp: null,  // app id when viewing inside one app
  selected: null,
  tiles: new Map(),     // key -> element, so tiles tween instead of flashing
  expanded: new Set(),  // pids whose explanation is open
  cmdlines: new Map(),  // pid -> argv, fetched once
  tab: "apps",          // "apps" | "perf"
  span: 3600,           // seconds of history shown
  history: [],          // machine series for the current span
};

const $ = (id) => document.getElementById(id);

/* ==================================================================
   Formatting
   ================================================================== */

const fmtPct = (v) => (v >= 10 ? v.toFixed(0) : v.toFixed(1));

function fmtBytes(b) {
  if (b >= 1024 ** 3) return { value: (b / 1024 ** 3).toFixed(1), unit: "GB" };
  if (b >= 1024 ** 2) return { value: (b / 1024 ** 2).toFixed(0), unit: "MB" };
  return { value: (b / 1024).toFixed(0), unit: "KB" };
}

const bytesText = (b) => {
  const { value, unit } = fmtBytes(b);
  return `${value} ${unit}`;
};

const cpuPct = (usage) => (usage.cpu_cores / state.cpuCount) * 100;

/* ==================================================================
   Colour

   Three categorical slots, read live from CSS so a theme change is
   picked up without touching this file. Colour encodes what *kind*
   of software a block is — not its size, which the area already
   says, and not which app it is, which the label says.
   ================================================================== */

const KIND_SLOT = { Gui: "--cat-1", System: "--cat-2", Kernel: "--cat-2", Background: "--cat-3" };

const KIND_LABEL = {
  Gui: "Your apps",
  System: "System services",
  Kernel: "System services",
  Background: "Background",
};

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

const colorForKind = (kind) => cssVar(KIND_SLOT[kind] || "--cat-3");

/* Chooses ink or white for a label sitting on a colour fill, so text
   inside a tile always clears contrast regardless of the hue. */
function inkOn(hex) {
  const m = hex.replace("#", "");
  const n = m.length === 3 ? m.split("").map((c) => c + c).join("") : m;
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(n.slice(i, i + 2), 16) / 255);
  const lin = (c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  const L = 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
  return L > 0.45 ? "#0b0b0b" : "#ffffff";
}

/* ==================================================================
   Summary
   ================================================================== */

function renderSummary(m) {
  const busy = (m.cpu_busy ?? 0) * 100;
  $("cpu-value").textContent = fmtPct(busy);
  $("cpu-sub").textContent = `of ${state.cpuCount} threads`;

  renderCores(m.per_cpu_busy || []);

  const used = m.mem.total - m.mem.available;
  const { value, unit } = fmtBytes(used);
  $("mem-value").textContent = value;
  $("mem-unit").textContent = unit;
  $("mem-sub").textContent =
    `of ${bytesText(m.mem.total)} · ${bytesText(m.mem.cached + m.mem.sreclaimable)} cache`;

  const real = m.apps.filter((a) => a.kind !== "Kernel");
  $("app-count").textContent = real.length;
  $("proc-count").textContent =
    `${m.apps.reduce((a, b) => a + b.process_count, 0)} processes`;

  renderVerdict(m, busy);
}

/* Sequential ramp: one hue, light to dark with magnitude. */
function renderCores(cores) {
  const host = $("cores");
  while (host.children.length < cores.length) {
    const d = document.createElement("div");
    d.className = "core";
    host.appendChild(d);
  }
  while (host.children.length > cores.length) host.lastChild.remove();

  const steps = ["--seq-100", "--seq-250", "--seq-400", "--seq-550", "--seq-700"];
  cores.forEach((busy, i) => {
    const el = host.children[i];
    if (busy < 0.02) {
      el.style.background = "var(--surface-sunk)";
    } else {
      const step = Math.min(steps.length - 1, Math.floor(busy * steps.length));
      el.style.background = `var(${steps[step]})`;
    }
    el.title = `Core ${i}: ${fmtPct(busy * 100)}%`;
  });
}

/* The distinction no mainstream monitor draws: a machine can be fully
   busy and perfectly healthy. Pressure, not utilisation, is what says
   whether anything is actually being held up. */
function renderVerdict(m, busyPct) {
  const cpuP = m.pressure.cpu?.some.avg10 ?? 0;
  const memP = m.pressure.memory?.some.avg10 ?? 0;
  const ioP = m.pressure.io?.some.avg10 ?? 0;

  let level = "good";
  let text = "Running comfortably";
  let sub = "";

  if (memP > 10) {
    level = "critical";
    text = "Short of memory";
    sub = "Programs are waiting on memory — this is what makes a machine feel stuck.";
  } else if (cpuP > 20) {
    level = "serious";
    text = "Competing for the processor";
    sub = "More work is ready to run than there are cores to run it.";
  } else if (ioP > 20) {
    level = "warning";
    text = "Waiting on the disk";
    sub = "Programs are held up reading or writing, not by the processor.";
  } else if (busyPct > 70) {
    text = "Working hard, but keeping up";
    sub = "High usage with nothing queuing — the machine is being used, not struggling.";
  } else {
    sub = "Nothing is waiting on the processor, memory or disk.";
  }

  $("verdict").dataset.level = level;
  $("verdict-text").textContent = text;
  $("verdict-sub").textContent = sub;
}

/* ==================================================================
   Treemap
   ================================================================== */

function valueOf(usage) {
  return state.metric === "cpu" ? usage.cpu_cores : usage.mem_pss;
}

const valueText = (usage) =>
  state.metric === "cpu" ? `${fmtPct(cpuPct(usage))}%` : bytesText(usage.mem_pss);

/* Flattens an app's process tree for the drill-down view. Self usage is
   used, not subtree, so nothing is counted twice inside one picture. */
function flattenProcs(nodes, out = []) {
  for (const n of nodes) {
    out.push(n);
    flattenProcs(n.children, out);
  }
  return out;
}

function mapItems(m) {
  if (state.drilledApp) {
    const app = m.apps.find((a) => a.id === state.drilledApp);
    if (!app) {
      state.drilledApp = null;
      return mapItems(m);
    }
    const procs = flattenProcs(app.roots).map((p) => ({
      id: `p${p.pid}`,
      name: p.window_title || p.comm,
      sub: p.role.label,
      why: p.role.explanation,
      value: valueOf(p.self_usage),
      kind: app.kind,
      node: p,
    }));
    return { items: foldTail(procs, 28, "Smaller helpers"), app };
  }

  const apps = m.apps
    .filter((a) => valueOf(a.totals) > 0)
    .map((a) => ({
      id: a.id,
      name: a.name,
      sub: `${a.process_count} process${a.process_count === 1 ? "" : "es"}`,
      value: valueOf(a.totals),
      kind: a.kind,
      app: a,
    }));
  return { items: foldTail(apps, 24), app: null };
}

function renderMap(m) {
  const host = $("map");
  const { items, app } = mapItems(m);
  const rect = host.getBoundingClientRect();
  if (rect.width < 4 || rect.height < 4) return;

  $("map-empty").style.display = items.length ? "none" : "grid";

  renderCrumbs(app);

  const laid = squarify(items, rect.width, rect.height);
  const seen = new Set();

  for (const t of laid) {
    seen.add(t.id);
    let el = state.tiles.get(t.id);
    if (!el) {
      el = document.createElement("div");
      el.className = "tile";
      el.tabIndex = 0;
      el.innerHTML = `<div class="tile-label"><span class="tile-name"></span><span class="tile-value num"></span></div>`;
      host.appendChild(el);
      state.tiles.set(t.id, el);
    }

    // A 2px gap in the surface colour separates tiles. A border would
    // add ink that is not data.
    el.style.left = `${t.x + 1}px`;
    el.style.top = `${t.y + 1}px`;
    el.style.width = `${Math.max(0, t.w - 2)}px`;
    el.style.height = `${Math.max(0, t.h - 2)}px`;

    const fill = t.isOther ? cssVar("--surface-sunk") : colorForKind(t.kind);
    el.style.background = fill;
    el.style.color = t.isOther ? cssVar("--ink-secondary") : inkOn(fill);

    // Labels are placed only where they genuinely fit — never clipped.
    const nameEl = el.querySelector(".tile-name");
    const valEl = el.querySelector(".tile-value");
    const roomForName = t.w >= 54 && t.h >= 26;
    const roomForValue = t.w >= 54 && t.h >= 42;
    nameEl.textContent = roomForName ? t.name : "";
    valEl.textContent = roomForValue
      ? `${t.isOther ? `${t.count} items · ` : ""}${t.value !== undefined && !t.isOther ? valueTextFor(t) : ""}`
      : "";

    el._tile = t;
  }

  for (const [id, el] of state.tiles) {
    if (!seen.has(id)) {
      el.remove();
      state.tiles.delete(id);
    }
  }

  renderLegend(app);
}

/* The trail out of a drill-down. Escape and a background click still
   work, but neither is discoverable, so neither counts as navigation. */
function renderCrumbs(app) {
  const drilled = Boolean(app);
  $("crumbs").dataset.drilled = String(drilled);
  $("crumb-back").hidden = !drilled;
  $("crumb-sep").hidden = !drilled;
  $("crumb-current").hidden = !drilled;
  if (drilled) $("crumb-current").textContent = app.name;

  $("map-note").textContent = drilled
    ? ` — ${app.process_count} process${app.process_count === 1 ? "" : "es"}`
    : state.metric === "cpu"
      ? " — block size is processor time"
      : " — block size is memory";
}

function drillOut() {
  if (!state.drilledApp) return;
  state.drilledApp = null;
  if (state.model) renderMap(state.model);
}

function valueTextFor(t) {
  if (t.node) return valueText(t.node.self_usage);
  if (t.app) return valueText(t.app.totals);
  return "";
}

/* A legend is always present when more than one colour is in play, so
   identity never rests on colour-matching alone. Inside one app every
   tile shares a hue, so the legend would restate the title. */
function renderLegend(app) {
  const host = $("legend");
  host.innerHTML = "";
  if (app) return;

  const kinds = new Set(
    (state.model?.apps || []).filter((a) => valueOf(a.totals) > 0).map((a) => a.kind),
  );
  const order = ["Gui", "Background", "System", "Kernel"];
  const seenLabels = new Set();

  for (const k of order) {
    if (!kinds.has(k) || seenLabels.has(KIND_LABEL[k])) continue;
    seenLabels.add(KIND_LABEL[k]);
    const item = document.createElement("div");
    item.className = "legend-item";
    item.innerHTML = `<span class="legend-swatch"></span><span>${KIND_LABEL[k]}</span>`;
    item.querySelector(".legend-swatch").style.background = colorForKind(k);
    host.appendChild(item);
  }
}

/* ==================================================================
   Application list
   ================================================================== */

function renderList(m) {
  const host = $("list");
  // Kernel threads are included here as the single grouped row the model
  // produces. Their CPU time is real, and hiding it in the list while the
  // treemap shows it would make the two views disagree.
  const apps = m.apps.slice(0, 60);
  host.innerHTML = "";

  for (const a of apps) {
    const row = document.createElement("div");
    row.className = "row";
    row.setAttribute("aria-selected", String(state.selected === a.id));

    const sub = a.windows[0] || subtitleFor(a);
    const est = isApproximate(a.totals) ? "~" : "";

    row.innerHTML = `
      <div class="row-name">
        <span class="row-dot"></span>
        <div style="min-width:0">
          <div class="row-title"></div>
          <div class="row-sub"></div>
        </div>
      </div>
      <div class="row-num num"><strong>${fmtPct(cpuPct(a.totals))}</strong>%</div>
      <div class="row-num num">${est}${bytesText(a.totals.mem_pss)}</div>
      <div class="row-procs num">${a.process_count} proc${a.process_count === 1 ? "" : "s"}</div>`;

    row.querySelector(".row-dot").style.background = colorForKind(a.kind);
    row.querySelector(".row-title").textContent = a.name;
    row.querySelector(".row-sub").textContent = sub;
    row.onclick = () => openDetail(a.id);
    host.appendChild(row);
  }
}

/* Mirrors the model's rule: a substituted fraction under a twentieth is
   smaller than the rounding, so it earns no qualifier. */
const isApproximate = (u) => u.mem_unmeasured * 20 > u.mem_pss;

function subtitleFor(a) {
  if (a.kind === "Kernel") return "Kernel threads — part of the operating system";
  if (a.kind === "System") return "System service";
  if (a.kind === "Background") return "Background";
  // Say so when the grouping rests on a weak signal rather than
  // presenting a guess with the same confidence as a fact.
  return a.identified_by === "Executable" ? "Unrecognised program" : "No window open";
}

/* ==================================================================
   Detail panel
   ================================================================== */

function openDetail(appId) {
  if (state.selected !== appId) state.expanded.clear();
  state.selected = appId;
  renderDetail();
  $("detail").dataset.open = "true";
  $("detail").setAttribute("aria-hidden", "false");
}

function closeDetail() {
  state.selected = null;
  $("detail").dataset.open = "false";
  $("detail").setAttribute("aria-hidden", "true");
  if (state.model) renderList(state.model);
}

function renderDetail() {
  const m = state.model;
  if (!m || !state.selected) return;
  const app = m.apps.find((a) => a.id === state.selected);
  if (!app) return closeDetail();

  $("detail-title").textContent = app.name;
  $("detail-sub").textContent =
    app.windows[0] || `${KIND_LABEL[app.kind]} · ${app.process_count} processes`;

  const over = app.totals.mem_rss - app.totals.mem_pss;
  const body = $("detail-body");
  const scroll = body.scrollTop;
  body.innerHTML = "";

  body.appendChild(
    html(`<div class="facts">
      <div><div class="fact-label">Processor</div><div class="fact-value num">${fmtPct(cpuPct(app.totals))}%</div></div>
      <div><div class="fact-label">Memory</div><div class="fact-value num">${bytesText(app.totals.mem_pss)}</div></div>
      <div><div class="fact-label">Processes</div><div class="fact-value num">${app.process_count}</div></div>
      <div><div class="fact-label">Windows</div><div class="fact-value num">${app.windows.length}</div></div>
    </div>`),
  );

  // The memory correction, stated plainly, because it is large and
  // nothing else the user has run tells them about it.
  if (app.process_count > 1 && over > 64 * 1024 * 1024) {
    body.appendChild(
      html(`<div class="note">These ${app.process_count} processes share a lot of memory between them.
      The real cost is <strong>${bytesText(app.totals.mem_pss)}</strong>; adding up each process
      separately, as most task managers do, would report ${bytesText(app.totals.mem_rss)}.</div>`),
    );
  }

  const composition = countRoles(app.roots);
  if (app.process_count > 1) {
    const parts = [...composition.entries()]
      .sort((a, b) => b[1] - a[1])
      .map(([label, n]) => `${n} × ${label.toLowerCase()}`)
      .join(", ");
    body.appendChild(html(`<div class="note">Made up of ${parts}.</div>`));
  }

  // One cheap query for the app being looked at, rather than sixty for a
  // list nobody is reading.
  const spark = html(`<div><div class="subhead">Processor, last 24 hours</div><div class="spark" id="app-spark"></div></div>`);
  body.appendChild(spark);
  loadAppSpark(app.id);

  body.appendChild(html(`<div class="subhead">Processes</div>`));
  for (const root of app.roots) renderProc(root, body, 0);

  body.scrollTop = scroll;
}

async function loadAppSpark(key) {
  const rows = await invoke("app_history", { key, spanSecs: 86400 });
  const host = $("app-spark");
  // The panel redraws constantly; by the time this resolves the user may be
  // looking at something else entirely.
  if (!host || state.selected !== key) return;
  if (!rows?.length) {
    host.innerHTML = `<div class="chart-empty" style="min-height:34px;font-size:11px">No history for this app yet</div>`;
    return;
  }
  drawSparkline(host, rows.map((r) => [r.t, r.cpu_pm / 10]), cssVar("--cat-1"));
}

function countRoles(nodes, acc = new Map()) {
  for (const n of nodes) {
    acc.set(n.role.label, (acc.get(n.role.label) || 0) + 1);
    countRoles(n.children, acc);
  }
  return acc;
}

function renderProc(node, host, depth) {
  const el = document.createElement("div");
  el.className = "proc";
  el.style.paddingLeft = `${depth * 12}px`;

  const own = cpuPct(node.self_usage);
  const sub = cpuPct(node.subtree_usage);
  const hasKids = node.children.length > 0;

  // Both figures, always. The subtree total is the honest headline, and
  // showing the process's own beside it is what stops an idle-looking
  // parent from reading as a bug.
  const cpuText = hasKids
    ? `${fmtPct(sub)}%<span style="color:var(--ink-muted)"> · ${fmtPct(own)}% own</span>`
    : `${fmtPct(own)}%`;

  el.innerHTML = `
    <div class="proc-main">
      <span class="proc-name"></span>
      <span class="proc-role"></span>
      <span class="proc-cpu num">${cpuText}</span>
    </div>`;

  el.querySelector(".proc-name").textContent = node.window_title || node.comm;
  el.querySelector(".proc-role").textContent = node.role.label;

  // Quiet inline explanation — the single sentence that answers the
  // question this whole project started from.
  if (node.self_usage.cpu_cores < 0.02 && sub - own > 5 / state.cpuCount) {
    const n = countDescendants(node);
    el.appendChild(
      html(`<div class="proc-hint">Idle itself — the work is in the ${n} process${n === 1 ? "" : "es"} below it.</div>`),
    );
  }

  // The longer "why does this exist" is one click away, so the list stays
  // scannable. Whether it is open lives in state, not in the DOM: the panel
  // redraws every second as the numbers change, and reading the DOM for the
  // answer meant the text you were halfway through vanished each refresh.
  const main = el.querySelector(".proc-main");
  main.style.cursor = "pointer";
  main.setAttribute("role", "button");
  main.setAttribute("aria-expanded", String(state.expanded.has(node.pid)));

  if (state.expanded.has(node.pid)) {
    el.appendChild(html(`<div class="proc-why">${escapeHtml(node.role.explanation)}</div>`));
    const cmd = state.cmdlines.get(node.pid);
    if (cmd?.length) {
      el.appendChild(html(`<div class="proc-cmd">${escapeHtml(cmd.join(" ").slice(0, 400))}</div>`));
    }
  }

  main.onclick = async () => {
    if (state.expanded.has(node.pid)) {
      state.expanded.delete(node.pid);
      renderDetail();
      return;
    }
    state.expanded.add(node.pid);
    renderDetail();
    // Fetched once and cached, so reopening is instant and a refresh mid-read
    // does not re-ask the backend.
    if (!state.cmdlines.has(node.pid)) {
      const cmd = await invoke("process_cmdline", { pid: node.pid });
      state.cmdlines.set(node.pid, cmd || []);
      if (state.expanded.has(node.pid)) renderDetail();
    }
  };

  host.appendChild(el);
  for (const c of node.children) renderProc(c, host, depth + 1);
}

const countDescendants = (n) =>
  n.children.reduce((a, c) => a + 1 + countDescendants(c), 0);

/* ==================================================================
   History

   The live view can only ever show the moment it is running. Everything
   before that comes from the collector, and when it has not been run
   the graphs say so rather than showing a convincing flat line.
   ================================================================== */

const pmPct = (pm) => pm / 10;

async function loadHistory() {
  const rows = await invoke("system_history", { spanSecs: state.span });
  state.history = rows || [];
  renderPerf();
}

function renderPerf() {
  if (state.tab !== "perf") return;

  const rows = state.history;
  const status = rows.length
    ? `${rows.length} readings · one point every ${describeStep(rows)}`
    : "";
  $("perf-note").textContent = status ? ` — ${status}` : "";

  const empty = state.collectorSeen === false
    ? "No history has been recorded yet. The background collector is what fills these graphs — it may not be installed."
    : "Nothing recorded for this range yet. The collector writes its first points within a minute of starting.";

  drawChart($("chart-cpu"), {
    series: [{ name: "Processor", color: cssVar("--cat-1"),
               points: rows.map((r) => [r.t, pmPct(r.cpu_pm)]) }],
    yMax: 100,
    yFormat: (v) => `${Math.round(v)}%`,
    height: 168,
    empty,
  });

  const totalMb = state.model ? state.model.mem.total / (1024 * 1024) : 0;
  drawChart($("chart-mem"), {
    series: [{ name: "Memory", color: cssVar("--cat-1"),
               points: rows.map((r) => [r.t, r.mem_mb / 1024]) }],
    // Scaled against installed memory, so the line means something absolute
    // rather than filling the frame whatever the numbers are.
    yMax: totalMb ? totalMb / 1024 : undefined,
    yFormat: (v) => `${v.toFixed(0)} GB`,
    height: 168,
    empty,
  });

  const psi = [
    { name: "Processor", color: cssVar("--cat-1"), key: "psi_cpu_pm" },
    { name: "Memory", color: cssVar("--cat-2"), key: "psi_mem_pm" },
    { name: "Disk", color: cssVar("--cat-3"), key: "psi_io_pm" },
  ];
  drawChart($("chart-psi"), {
    series: psi.map((p) => ({
      name: p.name,
      color: p.color,
      points: rows.map((r) => [r.t, pmPct(r[p.key])]),
    })),
    yMax: 100,
    yFormat: (v) => `${Math.round(v)}%`,
    height: 168,
    empty,
  });

  // Three series share this frame, so a legend is not optional.
  const legend = $("psi-legend");
  legend.innerHTML = "";
  for (const p of psi) {
    const item = document.createElement("div");
    item.className = "legend-item";
    item.innerHTML = `<span class="legend-swatch"></span><span>${p.name}</span>`;
    item.querySelector(".legend-swatch").style.background = p.color;
    legend.appendChild(item);
  }
}

function describeStep(rows) {
  if (rows.length < 2) return "sample";
  const step = rows[1].t - rows[0].t;
  if (step < 60) return `${step} seconds`;
  if (step < 3600) return `${Math.round(step / 60)} minutes`;
  return `${Math.round(step / 3600)} hours`;
}

function setTab(tab, remember = true) {
  state.tab = tab;
  if (remember) {
    const p = loadPrefs();
    p.tab = tab;
    savePrefs(p);
  }
  $("tab-apps").setAttribute("aria-selected", String(tab === "apps"));
  $("tab-perf").setAttribute("aria-selected", String(tab === "perf"));
  $("view-apps").hidden = tab !== "apps";
  $("view-perf").hidden = tab !== "perf";
  if (tab === "perf") loadHistory();
  else if (state.model) renderMap(state.model);
}

function setSpan(span) {
  state.span = span;
  const p = loadPrefs();
  p.span = span;
  savePrefs(p);
  for (const b of $("ranges").querySelectorAll("button")) {
    b.setAttribute("aria-pressed", String(Number(b.dataset.span) === span));
  }
  loadHistory();
}

/* ==================================================================
   Tooltip
   ================================================================== */

const tip = () => $("tip");

function showTip(t, ev) {
  const el = tip();
  const name = t.name;
  const value = valueTextFor(t);
  const why = t.why ? `<div class="tip-why">${escapeHtml(t.why)}</div>` : "";
  const extra = t.isOther ? `<div class="tip-row">${t.count} smaller items combined</div>` : "";
  el.innerHTML = `<div class="tip-title">${escapeHtml(name)}</div>
    <div class="tip-row">${escapeHtml(t.sub || "")}${value ? ` · ${value}` : ""}</div>${extra}${why}`;
  el.dataset.show = "true";
  moveTip(ev);
}

function moveTip(ev) {
  const el = tip();
  const pad = 14;
  const r = el.getBoundingClientRect();
  let x = ev.clientX + pad;
  let y = ev.clientY + pad;
  if (x + r.width > window.innerWidth - 8) x = ev.clientX - r.width - pad;
  if (y + r.height > window.innerHeight - 8) y = ev.clientY - r.height - pad;
  el.style.left = `${Math.max(8, x)}px`;
  el.style.top = `${Math.max(8, y)}px`;
}

const hideTip = () => { tip().dataset.show = "false"; };

/* ==================================================================
   Wiring
   ================================================================== */

function html(markup) {
  const t = document.createElement("template");
  t.innerHTML = markup.trim();
  return t.content.firstElementChild;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

const tauri = () => window.__TAURI__;

async function invoke(cmd, args) {
  try {
    return await tauri().core.invoke(cmd, args);
  } catch (err) {
    // Surface it rather than sitting on "Measuring…" forever. A denied
    // command usually means a missing capability, which is invisible
    // otherwise.
    fail(`Could not reach the backend (${cmd}): ${err}`);
    return null;
  }
}

function fail(message) {
  const el = $("banner");
  if (!el) return;
  el.hidden = false;
  el.innerHTML = `<strong>Something is not connected.</strong> ${escapeHtml(message)}`;
}

/* ==================================================================
   Appearance

   Both settings are per-viewer conveniences, so localStorage is the
   right home for them; every access is guarded because it throws
   outright in some contexts.
   ================================================================== */

const PREFS = "clearview.appearance";

function loadPrefs() {
  try {
    return JSON.parse(localStorage.getItem(PREFS) || "{}");
  } catch {
    return {};
  }
}

function savePrefs(p) {
  try {
    localStorage.setItem(PREFS, JSON.stringify(p));
  } catch {
    /* a read-only store is not worth failing over */
  }
}

function applyAppearance(prefs) {
  const root = document.documentElement;
  if (prefs.translucent) root.dataset.translucent = "true";
  else delete root.dataset.translucent;

  if (prefs.theme === "light") root.dataset.theme = "light";
  else delete root.dataset.theme;

  $("toggle-translucent").setAttribute("aria-pressed", String(!!prefs.translucent));
  $("toggle-theme").setAttribute("aria-pressed", String(prefs.theme === "light"));
  $("toggle-theme").textContent = prefs.theme === "light" ? "Dark" : "Light";

  // Tiles cache no colour, but the map reads its hues from CSS at paint
  // time, so it has to be redrawn when the palette underneath changes.
  if (state.tab === "perf") renderPerf();
  else if (state.model) {
    renderMap(state.model);
    renderList(state.model);
  }
}

function setMetric(metric) {
  state.metric = metric;
  $("metric-cpu").setAttribute("aria-pressed", String(metric === "cpu"));
  $("metric-mem").setAttribute("aria-pressed", String(metric === "mem"));
  if (state.model) renderMap(state.model);
}

function apply(model) {
  state.model = model;
  renderSummary(model);
  // Drawing a hidden view is wasted work, and the treemap in particular
  // measures a zero-width container and lays out nothing.
  if (state.tab === "apps") {
    renderMap(model);
    renderList(model);
  }
  if (state.selected) renderDetail();
}

/* Anything that throws in a callback would otherwise vanish: the page keeps
   running with stale content and no indication that it stopped updating.
   Twice now that turned a one-line bug into a hunt. */
function wireErrorReporting() {
  window.addEventListener("error", (e) =>
    fail(`${e.message} — ${String(e.filename).split("/").pop()}:${e.lineno}`));
  window.addEventListener("unhandledrejection", (e) =>
    fail(`Unhandled: ${e.reason?.message || e.reason}`));
}

function wire() {
  wireErrorReporting();
  $("metric-cpu").onclick = () => setMetric("cpu");
  $("metric-mem").onclick = () => setMetric("mem");
  $("detail-close").onclick = closeDetail;
  $("crumb-back").onclick = drillOut;
  $("crumb-root").onclick = drillOut;
  $("tab-apps").onclick = () => setTab("apps");
  $("tab-perf").onclick = () => setTab("perf");
  for (const b of $("ranges").querySelectorAll("button")) {
    b.onclick = () => setSpan(Number(b.dataset.span));
  }

  $("toggle-translucent").onclick = () => {
    const p = loadPrefs();
    p.translucent = !p.translucent;
    savePrefs(p);
    applyAppearance(p);
  };

  $("toggle-theme").onclick = () => {
    const p = loadPrefs();
    p.theme = p.theme === "light" ? "dark" : "light";
    savePrefs(p);
    applyAppearance(p);
  };

  const map = $("map");
  map.addEventListener("click", (ev) => {
    const tileEl = ev.target.closest(".tile");
    // Clicking the background still steps back out, as a shortcut for
    // people who find it — the breadcrumb is what makes it discoverable.
    if (!tileEl) return drillOut();
    const t = tileEl._tile;
    if (t.isOther) return;
    if (t.app) {
      state.drilledApp = t.app.id;
      openDetail(t.app.id);
      renderMap(state.model);
    }
  });

  map.addEventListener("mousemove", (ev) => {
    const tileEl = ev.target.closest(".tile");
    if (!tileEl) return hideTip();
    showTip(tileEl._tile, ev);
  });
  map.addEventListener("mouseleave", hideTip);

  document.addEventListener("keydown", (ev) => {
    if (ev.key !== "Escape") return;
    if (state.drilledApp) drillOut();
    else closeDetail();
  });

  let raf;
  window.addEventListener("resize", () => {
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(() => {
      if (state.tab === "perf") renderPerf();
      else if (state.model) renderMap(state.model);
    });
  });
}

async function start() {
  wire();
  const prefs = loadPrefs();
  applyAppearance(prefs);

  const api = tauri();
  if (!api) {
    fail("The Tauri bridge is not present on the page.");
    return;
  }

  // Subscribe before anything else is awaited. Live data is the one thing the
  // window cannot do without, and putting the subscription last meant a single
  // slow command upstream left the view frozen on its placeholders with no
  // indication anything was wrong.
  await api.event.listen("snapshot", (e) => apply(e.payload));

  const info = await invoke("machine_info");
  if (info) state.cpuCount = info.cpuCount || 1;

  const first = await invoke("latest_snapshot");
  if (first) apply(first);

  // Come back to whichever view was last open. Done after the live wiring, so
  // the history path can never hold up the live path.
  if (prefs.span) state.span = prefs.span;
  setSpan(state.span);
  if (prefs.tab === "perf") setTab("perf", false);

  const status = await invoke("history_status");
  state.collectorSeen = Boolean(status?.available);
  if (state.tab === "perf") renderPerf();

  // A slow refresh is plenty for a graph measured in minutes, and keeps the
  // history query off the once-a-second path.
  setInterval(() => state.tab === "perf" && loadHistory(), 15000);
}

start();
