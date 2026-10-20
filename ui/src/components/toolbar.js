// The toolbar every page starts with: the app's logo, the page's items,
// and a control at the end. The items wrap onto more rows as the window
// narrows; the logo and the end control keep the first row, the end
// control at its right, so nothing between them, however wide, moves it.

import { h } from './dom.js';

/**
 * @param {import('./dom.js').Child[]} items
 * @param {Node | null} [end]
 */
export function toolbar(items, end = null) {
  return h(
    'header',
    { class: 'toolbar' },
    h('span', { class: 'brand', role: 'img', 'aria-label': 'Wayfetch' }),
    h('div', { class: 'toolbar-main' }, items),
    end,
  );
}
