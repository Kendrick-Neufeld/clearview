/* Squarified treemap layout.
 *
 * Area encodes the value. The squarified variant is used rather than the naive
 * slice-and-dice because the naive one degenerates into unreadable slivers the
 * moment values are uneven — which, in a process list, they always are.
 *
 * Bruls, Huizing & van Wijk (2000).
 */

function worstRatio(row, side) {
  if (row.length === 0) return Infinity;
  let sum = 0;
  let max = -Infinity;
  let min = Infinity;
  for (const it of row) {
    sum += it.area;
    if (it.area > max) max = it.area;
    if (it.area < min) min = it.area;
  }
  if (sum <= 0 || min <= 0 || side <= 0) return Infinity;
  const s2 = sum * sum;
  const d2 = side * side;
  return Math.max((d2 * max) / s2, s2 / (d2 * min));
}

/* Places one finished row against the short edge and returns the rectangle
 * that is left over for everything after it. */
function layoutRow(row, rect, out) {
  const sum = row.reduce((a, b) => a + b.area, 0);
  const alongHeight = rect.w >= rect.h;
  const depth = sum / (alongHeight ? rect.h : rect.w);
  let offset = 0;

  for (const item of row) {
    const length = item.area / depth;
    out.push({
      ...item,
      x: alongHeight ? rect.x : rect.x + offset,
      y: alongHeight ? rect.y + offset : rect.y,
      w: alongHeight ? depth : length,
      h: alongHeight ? length : depth,
    });
    offset += length;
  }

  return alongHeight
    ? { x: rect.x + depth, y: rect.y, w: Math.max(0, rect.w - depth), h: rect.h }
    : { x: rect.x, y: rect.y + depth, w: rect.w, h: Math.max(0, rect.h - depth) };
}

/** items: [{ value, ... }]. Returns the same objects with x/y/w/h added. */
export function squarify(items, width, height) {
  const out = [];
  const usable = items.filter((i) => i.value > 0);
  const total = usable.reduce((a, b) => a + b.value, 0);
  if (total <= 0 || width <= 0 || height <= 0) return out;

  // Largest first, which is what makes the squarified pass produce compact
  // rectangles — and also puts the answer in the top-left, where people look.
  const sorted = [...usable].sort((a, b) => b.value - a.value);
  const scale = (width * height) / total;
  const nodes = sorted.map((n) => ({ ...n, area: n.value * scale }));

  let rect = { x: 0, y: 0, w: width, h: height };
  let row = [];

  for (let i = 0; i < nodes.length; ) {
    const side = Math.min(rect.w, rect.h);
    const candidate = [...row, nodes[i]];
    // Keep adding to the row while it makes the aspect ratios better.
    if (row.length === 0 || worstRatio(candidate, side) <= worstRatio(row, side)) {
      row = candidate;
      i += 1;
    } else {
      rect = layoutRow(row, rect, out);
      row = [];
    }
  }
  if (row.length) layoutRow(row, rect, out);

  return out;
}

/** Keeps the map readable by folding the long tail into a single tile.
 *  A treemap with two hundred slivers communicates nothing. */
export function foldTail(items, maxTiles, otherLabel = "Everything else") {
  if (items.length <= maxTiles) return items;
  const sorted = [...items].sort((a, b) => b.value - a.value);
  const head = sorted.slice(0, maxTiles - 1);
  const tail = sorted.slice(maxTiles - 1);
  const value = tail.reduce((a, b) => a + b.value, 0);
  if (value <= 0) return head;
  return [
    ...head,
    { id: "__other__", name: otherLabel, value, count: tail.length, isOther: true },
  ];
}
