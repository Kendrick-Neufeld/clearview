import test from "node:test";
import assert from "node:assert/strict";
import { niceTicks, timeLabel } from "./chart.js";

test("axis maxima round up to clean 1 / 2 / 5 steps", () => {
  // The cases that matter on screen: a percentage axis reads 0 / 50 / 100, and
  // installed memory of 31.1 GB reads 0 / 10 / 20 / 30.
  assert.deepEqual(niceTicks(100), { max: 100, step: 50 });
  assert.deepEqual(niceTicks(31.1), { max: 40, step: 10 });
  assert.deepEqual(niceTicks(7), { max: 8, step: 2 });
});

test("every step is a clean 1, 2 or 5 times a power of ten", () => {
  for (const v of [0.42, 3, 17, 96, 240, 1536, 91000]) {
    const { step } = niceTicks(v);
    const mag = 10 ** Math.floor(Math.log10(step));
    const norm = Number((step / mag).toPrecision(6));
    assert.ok([1, 2, 5, 10].includes(norm), `step ${step} for ${v} is not a clean number`);
  }
});

test("an axis carries between two and six gridlines", () => {
  // Fewer and the scale is unreadable; more and the grid competes with the data.
  for (const v of [0.42, 3, 17, 96, 240, 1536, 91000]) {
    const { max, step } = niceTicks(v);
    const lines = Math.round(max / step) + 1;
    assert.ok(lines >= 2 && lines <= 6, `${v} produced ${lines} gridlines`);
  }
});

test("an axis maximum carries no floating point noise", () => {
  assert.equal(niceTicks(0.42).max, 0.6);
  assert.equal(niceTicks(0.07).max, 0.08);
});

test("an axis maximum is never below the data it must show", () => {
  for (const v of [1, 3, 7, 19, 64, 101, 999, 4096]) {
    assert.ok(niceTicks(v).max >= v, `${v} would be clipped`);
  }
});

test("a flat or empty series still yields a drawable axis", () => {
  assert.deepEqual(niceTicks(0), { max: 1, step: 1 });
  assert.deepEqual(niceTicks(-3), { max: 1, step: 1 });
});

test("time labels show the clock up close and the date further out", () => {
  const t = Date.UTC(2026, 0, 15, 13, 45) / 1000;
  assert.match(timeLabel(t, 3600), /^\d{2}:\d{2}$/, "an hour of history wants the clock");
  assert.match(timeLabel(t, 7 * 86400), /^\d{1,2}\/\d{1,2}$/, "a week wants the date");
});
