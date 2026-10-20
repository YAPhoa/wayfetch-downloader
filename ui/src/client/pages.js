// Where a page of a cursor-paged list sits in the whole list: its rows'
// range, and its number out of how many. The engine says how many rows
// come before the page; totals may come from counts read apart from the
// page, so they may lag, and the page itself always wins.
// No DOM, so it is tested under Node.

/**
 * @typedef {{ first: number, last: number, total: number | null, page: number, pages: number | null }} PagePlace
 *   `first` > `last` when the page has no rows
 */

/**
 * @param {{ earlier: number, before: number | null, size: number, rows: number, more: boolean, total: number | null }} at
 *   `earlier` pages were paged through to reach this one, `before` rows
 *   come before it (when unknown, full earlier pages are counted), `rows` are on it, whether `more` follow, and the list's
 *   `total` if known
 * @returns {PagePlace}
 */
export function pagePlace({ earlier, before, size, rows, more, total }) {
  const offset = before ?? earlier * size;
  const first = offset + 1;
  const last = offset + rows;
  const page = earlier + 1;
  if (total === null) return { first, last, total: null, page, pages: null };
  const seen = Math.max(total, last + (more ? 1 : 0));
  return { first, last, total: seen, page, pages: page + Math.ceil((seen - last) / size) };
}
