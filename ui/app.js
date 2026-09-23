import { squarify, foldTail } from "./treemap.js";
import { drawChart, drawSparkline } from "./chart.js";
import {
  arrangeApps, bytesText, describeStep, escapeHtml, filterTree, findGrowth,
  findSpikes, fmtBytes, fmtPct, inkOn, isApproximate, rateText,
} from "./lib.js";

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
  query: "",            // list filter text
  procQuery: "",        // filter within the selected app's processes
  showHistory: false,   // per-app history charts, collapsed by default
  detailShell: null,    // which app the panel's stable parts were built for
  sort: "cpu",          // "cpu" | "mem" | "name"
  heldOrder: null,      // app ids, frozen while the pointer is in the list
  heldLayout: null,     // tile rectangles, frozen while the pointer is in the map
  span: 0,              // seconds of history shown; 0 means the live buffer
  history: [],          // machine series for the current span
  live: [],             // rolling per-second buffer, newest last
  spikes: { cpu: [], memory: [], gpu: [], network: [], disk: [] },
  ending: null,        // in-flight confirmation for closing an app
};

const $ = (id) => document.getElementById(id);

/* ==================================================================
   Formatting
   ================================================================== */

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

  // Holding the layout still while the pointer is over it is the difference
  // between a picture you can click and one that squirms away. Sizes are
  // recomputed every second, so without this the block being aimed at has
  // moved by the time the click lands. Values keep updating; only the
  // geometry is pinned.
  let laid;
  if (state.heldLayout) {
    const frozen = new Map(state.heldLayout.map((t) => [t.id, t]));
    laid = items
      .filter((it) => frozen.has(it.id))
      .map((it) => ({ ...it, ...pick(frozen.get(it.id), ["x", "y", "w", "h"]) }));
  } else {
    laid = squarify(items, rect.width, rect.height);
  }
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
  state.heldLayout = null;
  if (state.model) renderMap(state.model);
}

const pick = (obj, keys) => Object.fromEntries(keys.map((k) => [k, obj[k]]));

function holdLayout() {
  if (!state.model || state.heldLayout) return;
  const host = $("map");
  const rect = host.getBoundingClientRect();
  const { items } = mapItems(state.model);
  state.heldLayout = squarify(items, rect.width, rect.height);
}

