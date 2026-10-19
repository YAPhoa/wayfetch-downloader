// Where a page of a cursor-paged list sits in the whole list: its rows'
// range, and its number out of how many. Totals come from counts read
// apart from the page, so they may lag; the page itself always wins.
// No DOM, so it is tested under Node.

/**
 * @typedef {{ first: number, last: number, total: number | null, page: number, pages: number | null }} PagePlace
 */

/**
 * @param {{ earlier: number, size: number, rows: number, more: boolean, total: number | null }} at
 *   `earlier` pages before this one of `size` rows each, `rows` on this
 *   one, whether `more` follow, and the list's `total` if known
 * @returns {PagePlace}
 */
export function pagePlace({ earlier, size, rows, more, total }) {
  const first = earlier * size + (rows > 0 ? 1 : 0);
  const last = earlier * size + rows;
  const page = earlier + 1;
  if (total === null) return { first, last, total: null, page, pages: null };
  const seen = Math.max(total, last + (more ? 1 : 0));
  return { first, last, total: seen, page, pages: Math.max(page + (more ? 1 : 0), Math.ceil(seen / size)) };
}
