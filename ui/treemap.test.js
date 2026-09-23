import test from "node:test";
import assert from "node:assert/strict";
import { squarify, foldTail } from "./treemap.js";

const W = 800;
const H = 500;

const overlaps = (a, b) =>
  a.x < b.x + b.w - 0.01 && b.x < a.x + a.w - 0.01 &&
  a.y < b.y + b.h - 0.01 && b.y < a.y + a.h - 0.01;

const items = (values) => values.map((value, i) => ({ id: `i${i}`, value }));

test("tile areas are proportional to their values", () => {
  const values = [50, 25, 12, 8, 5];
  const laid = squarify(items(values), W, H);
  const total = values.reduce((a, b) => a + b, 0);
  for (const tile of laid) {
    const expected = (tile.value / total) * W * H;
    const actual = tile.w * tile.h;
    assert.ok(
      Math.abs(actual - expected) / expected < 0.01,
      `${tile.id}: area ${actual.toFixed(0)} vs expected ${expected.toFixed(0)}`,
    );
  }
});

test("tiles fill the frame and never overlap", () => {
  const laid = squarify(items([40, 30, 15, 9, 4, 2]), W, H);
  const covered = laid.reduce((a, t) => a + t.w * t.h, 0);
  assert.ok(Math.abs(covered - W * H) / (W * H) < 0.01, "the frame is fully covered");

  for (let i = 0; i < laid.length; i += 1) {
    for (let j = i + 1; j < laid.length; j += 1) {
      assert.ok(!overlaps(laid[i], laid[j]), `${laid[i].id} overlaps ${laid[j].id}`);
    }
  }
});

test("no tile escapes the frame", () => {
  for (const t of squarify(items([9, 5, 3, 1]), W, H)) {
    assert.ok(t.x >= -0.01 && t.y >= -0.01, `${t.id} starts outside`);
    assert.ok(t.x + t.w <= W + 0.01 && t.y + t.h <= H + 0.01, `${t.id} runs past the edge`);
  }
});

test("the largest value is laid out first, at the origin", () => {
  const laid = squarify(
    [{ id: "small", value: 1 }, { id: "big", value: 90 }, { id: "mid", value: 9 }],
    W,
    H,
  );
  assert.equal(laid[0].id, "big");
  assert.ok(laid[0].x < 1 && laid[0].y < 1, "the answer belongs where people look first");
});

test("squarifying keeps rectangles usable rather than slivers", () => {
  // The failure mode of the naive slice-and-dice: very uneven values degrade
  // into unreadable strips.
  const laid = squarify(items([100, 50, 25, 12, 6, 3, 2, 1]), W, H);
  const worst = Math.max(...laid.map((t) => Math.max(t.w / t.h, t.h / t.w)));
  assert.ok(worst < 12, `worst aspect ratio ${worst.toFixed(1)} is a sliver`);
});

test("zero and negative values are dropped, not drawn", () => {
  const laid = squarify(
    [{ id: "a", value: 10 }, { id: "b", value: 0 }, { id: "c", value: -5 }],
    W,
    H,
  );
  assert.deepEqual(laid.map((t) => t.id), ["a"]);
});

test("degenerate inputs produce nothing rather than throwing", () => {
  assert.deepEqual(squarify([], W, H), []);
  assert.deepEqual(squarify(items([1, 2]), 0, H), []);
  assert.deepEqual(squarify(items([1, 2]), W, -4), []);
});

test("a single tile takes the whole frame", () => {
  const [only] = squarify(items([7]), W, H);
  assert.equal(only.w, W);
  assert.equal(only.h, H);
});

/* ── Folding the tail ────────────────────────────────────────────────── */

test("the long tail folds into one tile that preserves the total", () => {
  const values = [50, 20, 10, 5, 4, 3, 2, 1];
  const folded = foldTail(items(values), 4);
  assert.equal(folded.length, 4);
  assert.equal(folded.at(-1).id, "__other__");
  assert.equal(
    folded.reduce((a, t) => a + t.value, 0),
    values.reduce((a, b) => a + b, 0),
    "folding must not lose any of the total",
  );
  assert.equal(folded.at(-1).count, 5, "and it says how many it stands for");
});

test("a list already short enough is left alone", () => {
  const short = items([3, 2, 1]);
  assert.equal(foldTail(short, 10), short);
});

test("a tail worth nothing is dropped rather than folded into an empty tile", () => {
  const folded = foldTail(
    [{ id: "a", value: 10 }, { id: "b", value: 5 }, { id: "y", value: 0 }, { id: "z", value: 0 }],
    3,
  );
  assert.ok(!folded.some((t) => t.isOther), "an empty 'everything else' helps nobody");
  assert.deepEqual(folded.map((t) => t.id), ["a", "b"]);
});

test("a tail with value in it is folded, not discarded", () => {
  const folded = foldTail([{ id: "a", value: 10 }, { id: "b", value: 5 }], 2 - 1 + 1);
  assert.equal(folded.length, 2, "nothing to fold at this limit");
  const tight = foldTail([{ id: "a", value: 10 }, { id: "b", value: 5 }], 2);
  assert.equal(tight.length, 2);
});
