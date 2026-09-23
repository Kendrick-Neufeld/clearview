import test from "node:test";
import assert from "node:assert/strict";
import {
  arrangeApps, bytesText, contrastRatio, describeStep, escapeHtml, filterTree,
  findGrowth, findSpikes, fmtBytes, inkOn, isApproximate, rateText,
} from "./lib.js";

/* ── Colour ──────────────────────────────────────────────────────────── */

test("a label always takes the higher-contrast ink", () => {
  // Every colour the treemap can paint a tile. A previous version thresholded
  // on luminance and got all six wrong, putting white on fills where it
  // measured under 3:1.
  const palette = [
    "#2a78d6", "#1baf7a", "#eb6834", // light mode
    "#3987e5", "#199e70", "#d95926", // dark mode
  ];
  for (const fill of palette) {
    const chosen = inkOn(fill);
    const other = chosen === "#ffffff" ? "#000000" : "#ffffff";
    assert.ok(
      contrastRatio(fill, chosen) >= contrastRatio(fill, other),
      `${fill}: chose ${chosen} at ${contrastRatio(fill, chosen).toFixed(2)}:1 over ` +
        `${other} at ${contrastRatio(fill, other).toFixed(2)}:1`,
    );
  }
});

test("every tile colour clears 4.5:1 for its label", () => {
  for (const fill of ["#2a78d6", "#1baf7a", "#eb6834", "#3987e5", "#199e70", "#d95926"]) {
    const ratio = contrastRatio(fill, inkOn(fill));
    assert.ok(ratio >= 4.5, `${fill} only reaches ${ratio.toFixed(2)}:1`);
  }
});

test("contrast is symmetric and bounded", () => {
  assert.equal(contrastRatio("#000000", "#ffffff").toFixed(2), "21.00");
  assert.equal(contrastRatio("#ffffff", "#000000").toFixed(2), "21.00");
  assert.equal(contrastRatio("#123456", "#123456").toFixed(2), "1.00");
});

/* ── Formatting ──────────────────────────────────────────────────────── */

test("byte units switch at the right boundaries", () => {
  assert.deepEqual(fmtBytes(1023), { value: "1", unit: "KB" });
  assert.deepEqual(fmtBytes(1024 ** 2), { value: "1", unit: "MB" });
  assert.deepEqual(fmtBytes(1024 ** 3), { value: "1.0", unit: "GB" });
  assert.equal(bytesText(1536 * 1024 ** 2), "1.5 GB");
});

test("rates switch to megabytes only above a megabyte", () => {
  assert.equal(rateText(1023), "1023 KB/s");
  assert.equal(rateText(1024), "1.0 MB/s");
});

test("sample spacing is pluralised", () => {
  assert.equal(describeStep([{ t: 0 }, { t: 60 }]), "1 minute");
  assert.equal(describeStep([{ t: 0 }, { t: 120 }]), "2 minutes");
  assert.equal(describeStep([{ t: 0 }, { t: 5 }]), "5 seconds");
  assert.equal(describeStep([{ t: 0 }]), "sample");
});

test("markup in a process name cannot escape into the page", () => {
  assert.equal(escapeHtml('<img src=x onerror="bad">'), "&lt;img src=x onerror=&quot;bad&quot;&gt;");
});

test("a total is only called approximate when the gap is material", () => {
  assert.equal(isApproximate({ mem_pss: 1_000_000, mem_unmeasured: 2_000 }), false);
  assert.equal(isApproximate({ mem_pss: 1_000, mem_unmeasured: 900 }), true);
});

/* ── Outliers ────────────────────────────────────────────────────────── */

const flat = (n, v) => Array.from({ length: n }, (_, i) => ({ t: i, v }));

test("a quiet machine produces no spikes", () => {
  assert.deepEqual(findSpikes(flat(60, 3), (p) => p.v), []);
});

test("a burst is found, and a run of it counts once", () => {
  const points = flat(60, 2);
  // One sustained burst across five consecutive samples.
  for (let i = 30; i < 35; i += 1) points[i].v = 40 + i;
  const found = findSpikes(points, (p) => p.v);
  assert.equal(found.length, 1, "one burst should produce one mark");
  assert.equal(found[0].t, 34, "the mark goes on the run's peak");
});

test("two separate bursts are reported separately, newest last", () => {
  const points = flat(80, 1);
  points[10].v = 50;
  points[60].v = 70;
  const found = findSpikes(points, (p) => p.v);
  assert.deepEqual(found.map((p) => p.t), [10, 60]);
});

test("too few points to judge means no spikes claimed", () => {
  assert.deepEqual(findSpikes(flat(5, 1), (p) => p.v), []);
});

test("growth finds the moment of change, not the high level", () => {
  // Memory climbs once and then stays high. The event is the climb.
  const points = [];
  for (let i = 0; i < 20; i += 1) points.push({ t: i, v: i < 10 ? 2 : 9 });
  const found = findGrowth(points, (p) => p.v);
  assert.equal(found.length, 1);
  assert.equal(found[0].t, 10, "the jump, not the plateau after it");
});

test("a level that only falls produces no growth events", () => {
  const points = Array.from({ length: 20 }, (_, i) => ({ t: i, v: 100 - i }));
  assert.deepEqual(findGrowth(points, (p) => p.v), []);
});

/* ── Ordering ────────────────────────────────────────────────────────── */

const app = (id, name, cpu, mem, windows = []) => ({
  id, name, windows, totals: { cpu_cores: cpu, mem_pss: mem },
});