function releaseLayout() {
  if (!state.heldLayout) return;
  state.heldLayout = null;
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

function orderedApps(m) {
  return arrangeApps(m.apps, {
    query: state.query,
    sort: state.sort,
    heldOrder: state.heldOrder,
  });
}

function renderList(m) {
  const host = $("list");
  const apps = orderedApps(m).slice(0, 80);

  $("list-hint").hidden = !state.heldOrder;
  if (state.heldOrder) {
    $("list-hint").textContent = "Order held while you are in the list — values are still live.";
  }

  if (!apps.length) {
    host.innerHTML = `<div class="list-empty">Nothing matches “${escapeHtml(state.query)}”.</div>`;
    return;
  }

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
      <div class="row-num num">${a.totals.mem_pss > 0 ? est + bytesText(a.totals.mem_pss) : "—"}</div>
      <div class="row-procs num">${a.process_count} proc${a.process_count === 1 ? "" : "s"}</div>`;

    row.querySelector(".row-dot").style.background = colorForKind(a.kind);
    row.querySelector(".row-title").textContent = a.name;
    row.querySelector(".row-sub").textContent = sub;
    row.onclick = () => openDetail(a.id);
    host.appendChild(row);
  }
}

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
  if (state.selected !== appId) {
    state.expanded.clear();
    state.ending = null;
    state.procQuery = "";
  }
  state.selected = appId;
  renderDetail();
  $("detail").dataset.open = "true";
  $("detail").setAttribute("aria-hidden", "false");
}

function closeDetail() {
  state.selected = null;
  state.detailShell = null;
  $("detail").dataset.open = "false";
  $("detail").setAttribute("aria-hidden", "true");
  if (state.model) renderList(state.model);
}

/* The panel refreshes on the one-second tick, and rebuilding all of it
 * destroyed two things that have to survive that.
 *
 * The filter field was recreated every second, so focus and the caret went
 * with it — the field could not actually be typed into. And the history charts
 * were redrawn from scratch each time, which read as a blink.
 *
 * So the panel is built once per application, into containers that are then
 * left alone, and only the parts whose numbers change are re-rendered. */
function renderDetail() {
  const m = state.model;
  if (!m || !state.selected) return;
  const app = m.apps.find((a) => a.id === state.selected);
  if (!app) return closeDetail();

  $("detail-title").textContent = app.name;
  $("detail-sub").textContent =
    app.windows[0] ||
    `${KIND_LABEL[app.kind]} · ${app.process_count} process${app.process_count === 1 ? "" : "es"}`;

  if (state.detailShell !== app.id) buildDetailShell(app);

  renderFacts(app);
  renderActions(app, $("d-actions"));
  renderProcessList(app);
}

/** The app the panel is showing, re-read rather than closed over: a handler
 *  installed once outlives every snapshot that follows it. */
function currentApp() {
  return state.model?.apps.find((a) => a.id === state.selected) || null;
}

function buildDetailShell(app) {
  state.detailShell = app.id;
  const body = $("detail-body");
  body.innerHTML = "";
  body.scrollTop = 0;

  body.appendChild(html(`<div id="d-facts"></div>`));
  body.appendChild(html(`<div class="history" id="d-history"></div>`));
  body.appendChild(html(`<div id="d-actions"></div>`));

  // The process list gets its own filter: an app with thirty processes is a
  // long scroll, and what is being looked for is usually known by name. Built
  // here, once, so the tick cannot take it away mid-word.
  const head = html(
    `<div class="proc-head">` +
      `<span class="subhead" style="margin:0">Processes</span>` +
      `<input class="search proc-search" id="proc-search" type="search" spellcheck="false"` +
      ` placeholder="Filter processes\u2026" aria-label="Filter processes" /></div>`,
  );
  const field = head.querySelector("#proc-search");
  field.value = state.procQuery;
  field.addEventListener("input", () => {
    state.procQuery = field.value;
    const now = currentApp();
    // Only the list is redrawn; the field being typed into is left alone.
    if (now) renderProcessList(now);
  });
  body.appendChild(head);
  body.appendChild(html(`<div id="d-procs"></div>`));

  renderHistory(app);
}

function renderFacts(app) {
  const over = app.totals.mem_rss - app.totals.mem_pss;
  const host = $("d-facts");
  host.innerHTML = "";

  host.appendChild(
    html(`<div class="facts">
      <div><div class="fact-label">Processor</div><div class="fact-value num">${fmtPct(cpuPct(app.totals))}%</div></div>
      <div><div class="fact-label">Memory</div><div class="fact-value num">${bytesText(app.totals.mem_pss)}</div></div>
      <div><div class="fact-label">Processes</div><div class="fact-value num">${app.process_count}</div></div>
      <div><div class="fact-label">Windows</div><div class="fact-value num">${app.windows.length}</div></div>
    </div>`),
  );

  // The memory correction, stated plainly, because it is large and nothing
  // else the user has run tells them about it.
  if (app.process_count > 1 && over > 64 * 1024 * 1024) {
    host.appendChild(
      html(`<div class="note">These ${app.process_count} processes share a lot of memory between them.
      The real cost is <strong>${bytesText(app.totals.mem_pss)}</strong>; adding up each process
      separately, as most task managers do, would report ${bytesText(app.totals.mem_rss)}.</div>`),
    );
  }

  if (app.process_count > 1) {
    const parts = [...countRoles(app.roots).entries()]
      .sort((a, b) => b[1] - a[1])
      .map(([label, n]) => `${n} × ${label.toLowerCase()}`)
      .join(", ");
    host.appendChild(html(`<div class="note">Made up of ${parts}.</div>`));
  }
}

/* Drawn when the panel opens and again only when it is toggled. These charts
 * cover a whole day; one second of new data cannot visibly change them, so
 * repainting on every tick bought nothing and cost a blink. */
function renderHistory(app) {
  const host = $("d-history");
  if (!host) return;
  host.innerHTML = "";

  const toggle = html(
    `<button class="disclosure" aria-expanded="${state.showHistory}">` +
      `<span class="disclosure-mark">${state.showHistory ? "\u25be" : "\u25b8"}</span>` +
      `Last 24 hours</button>`,
  );
  toggle.onclick = () => {
    state.showHistory = !state.showHistory;
    const saved = loadPrefs();
    saved.showHistory = state.showHistory;
    savePrefs(saved);
    const now = currentApp();
    if (now) renderHistory(now);
  };
  host.appendChild(toggle);

  if (!state.showHistory) return;

  host.appendChild(
    html(`<div class="sparks">
      <div class="spark-row"><span class="spark-label">Processor</span><div class="spark" id="spark-cpu"></div></div>
      <div class="spark-row"><span class="spark-label">Memory</span><div class="spark" id="spark-mem"></div></div>
      <div class="spark-row"><span class="spark-label">Graphics</span><div class="spark" id="spark-gpu"></div></div>
      <div class="spark-row"><span class="spark-label">Network</span><div class="spark" id="spark-net"></div></div>
    </div>`),
  );
  loadAppSpark(app.id);
}

function renderProcessList(app) {
  const host = $("d-procs");
  if (!host) return;
  host.innerHTML = "";

  const shown = filterTree(app.roots, state.procQuery);
  if (!shown.length) {
    host.appendChild(
      html(`<div class="list-empty">No process matches \u201c${escapeHtml(state.procQuery)}\u201d.</div>`),
    );
    return;
  }
  for (const root of shown) renderProc(root, host, 0);
}

/* Closing an application.
 *
 * Deliberately two steps with the consequence stated in between. The first
 * press only arms it; the warning appears above the button that carries it
 * out, not after. Nothing here force-kills on the first attempt: programs are
 * asked to close so they can save, and forcing is offered separately, only
 * once asking has visibly failed. */
function renderActions(app, host) {
  if (!host) return;
  host.innerHTML = "";
  const targets = collectTargets(app.roots);
  if (!targets.length) return;
  host.className = "actions";

  const ending = state.ending?.appId === app.id ? state.ending : null;

  if (!ending) {
    const button = html(
      `<button class="danger">Close ${escapeHtml(app.name)}</button>`,
    );
    button.onclick = () => {
      state.ending = { appId: app.id, stage: "confirm" };
      renderDetail();
    };
    host.appendChild(button);
    return;
  }

  const plural = targets.length === 1 ? "process" : `${targets.length} processes`;
  const panel = html(`<div class="confirm"></div>`);
  panel.appendChild(
    html(`<div class="confirm-title">Close ${escapeHtml(app.name)}?</div>`),
  );
  panel.appendChild(
    html(`<div>It will be asked to close its ${plural}. Anything unsaved is up to the program to handle.</div>`),
  );
  if (app.caution) {
    panel.appendChild(html(`<div class="confirm-caution">${escapeHtml(app.caution)}</div>`));
  }

  if (ending.stage === "confirm" || ending.stage === "working") {
    const row = html(`<div class="confirm-row"></div>`);
    const go = html(
      `<button class="danger" data-armed="true"${ending.stage === "working" ? " disabled" : ""}>${
        ending.stage === "working" ? "Closing…" : "Close it"
      }</button>`,
    );
    go.onclick = () => endApp(app, targets, false);
    const cancel = html(`<button class="ghost">Cancel</button>`);
    cancel.onclick = () => {
      state.ending = null;
      renderDetail();
    };
    row.append(go, cancel);
    panel.appendChild(row);
  }

  if (ending.message) {
    panel.appendChild(html(`<div class="confirm-result">${escapeHtml(ending.message)}</div>`));
  }

  if (ending.stage === "stubborn") {
    const row = html(`<div class="confirm-row"></div>`);
    const force = html(`<button class="danger" data-armed="true">Force it to stop</button>`);
    force.onclick = () => endApp(app, targets, true);
    const cancel = html(`<button class="ghost">Leave it</button>`);
    cancel.onclick = () => {
      state.ending = null;
      renderDetail();
    };
    row.append(force, cancel);
    panel.appendChild(row);
  }

  host.appendChild(panel);
}

function collectTargets(nodes, out = []) {
  for (const n of nodes) {
    out.push({ pid: n.pid, start_time: n.key.start_time });
    collectTargets(n.children, out);
  }
  return out;
}

async function endApp(app, targets, force) {
  state.ending = { appId: app.id, stage: "working" };
  renderDetail();

  const result = await invoke("terminate", { targets, force });
  if (!result) {
    state.ending = { appId: app.id, stage: "confirm", message: "Could not reach the backend." };
    return renderDetail();
  }

  if (result.refused.length) {
    state.ending = {
      appId: app.id,
      stage: "confirm",
      message: `Refused: ${result.refused.join("; ")}`,
    };
    return renderDetail();
  }

  if (force) {
    state.ending = null;
    return renderDetail();
  }

  // Give it a moment to shut down on its own before suggesting anything
  // heavier — most programs take a second or two to save and exit.
  state.ending = { appId: app.id, stage: "working", message: "Asked it to close…" };
  renderDetail();

  await new Promise((r) => setTimeout(r, 2500));
  const left = await invoke("still_running", { targets });
  if (!left?.length) {
    state.ending = null;
    return renderDetail();
  }

  state.ending = {
    appId: app.id,
    stage: "stubborn",
    message: `${left.length} ${left.length === 1 ? "process is" : "processes are"} still running. Forcing it means nothing gets saved.`,
  };
  renderDetail();
}

async function loadAppSpark(key) {
  const rows = await invoke("app_history", { key, spanSecs: 86400 });
  // The panel redraws constantly; by the time this resolves the user may be
  // looking at something else entirely.
  if (state.selected !== key || !state.showHistory) return;

  for (const [id, pick] of [
    ["spark-cpu", (r) => r.cpu_pm / 10],
    ["spark-mem", (r) => r.mem_mb],
    ["spark-gpu", (r) => r.gpu_pm / 10],
    ["spark-net", (r) => r.net_kbps],
  ]) {
    const host = $(id);
    if (!host) continue;
    if (!rows?.length) {
      host.innerHTML = `<div class="spark-empty">no history yet</div>`;
      continue;
    }
    const points = rows.map((r) => [r.t, pick(r)]);
    // A flat zero line says "this app never used it", which is worth showing
    // as emptiness rather than as a line pinned to the axis.
    if (points.every((point) => point[1] === 0)) {
      host.innerHTML = `<div class="spark-empty">none recorded</div>`;
      continue;
    }
    drawSparkline(host, points, cssVar("--cat-1"));
  }
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
    if (node.role.kind !== "Kernel") {
      const end = html(`<button class="proc-end">Close this process</button>`);
      end.onclick = async (ev) => {
        ev.stopPropagation();
        end.disabled = true;
        end.textContent = "Closing…";
        const targets = [{ pid: node.pid, start_time: node.key.start_time }];
        const r = await invoke("terminate", { targets, force: false });
        end.textContent = r?.refused.length ? r.refused[0] : "Asked it to close";
      };
      el.appendChild(html(`<div style="margin-top:var(--s-2)"></div>`)).appendChild(end);
    }
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
  if (state.span === 0) {
    // Nothing to fetch: the live view is the buffer this page has been
    // filling from the stream.
    state.history = [];
    state.spikes = { cpu: [], memory: [], gpu: [], network: [], disk: [] };
    renderPerf();
    return;
  }
  const [rows, cpu, memory, gpu, network, disk] = await Promise.all([
    invoke("system_history", { spanSecs: state.span }),
    invoke("spikes", { spanSecs: state.span, metric: "cpu" }),
    invoke("spikes", { spanSecs: state.span, metric: "memory" }),
    invoke("spikes", { spanSecs: state.span, metric: "gpu" }),
    invoke("spikes", { spanSecs: state.span, metric: "network" }),
    invoke("spikes", { spanSecs: state.span, metric: "disk" }),
  ]);
  state.history = rows || [];
  state.spikes = {
    cpu: cpu || [],
    memory: memory || [],
    gpu: gpu || [],
    network: network || [],
    disk: disk || [],
  };
  renderPerf();
}

/* The same outlier rules the backend applies to stored history, run over the
   live buffer — where the culprit is known per second rather than per minute. */
function liveSpikes(points, pick, nameOf, valueOf) {
  return findSpikes(points, pick).map((p) => ({
    t: p.t, value: pick(p), app: nameOf(p), app_value: valueOf(p), app_window: 1,
  }));
}

function liveGrowth(points) {
  return findGrowth(points, (p) => p.memGb).map((j) => ({
    t: j.t, value: j.growth, app: null, app_value: 0, app_window: 1,
  }));
}

function renderPerf() {
  if (state.tab !== "perf") return;

  const live = state.span === 0;
  const rows = live ? state.live : state.history;

  $("perf-note").textContent = live
    ? rows.length
      ? ` — last ${rows.length} second${rows.length === 1 ? "" : "s"}, updating every second`
      : " — starting"
    : rows.length
      ? ` — ${rows.length} readings · one point every ${describeStep(rows)}`
      : "";

  const empty = live
    ? "Collecting. The live view builds as the app runs — switch to Hour for what was recorded before now."
    : state.collectorSeen === false
      ? "No history has been recorded yet. The background collector is what fills these graphs — it may not be installed."
      : "Nothing recorded for this range yet. The collector writes its first points within a minute of starting.";

  const at = (fromLive, fromStored) => (live ? rows.map(fromLive) : rows.map(fromStored));

  // ---- Processor ------------------------------------------------------
  const cpuSpikes = live
    ? liveSpikes(rows, (p) => p.cpu, (p) => p.app, (p) => p.appCpu)
    : state.spikes.cpu;

  drawChart($("chart-cpu"), {
    series: [{
      name: "Processor",
      color: cssVar("--cat-1"),
      points: at((r) => [r.t, r.cpu], (r) => [r.t, pmPct(r.cpu_pm)]),
    }],
    yMax: 100,
    yFormat: (v) => `${Math.round(v)}%`,
    height: 168,
    empty,
    markers: markersFor(cpuSpikes, (s) => (live ? s.value : pmPct(s.value))),
  });
  renderSpikes($("spikes"), cpuSpikes, {
    live,
    format: (v) => `${fmtPct(live ? v : pmPct(v))}%`,
    appFormat: (v) => `${fmtPct(live ? v : pmPct(v))}% of the machine`,
  });
  renderHardware();

  // ---- Memory ---------------------------------------------------------
  const memSpikes = live ? liveGrowth(rows) : state.spikes.memory;
  const totalGb = state.model ? state.model.mem.total / 1024 ** 3 : undefined;
  drawChart($("chart-mem"), {
    series: [{
      name: "Memory",
      color: cssVar("--cat-1"),
      points: at((r) => [r.t, r.memGb], (r) => [r.t, r.mem_mb / 1024]),
    }],
    yMax: totalGb,
    yFormat: (v) => `${v.toFixed(0)} GB`,
    height: 168,
    empty,
  });
  renderSpikes($("spikes-mem"), memSpikes, {
    live,
    heading: "Biggest increases",
    format: (v) => `+${(live ? v : v / 1024).toFixed(1)} GB`,
    appFormat: (v) => `grew by ${(v / 1024).toFixed(1)} GB`,
    note: live
      ? "The moments memory rose fastest. Per-app attribution needs the collector — switch to Hour."
      : null,
  });

  // ---- Graphics -------------------------------------------------------
  const gpuSpikes = live
    ? liveSpikes(rows, (p) => p.gpu, (p) => p.gpuApp, (p) => p.gpuAppPct)
    : state.spikes.gpu;
  drawChart($("chart-gpu"), {
    series: [{
      name: "Graphics",
      color: cssVar("--cat-1"),
      points: at((r) => [r.t, r.gpu], (r) => [r.t, pmPct(r.gpu_pm)]),
    }],
    yMax: 100,
    yFormat: (v) => `${Math.round(v)}%`,
    height: 168,
    empty,
    markers: markersFor(gpuSpikes, (s) => (live ? s.value : pmPct(s.value))),
  });
  renderSpikes($("spikes-gpu"), gpuSpikes, {
    live,
    format: (v) => `${fmtPct(live ? v : pmPct(v))}%`,
    appFormat: (v) => `${fmtPct(live ? v : pmPct(v))}% of the GPU`,
  });
  renderGpuHardware();

  // ---- Network --------------------------------------------------------
  const netSpikes = live
    ? liveSpikes(rows, (p) => p.rxKb + p.txKb, (p) => p.netApp, (p) => p.netAppKb)
    : state.spikes.network;
  drawChart($("chart-net"), {
    series: [
      {
        name: "Down",
        color: cssVar("--cat-1"),
        points: at((r) => [r.t, r.rxKb], (r) => [r.t, r.net_rx_kbps]),
      },
      {
        name: "Up",
        color: cssVar("--cat-2"),
        points: at((r) => [r.t, r.txKb], (r) => [r.t, r.net_tx_kbps]),
      },
    ],
    // One unit for the whole axis, chosen from its top value.
    yFormat: (v, max) =>
      (max ?? v) >= 1024
        ? `${(v / 1024).toFixed(1)} MB/s`
        : `${Math.round(v)} KB/s`,
    height: 168,
    yMin: 64,
    empty,
    markers: markersFor(netSpikes, (s) => s.value),
  });
  fillLegend($("net-legend"), [
    { name: "Down", color: cssVar("--cat-1") },
    { name: "Up", color: cssVar("--cat-2") },
  ]);
  renderSpikes($("spikes-net"), netSpikes, {
    live,
    heading: "Busiest moments",
    format: (v) => rateText(v),
    appFormat: (v) => `${rateText(v)} over TCP`,
    note: "Per-app figures count TCP only — UDP, and so most modern browser traffic, keeps no per-socket counter. The graph above counts everything, so it reads higher.",
  });

  // ---- Disk -----------------------------------------------------------
  const diskSpikes = live
    ? liveSpikes(rows, (p) => p.rdKb + p.wrKb, (p) => p.diskApp, (p) => p.diskAppKb)
    : state.spikes.disk;
  const busiest = state.model?.disks?.reduce((a, d) => Math.max(a, d.busy), 0) ?? 0;
  $("disk-note").textContent = state.model?.disks?.length
    ? `${state.model.disks.map((d) => d.name).join(", ")} — ${Math.round(busiest * 100)}% of the time with work in flight`
    : "";
  drawChart($("chart-disk"), {
    series: [
      {
        name: "Read",
        color: cssVar("--cat-1"),
        points: at((r) => [r.t, r.rdKb], (r) => [r.t, r.disk_rd_kbps]),
      },
      {
        name: "Write",
        color: cssVar("--cat-2"),
        points: at((r) => [r.t, r.wrKb], (r) => [r.t, r.disk_wr_kbps]),
      },
    ],
    yFormat: (v, max) =>
      (max ?? v) >= 1024 ? `${(v / 1024).toFixed(1)} MB/s` : `${Math.round(v)} KB/s`,
    height: 168,
    yMin: 64,
    empty,
    markers: markersFor(diskSpikes, (s) => s.value),
  });
  fillLegend($("disk-legend"), [
    { name: "Read", color: cssVar("--cat-1") },
    { name: "Write", color: cssVar("--cat-2") },
  ]);
  renderSpikes($("spikes-disk"), diskSpikes, {
    live,
    heading: "Heaviest moments",
    format: (v) => rateText(v),
    appFormat: (v) => `${rateText(v)} through the kernel`,
    note: "Per-app figures are what each program asked the kernel for. The graph above is what the drive actually did, so cached reads appear in one and not the other.",
  });

  // ---- Pressure -------------------------------------------------------
  const psi = [
    { name: "Processor", color: cssVar("--cat-1"), key: "psi_cpu_pm", i: 0 },
    { name: "Memory", color: cssVar("--cat-2"), key: "psi_mem_pm", i: 1 },
    { name: "Disk", color: cssVar("--cat-3"), key: "psi_io_pm", i: 2 },
  ];
  drawChart($("chart-psi"), {
    series: psi.map((p) => ({
      name: p.name,
      color: p.color,
      points: at((r) => [r.t, r.psi[p.i]], (r) => [r.t, pmPct(r[p.key])]),
    })),
    yMax: 100,
    yFormat: (v) => `${Math.round(v)}%`,
    height: 168,
    empty,
  });
  fillLegend($("psi-legend"), psi);
}

function markersFor(spikes, valueOf) {
  return spikes.map((s) => ({
    t: s.t,
    v: valueOf(s),
    color: cssVar("--cat-2"),
    label: s.app || null,
  }));
}

/* A legend is not optional once two series share a frame. */
function fillLegend(host, entries) {
  host.innerHTML = "";
  for (const e of entries) {
    const item = document.createElement("div");
    item.className = "legend-item";
    item.innerHTML = `<span class="legend-swatch"></span><span>${e.name}</span>`;
    item.querySelector(".legend-swatch").style.background = e.color;
    host.appendChild(item);
  }
}

/* Package temperature, power draw and clock speed — the context that says
   whether a busy processor is also a stressed one. */
function renderHardware() {
  const m = state.model;
  const host = $("cpu-hardware");
  if (!m) return;

  const facts = [];
  if (m.thermals.cpu_package_c != null) {
    facts.push(["Package", `${Math.round(m.thermals.cpu_package_c)}°C`, heat(m.thermals.cpu_package_c)]);
  }
  if (m.power_w != null) facts.push(["Drawing", `${m.power_w.toFixed(1)} W`, null]);
  if (m.core_mhz?.length) {
    const peak = Math.max(...m.core_mhz);
    const avg = Math.round(m.core_mhz.reduce((a, b) => a + b, 0) / m.core_mhz.length);
    facts.push(["Clock", `${(avg / 1000).toFixed(1)} GHz avg`, null]);
    facts.push(["Peak", `${(peak / 1000).toFixed(1)} GHz`, null]);
  }
  if (m.thermals.nvme_c != null) {
    facts.push(["Drive", `${Math.round(m.thermals.nvme_c)}°C`, heat(m.thermals.nvme_c, 70, 80)]);
  }
  host.innerHTML = facts
    .map(([label, value, level]) =>
      `<div class="hw"><span class="hw-label">${label}</span><span class="hw-value"${
        level ? ` data-heat="${level}"` : ""
      }>${value}</span></div>`)
    .join("");

  renderCoreGrid();
}

/* A temperature is a status, so it gets the reserved status colours — and it
   always shows its number, so the colour is never carrying the meaning alone. */
function heat(c, warm = 75, hot = 90) {
  if (c >= hot + 10) return "urgent";
  if (c >= hot) return "hot";
  if (c >= warm) return "warm";
  return null;
}

function renderCoreGrid() {
  const m = state.model;
  const host = $("core-grid");
  const busy = m.per_cpu_busy || [];
  if (!busy.length) return;

  const physical = m.core_of_cpu || [];
  const temps = m.thermals.cores_c || [];
  const steps = ["--seq-100", "--seq-250", "--seq-400", "--seq-550", "--seq-700"];

  $("cores-note").textContent = temps.length
    ? `${busy.length} threads on ${temps.length} cores — hyper-threads share a temperature`
    : `${busy.length} threads`;

  host.innerHTML = busy
    .map((fraction, i) => {
      const pct = fraction * 100;
      const step = steps[Math.min(steps.length - 1, Math.floor(fraction * steps.length))];
      const mhz = m.core_mhz?.[i];
      const temp = temps[physical[i]];
      const level = temp != null ? heat(temp) : null;
      return `<div class="core-cell">
        <div class="core-top">
          <span class="core-id">CPU ${i}</span>
          <span class="core-pct num">${Math.round(pct)}%</span>
        </div>
        <div class="core-bar"><div class="core-fill" style="width:${pct.toFixed(0)}%;background:var(${step})"></div></div>
        <div class="core-foot">
          <span>${mhz ? `${(mhz / 1000).toFixed(1)} GHz` : ""}</span>
          <span${level ? ` class="hw-value" data-heat="${level}"` : ""}>${
            temp != null ? `${Math.round(temp)}°C` : ""
          }</span>
        </div>
      </div>`;
    })
    .join("");
}

function renderGpuHardware() {
  const m = state.model;
  const host = $("gpu-hardware");
  if (!m?.gpus?.length) {
    host.innerHTML = "";
    $("gpu-note").textContent = "";
    return;
  }

  const integrated = m.gpus.find((g) => g.busy_from_clients);
  // Say where the number comes from. Summed client time misses work the driver
  // cannot attribute, so it is a floor rather than a measurement.
  $("gpu-note").textContent = integrated
    ? "built-in GPU — summed from what each program reports, so a slight underestimate"
    : "";

  host.innerHTML = m.gpus
    .map((g) => {
      const bits = [`<span class="hw-label">${escapeHtml(g.name)}</span>`];
      if (g.busy != null) bits.push(`<span class="hw-value">${Math.round(g.busy * 100)}%</span>`);
      if (g.temp_c != null) {
        bits.push(`<span class="hw-value" ${
          heat(g.temp_c, 75, 85) ? `data-heat="${heat(g.temp_c, 75, 85)}"` : ""
        }>${Math.round(g.temp_c)}°C</span>`);
      }
      if (g.power_w != null) bits.push(`<span class="hw-value">${g.power_w.toFixed(0)} W</span>`);
      if (g.mem_used_bytes) bits.push(`<span class="hw-value">${bytesText(g.mem_used_bytes)}</span>`);
      return `<div class="hw">${bits.join(" ")}</div>`;
    })
    .join("");
}

/* Every event is listed, whether or not its label fitted on the chart. */
function renderSpikes(host, spikes, opts) {
  host.hidden = spikes.length === 0;
  if (!spikes.length) return;

  const rows = spikes
    .slice()
    .sort((a, b) => b.value - a.value)
    .map((s) => {
      const when = new Date(s.t * 1000);
      const clock = `${String(when.getHours()).padStart(2, "0")}:${String(when.getMinutes()).padStart(2, "0")}`;
      const who = s.app
        ? `${escapeHtml(s.app)} <em>— ${opts.appFormat(s.app_value)}</em>`
        : `<em>no per-app record for this moment</em>`;
      return `<div class="spike-row">
        <span class="spike-time">${clock}</span>
        <span class="spike-value">${opts.format(s.value)}</span>
        <span class="spike-app">${who}</span>
      </div>`;
    })
    .join("");

  // Say how the attribution was made. Matching a five-second spike against a
  // one-minute average is a reasonable inference, not a measurement, and
  // presenting it as certainty would be the wrong kind of confident.
  const window = spikes[0]?.app_window ?? 60;
  const provenance = opts.live
    ? "Measured at the moment of each event."
    : `Attributed to the busiest app in the surrounding ${
        window >= 600 ? "ten minutes" : "minute"
      } — the finest per-app detail kept this far back. Use Live for exact attribution.`;

  host.innerHTML = `<div class="spikes-head">${opts.heading || "Biggest bursts"}</div>${rows}` +
    `<div class="spike-note">${opts.note ? `${opts.note} ` : ""}${provenance}</div>`;
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

function releaseOrder() {
  if (!state.heldOrder) return;
  state.heldOrder = null;
  if (state.model) renderList(state.model);
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

  const side = prefs.layout === "side";
  $("view-apps").dataset.layout = side ? "side" : "stacked";
  $("toggle-layout").setAttribute("aria-pressed", String(side));
  $("toggle-layout").textContent = side ? "Stacked" : "Side by side";

  // Tiles cache no colour, but the map reads its hues from CSS at paint
  // time, so it has to be redrawn when the palette underneath changes.
  // A palette change has to repaint the charts, which are otherwise never
  // redrawn once painted.
  state.detailShell = null;
  if (state.tab === "perf") renderPerf();
  else if (state.model) {
    renderMap(state.model);
    renderList(state.model);
  }
  if (state.selected) renderDetail();
}

function setMetric(metric) {
  state.metric = metric;
  state.heldLayout = null;
  $("metric-cpu").setAttribute("aria-pressed", String(metric === "cpu"));
  $("metric-mem").setAttribute("aria-pressed", String(metric === "mem"));
  if (state.model) renderMap(state.model);
}

/* Roughly five minutes of per-second readings. The database cannot hold this
   resolution — five seconds is its finest — so the live view is the only place
   a short burst is visible at the moment it happens. */
const LIVE_CAPACITY = 300;

function recordLive(model) {
  const cpus = state.cpuCount || 1;
  // The top app is captured at the instant of the sample, so a live spike is
  // attributed exactly rather than to whoever dominated the surrounding minute.
  const top = model.apps.find((a) => a.kind !== "Kernel") || model.apps[0];
  const byGpu = [...model.apps].sort((a, b) => b.totals.gpu_busy - a.totals.gpu_busy)[0];
  const byNet = [...model.apps].sort(
    (a, b) => b.totals.net_rx_bps + b.totals.net_tx_bps - (a.totals.net_rx_bps + a.totals.net_tx_bps),
  )[0];
  const byDisk = [...model.apps].sort(
    (a, b) =>
      b.totals.disk_read_bps + b.totals.disk_write_bps -
      (a.totals.disk_read_bps + a.totals.disk_write_bps),
  )[0];
  const integrated = model.gpus.find((g) => g.busy_from_clients);

  state.live.push({
    t: Math.floor(Date.now() / 1000),
    cpu: (model.cpu_busy ?? 0) * 100,
    memGb: (model.mem.total - model.mem.available) / 1024 ** 3,
    gpu: (integrated?.busy ?? 0) * 100,
    rxKb: model.interfaces.reduce((a, i) => a + i.rx_bps, 0) / 1024,
    txKb: model.interfaces.reduce((a, i) => a + i.tx_bps, 0) / 1024,
    rdKb: model.disks.reduce((a, d) => a + d.read_bps, 0) / 1024,
    wrKb: model.disks.reduce((a, d) => a + d.write_bps, 0) / 1024,
    diskBusy: model.disks.reduce((a, d) => Math.max(a, d.busy), 0) * 100,
    diskApp: byDisk ? byDisk.name : null,
    diskAppKb: byDisk
      ? (byDisk.totals.disk_read_bps + byDisk.totals.disk_write_bps) / 1024
      : 0,
    psi: [
      model.pressure.cpu?.some.avg10 ?? 0,
      model.pressure.memory?.some.avg10 ?? 0,
      model.pressure.io?.some.avg10 ?? 0,
    ],
    app: top?.name ?? null,
    appCpu: top ? (top.totals.cpu_cores / cpus) * 100 : 0,
    gpuApp: byGpu?.totals.gpu_busy > 0 ? byGpu.name : null,
    gpuAppPct: (byGpu?.totals.gpu_busy ?? 0) * 100,
    netApp: byNet ? byNet.name : null,
    netAppKb: byNet ? (byNet.totals.net_rx_bps + byNet.totals.net_tx_bps) / 1024 : 0,
  });
  if (state.live.length > LIVE_CAPACITY) state.live.shift();
}

function apply(model) {
  state.model = model;
  recordLive(model);
  noteWindowSource(model);
  renderSummary(model);
  // Drawing a hidden view is wasted work, and the treemap in particular
  // measures a zero-width container and lays out nothing.
  if (state.tab === "apps") {
    renderMap(model);
    renderList(model);
  }
  if (state.selected) renderDetail();
  // The live view is driven by the stream itself, not by a timer.
  if (state.tab === "perf" && state.span === 0) renderPerf();
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

/* Without window information, an application cannot be told from a background
   process, and a program the compositor launched can end up grouped with the
   compositor. Everything still works — but silently working differently is how
   a confusing list gets mistaken for a bug in the grouping. */
function noteWindowSource(model) {
  if (model.window_source || state.warnedNoWindows) return;
  state.warnedNoWindows = true;
  fail(
    "No compositor could be queried for window information, so applications " +
    "cannot be separated from background processes and may be grouped with " +
    "whatever launched them. Hyprland and niri are supported.",
  );
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

  // Filtering and sorting -------------------------------------------
  const search = $("list-search");
  search.addEventListener("input", () => {
    state.query = search.value;
    if (state.model) renderList(state.model);
    $("list").scrollTop = 0;
  });
  // A held order while typing would be actively unhelpful: the filter is
  // meant to resequence the list.
  search.addEventListener("focus", () => releaseOrder());

  for (const b of document.querySelectorAll("[data-sort]")) {
    b.onclick = () => {
      state.sort = b.dataset.sort;
      for (const other of document.querySelectorAll("[data-sort]")) {
        other.setAttribute("aria-pressed", String(other === b));
      }
      const p = loadPrefs();
      p.sort = state.sort;
      savePrefs(p);
      releaseOrder();
      if (state.model) renderList(state.model);
      $("list").scrollTop = 0;
    };
  }

  const list = $("list");
  list.addEventListener("pointerenter", () => {
    if (state.sort === "name" || !state.model) return;
    state.heldOrder = orderedApps(state.model).map((a) => a.id);
    renderList(state.model);
  });
  list.addEventListener("pointerleave", () => releaseOrder());

  $("toggle-layout").onclick = () => {
    const p = loadPrefs();
    p.layout = p.layout === "side" ? "stacked" : "side";
    savePrefs(p);
    applyAppearance(p);
  };

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
      state.heldLayout = null;
      state.drilledApp = t.app.id;
      openDetail(t.app.id);
      renderMap(state.model);
    }
  });

  map.addEventListener("pointerenter", holdLayout);
  map.addEventListener("pointerleave", () => {
    hideTip();
    releaseLayout();
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
      // A resize invalidates any pinned geometry.
      state.heldLayout = null;
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
  if (prefs.showHistory) state.showHistory = true;
  if (prefs.sort) {
    state.sort = prefs.sort;
    for (const b of document.querySelectorAll("[data-sort]")) {
      b.setAttribute("aria-pressed", String(b.dataset.sort === state.sort));
    }
  }
  setSpan(state.span);
  if (prefs.tab === "perf") setTab("perf", false);

  const status = await invoke("history_status");
  state.collectorSeen = Boolean(status?.available);
  if (state.tab === "perf") renderPerf();

  // A slow refresh is plenty for a graph measured in minutes, and keeps the
  // history query off the once-a-second path.
  // Stored history refreshes slowly; the live view is driven by the stream.
  setInterval(() => state.tab === "perf" && state.span !== 0 && loadHistory(), 15000);
}

start();
