// Element construction for the views. Text is only ever added as text
// nodes, never parsed as HTML, so names and messages from downloads show
// exactly as they are; the page's Trusted Types policy makes any HTML
// parsing of strings fail outright.

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
 * Replaces `parent`'s children.
 * @param {Element} parent
 * @param {...Children} children
 */
export function show(parent, ...children) {
  parent.replaceChildren(...nodes(children));
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
