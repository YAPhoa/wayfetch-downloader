// How a table is sorted by one of its columns: clicking a column's header
// sorts by it, and clicking it again reverses the order (a third click may
// go back to the list's own order). Rows that tie, or that have no value
// for the column, keep their list order, the latter always last. No DOM,
// so it is tested under Node.

/**
 * @template {string | null} C
 * @typedef {{ column: C, descending: boolean }} Sort
 */

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

/**
 * The order after clicking `column`'s header while sorted by `current`.
 * @template {string} C
 * @param {Sort<C | null>} current
 * @param {C} column
 * @returns {Sort<C>}
 */
export function toggleSort(current, column) {
  return { column, descending: current.column === column ? !current.descending : false };
}

/**
 * Like `toggleSort`, but a third click on the column goes back to the
 * list's own order, which `column: null` stands for.
 * @template {string} C
 * @param {Sort<C | null>} current
 * @param {C} column
 * @returns {Sort<C | null>}
 */
export function cycleSort(current, column) {
  return current.column === column && current.descending ? { column: null, descending: false } : toggleSort(current, column);
}

/**
 * `items` sorted by the value `key` gives each, as a new array. Numbers
 * compare as numbers and text as people read it, ignoring case and with
 * numbers inside it in numeric order.
 * @template T
 * @param {readonly T[]} items
 * @param {(item: T) => string | number | null | undefined} key
 * @param {boolean} descending
 */
export function sortedBy(items, key, descending) {
  const sign = descending ? -1 : 1;
  return items
    .map((item, index) => ({ item, index, value: key(item) ?? null }))
    .sort((a, b) => {
      if (a.value === null || b.value === null) {
        return a.value === b.value ? a.index - b.index : a.value === null ? 1 : -1;
      }
      const order =
        typeof a.value === 'number' && typeof b.value === 'number'
          ? a.value - b.value
          : collator.compare(String(a.value), String(b.value));
      return order === 0 ? a.index - b.index : sign * order;
    })
    .map(({ item }) => item);
}

/**
 * A saved order read back, or `fallback` when there is none or it names a
 * column `columns` lacks. Storage may be blocked, so failures fall back.
 * @template {string | null} C
 * @param {string} key
 * @param {readonly C[]} columns
 * @param {Sort<C>} fallback
 * @returns {Sort<C>}
 */
export function savedSort(key, columns, fallback) {
  try {
    const saved = JSON.parse(globalThis.localStorage?.getItem(key) ?? 'null');
    if (columns.includes(saved?.column) && typeof saved.descending === 'boolean') {
      return { column: saved.column, descending: saved.descending };
    }
  } catch {
    // Unreadable or blocked storage: use the default.
  }
  return fallback;
}

/**
 * Keeps `sort` for the next visit, when storage allows.
 * @template {string | null} C
 * @param {string} key
 * @param {Sort<C>} sort
 */
export function saveSort(key, sort) {
  try {
    globalThis.localStorage?.setItem(key, JSON.stringify(sort));
  } catch {
    // Blocked storage: the order lasts until the page is closed.
  }
}
