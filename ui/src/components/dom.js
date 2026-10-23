// Element construction for the views. Text is only ever added as text
// nodes, never parsed as HTML, so names and messages from downloads show
// exactly as they are; the page's Trusted Types policy makes any HTML
// parsing of strings fail outright.

import { icon } from './icons.js';

/**
 * @typedef {Node | string | number | null | undefined | false} Child
 * @typedef {Child | Child[]} Children
 */

/**
 * An element with `props` and `children`. A prop named `on<event>` adds a
 * listener; `true`/`false` set or leave out a boolean attribute; anything
 * else is set as an attribute. Strings and numbers among the children
 * become text.
 * @template {keyof HTMLElementTagNameMap} K
 * @param {K} tag
 * @param {Record<string, unknown> | null} [props]
 * @param {...Children} children
 * @returns {HTMLElementTagNameMap[K]}
 */
export function h(tag, props, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(props ?? {})) {
    if (name.startsWith('on') && typeof value === 'function') {
      element.addEventListener(name.slice(2), /** @type {EventListener} */ (value));
    } else if (value === true) {
      element.setAttribute(name, '');
    } else if (value !== false && value !== null && value !== undefined) {
      element.setAttribute(name, String(value));
    }
  }
  element.append(...nodes(children));
  return element;
}

/**
 * Updates `parent`'s children, leaving retained nodes connected and in place.
 * Detaching even an unchanged control can swallow an in-progress click.
 * @param {Element} parent
 * @param {...Children} children
 */
export function show(parent, ...children) {
  let at = parent.firstChild;
  for (const child of nodes(children)) {
    if (child === at || (typeof child === 'string' && at?.nodeType === Node.TEXT_NODE && at.textContent === child)) {
      at = at.nextSibling;
      continue;
    }
    const node = typeof child === 'string' ? document.createTextNode(child) : child;
    parent.insertBefore(node, at);
  }
  while (at) {
    const next = at.nextSibling;
    parent.removeChild(at);
    at = next;
  }
}

/**
 * @param {Children[]} children
 * @returns {(Node | string)[]}
 */
function nodes(children) {
  return children
    .flat()
    .filter((child) => child !== null && child !== undefined && child !== false)
    .map((child) => (typeof child === 'number' ? String(child) : /** @type {Node | string} */ (child)));
}

/**
 * The values of a form's named fields.
 * @param {HTMLFormElement} form
 * @returns {Record<string, string>}
 */
export function fields(form) {
  /** @type {Record<string, string>} */
  const values = {};
  for (const [name, value] of new FormData(form)) {
    values[name] = typeof value === 'string' ? value : '';
  }
  return values;
}

/**
 * The `data-key` of the focused element inside `root`, so that focus can
 * be put back after `root`'s content is rebuilt.
 * @param {Element} root
 * @returns {string | null}
 */
export function focusedKey(root) {
  const active = document.activeElement;
  return active instanceof HTMLElement && root.contains(active) ? (active.dataset.key ?? null) : null;
}

/**
 * Focuses the element with `data-key` `key` inside `root`, unless focus is
 * already on it.
 * @param {Element} root
 * @param {string | null} key
 */
export function refocus(root, key) {
  if (key === null || focusedKey(root) === key) return;
  const target = root.querySelector(`[data-key="${CSS.escape(key)}"]`);
  if (target instanceof HTMLElement) target.focus({ preventScroll: true });
}

/**
 * Marks a table row as the base or alternate shade by its position among
 * the rows shown, so the shading follows filtering, sorting and paging.
 * @template {HTMLElement} R
 * @param {R} row
 * @param {number} index the row's position, from 0
 */
export function stripe(row, index) {
  row.dataset.stripe = index % 2 === 1 ? 'alternate' : 'base';
  return row;
}

/**
 * A column header that sorts its table: a button that sorts by `column`,
 * marked with the order while the table is sorted by it.
 * @template {string} C
 * @param {{ column: C, label: string, sort: import('../client/sort.js').Sort<C | null>, onSort: (column: C) => void, className?: string }} header
 */
export function sortHeader({ column, label, sort, onSort, className }) {
  const active = sort.column === column;
  return h(
    'th',
    {
      scope: 'col',
      class: className ? `sortable ${className}` : 'sortable',
      'aria-sort': active ? (sort.descending ? 'descending' : 'ascending') : null,
    },
    h(
      'button',
      { type: 'button', class: 'sort', 'data-key': `sort-${column}`, onclick: () => onSort(column) },
      label,
      active && icon(sort.descending ? 'arrow-down' : 'arrow-up'),
    ),
  );
}

/**
 * Whether `event` is Ctrl+A (Cmd+A on a Mac) meant for a table's rows
 * rather than the page's text: pressed in `table`, or with nothing
 * focused while some rows are `selected`. Fields, menus and dialogs keep
 * their own Select all.
 * @param {KeyboardEvent} event
 * @param {Element} table
 * @param {boolean} selected
 */
export function isSelectAllKey(event, table, selected) {
  if (event.key.toLowerCase() !== 'a' || !(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey) return false;
  const target = /** @type {Element} */ (event.target);
  const inTable = table.contains(target) && !(target instanceof HTMLInputElement && target.type !== 'checkbox');
  return inTable || (target === document.body && selected);
}

/**
 * The bar under a paged table: which rows are shown out of how many, and
 * the way to the pages before and after. Both buttons stay in place,
 * disabled at either end, so paging never moves them.
 * @param {{ place: import('../client/pages.js').PagePlace, more: boolean, key: string, onPage: (direction: 'next' | 'back') => void }} pager
 */
export function pagerBar({ place, more, key, onPage }) {
  const { first, last, total, page, pages } = place;
  return [
    h('span', { class: 'range' }, first > last ? '' : total === null ? `${first}\u2013${last}` : `${first}\u2013${last} of ${total}`),
    h('span', { class: 'spacer' }),
    h(
      'button',
      { type: 'button', 'aria-label': 'Previous page', 'data-key': `${key}-back`, disabled: page === 1, onclick: () => onPage('back') },
      'Previous',
    ),
    h('span', { class: 'page' }, pages === null ? `Page ${page}` : `Page ${page} of ${pages}`),
    h(
      'button',
      { type: 'button', 'aria-label': 'Next page', 'data-key': `${key}-next`, disabled: !more, onclick: () => onPage('next') },
      'Next',
    ),
  ];
}

/**
 * Copies `text` to the clipboard: through the Clipboard API where the page
 * may use it, else through a selected, hidden field.
 * @param {string} text
 */
export async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // Not a secure context, or not allowed; fall back below.
  }
  const field = h('textarea', { class: 'visually-hidden', readonly: true, 'aria-hidden': 'true' });
  field.value = text;
  document.body.append(field);
  field.select();
  const copied = document.execCommand('copy');
  field.remove();
  if (!copied) throw new Error('The clipboard is not available.');
}
