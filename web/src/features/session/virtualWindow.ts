export const DEFAULT_ROW = 96;
export const OVERSCAN = 8;

export type WindowRange = {
  start: number;
  end: number;
  padTop: number;
  padBottom: number;
  total: number;
};

function sizeAt(sizes: ArrayLike<number>, estimate: number, index: number): number {
  const value = index < sizes.length ? sizes[index] : 0;
  return value > 0 ? value : estimate;
}

/** Prefix offsets for `count` rows. `sizes[i] <= 0` falls back to `estimate`. */
export function rowOffsets(count: number, sizes: ArrayLike<number>, estimate = DEFAULT_ROW): { offsets: number[]; total: number } {
  const offsets = new Array<number>(count);
  let total = 0;
  for (let i = 0; i < count; i++) {
    offsets[i] = total;
    total += sizeAt(sizes, estimate, i);
  }
  return { offsets, total };
}

export function indexAtOffset(count: number, sizes: ArrayLike<number>, offset: number, estimate = DEFAULT_ROW): number {
  if (count <= 0) return 0;
  const { offsets, total } = rowOffsets(count, sizes, estimate);
  if (offset <= 0) return 0;
  if (offset >= total) return count - 1;
  let lo = 0;
  let hi = count - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const start = offsets[mid];
    const end = start + sizeAt(sizes, estimate, mid);
    if (offset < start) hi = mid - 1;
    else if (offset >= end) lo = mid + 1;
    else return mid;
  }
  return Math.min(count - 1, Math.max(0, lo));
}

export function visibleRange(
  count: number,
  sizes: ArrayLike<number>,
  scrollTop: number,
  viewport: number,
  overscan = OVERSCAN,
  estimate = DEFAULT_ROW,
): WindowRange {
  if (count <= 0) return { start: 0, end: 0, padTop: 0, padBottom: 0, total: 0 };
  const { offsets, total } = rowOffsets(count, sizes, estimate);
  const viewStart = Math.max(0, scrollTop);
  const viewEnd = viewStart + Math.max(viewport, 1);
  const first = indexAtOffset(count, sizes, viewStart, estimate);
  let last = first;
  while (last < count && offsets[last] < viewEnd) last += 1;
  const start = Math.max(0, first - overscan);
  const end = Math.min(count, last + overscan);
  const padTop = offsets[start] ?? 0;
  const endOffset = end <= 0 ? 0 : offsets[end - 1] + sizeAt(sizes, estimate, end - 1);
  return { start, end, padTop, padBottom: Math.max(0, total - endOffset), total };
}