const sample = [
  app("a", "Vesktop", 0.5, 100),
  app("b", "Zen Browser", 0.1, 900, ["Some page — Zen Browser"]),
  app("c", "claude", 0.9, 400),
];

test("sorting by processor, memory and name each order correctly", () => {
  assert.deepEqual(arrangeApps(sample, { sort: "cpu" }).map((a) => a.id), ["c", "a", "b"]);
  assert.deepEqual(arrangeApps(sample, { sort: "mem" }).map((a) => a.id), ["b", "c", "a"]);
  // Name sorting ignores case, or "claude" would land after "Zen".
  assert.deepEqual(arrangeApps(sample, { sort: "name" }).map((a) => a.id), ["c", "a", "b"]);
});

test("filtering matches name, id and window title", () => {
  assert.deepEqual(arrangeApps(sample, { query: "ves" }).map((a) => a.id), ["a"]);
  assert.deepEqual(arrangeApps(sample, { query: "some page" }).map((a) => a.id), ["b"]);
  assert.deepEqual(arrangeApps(sample, { query: "nothing" }), []);
});

test("a held order survives the values changing underneath it", () => {
  const held = ["b", "a", "c"];
  // Processor use now says the opposite; the pinned arrangement must win.
  const got = arrangeApps(sample, { sort: "cpu", heldOrder: held }).map((a) => a.id);
  assert.deepEqual(got, held);
});

test("an app that appears while the order is held goes to the end", () => {
  const held = ["a", "b"];
  const withNew = [...sample, app("d", "New thing", 5, 5)];
  const got = arrangeApps(withNew, { sort: "cpu", heldOrder: held }).map((a) => a.id);
  assert.deepEqual(got.slice(0, 2), ["a", "b"], "existing rows keep their places");
  assert.ok(got.slice(2).includes("d"), "the newcomer is appended, not inserted");
});

/* ── Process tree filtering ──────────────────────────────────────────── */

const node = (pid, comm, label, children = []) => ({
  pid, comm, children, window_title: null, role: { label },
});

test("filtering a tree keeps the branch that leads to a match", () => {
  const tree = [
    node(1, "vesktop", "Main process", [
      node(2, "vesktop", "Process template", [node(3, "vesktop", "Window or tab")]),
      node(4, "vesktop", "Audio"),
    ]),
  ];
  const found = filterTree(tree, "window");
  assert.equal(found.length, 1, "the root is kept as the path to the match");
  assert.equal(found[0].children.length, 1, "the branch without a match is dropped");
  assert.equal(found[0].children[0].children[0].pid, 3);
});

test("filtering a tree matches on pid too", () => {
  const tree = [node(1, "a", "Main", [node(4242, "b", "Helper")])];
  assert.equal(filterTree(tree, "4242")[0].children[0].pid, 4242);
});

test("an empty filter returns the tree untouched", () => {
  const tree = [node(1, "a", "Main")];
  assert.equal(filterTree(tree, "  "), tree);
});

/* ── Gaps ────────────────────────────────────────────────────────────── */

import { axisGutter, segmentByGaps } from "./lib.js";

const evenly = (n, step = 60, from = 0) =>
  Array.from({ length: n }, (_, i) => [from + i * step, i]);

test("an unbroken series stays in one piece", () => {
  const points = evenly(20);
  const parts = segmentByGaps(points);
  assert.equal(parts.length, 1);
  assert.equal(parts[0].length, 20);
});

test("a machine switched off overnight leaves a hole, not a diagonal", () => {
  // The real shape: samples every minute, then 7.5 hours of nothing, then more.
  const before = evenly(30, 60, 0);
  const after = evenly(30, 60, 30 * 60 + 7.5 * 3600);
  const parts = segmentByGaps([...before, ...after]);
  assert.equal(parts.length, 2, "the two sessions must not be joined");
  assert.equal(parts[0].length, 30);
  assert.equal(parts[1].length, 30);
});

test("a single missed sample does not split the line", () => {
  // evenly(10) ends at t=540. One dropped sample means the next lands at 660,
  // a 120s gap where 60s is expected — within tolerance, so the line holds.
  const points = [...evenly(10), [660, 99], [720, 98]];
  assert.equal(segmentByGaps(points).length, 1);
});

test("a sustained outage does split the line", () => {
  // Several minutes with nothing recorded is a real absence, not a hiccup,
  // and joining across it would assert a value nobody measured.
  const points = [...evenly(10), [540 + 600, 99], [540 + 660, 98]];
  assert.equal(segmentByGaps(points).length, 2);
});

test("the expected spacing comes from the median, not the first gap", () => {
  // A long gap first, then a steady minute cadence. A naive implementation
  // takes the first gap as normal and then splits everywhere.
  const points = [[0, 1], [3600, 2], ...evenly(20, 60, 3660)];
  const parts = segmentByGaps(points);
  assert.equal(parts.length, 2, "only the leading hour-long gap should split");
  assert.equal(parts[1].length, 21);
});

test("degenerate series do not throw", () => {
  assert.deepEqual(segmentByGaps([]), []);
  assert.deepEqual(segmentByGaps([[5, 1]]), [[[5, 1]]]);
});

test("the axis gutter grows with the longest label", () => {
  const narrow = axisGutter(["0%", "50%", "100%"]);
  const wide = axisGutter(["0.0 MB/s", "48.8 MB/s", "195.3 MB/s"]);
  assert.ok(wide > narrow, "a wider label must reserve more room");
  assert.ok(wide >= "195.3 MB/s".length * 6, `${wide}px cannot hold "195.3 MB/s"`);
});
