// Small stroke icons shared by the views: 14×14 paths drawn in the text
// colour, always next to a visible or accessible label and hidden from
// assistive technology themselves.

/** Path data by name. */
export const ICONS = {
  settings:
    'M11.35 5.5L13 5.84v2.32l-1.65.34-.22.52.93 1.39-1.65 1.65-1.39-.93-.52.22L8.16 13H5.84l-.34-1.65-.52-.22-1.39.93' +
    '-1.65-1.65.93-1.39-.22-.52L1 8.16V5.84l1.65-.34.22-.52-.93-1.39 1.65-1.65 1.39.93.52-.22L5.84 1h2.32l.34 1.65' +
    '.52.22 1.39-.93 1.65 1.65-.93 1.39zM7 5a2 2 0 1 0 0 4a2 2 0 1 0 0-4',
  back: 'M11.5 7h-9M6 3L2 7l4 4',
  folder: 'M1.5 3h4l1.5 1.5h5.5v7h-11z',
  display: 'M2 3h10M2 7h10M2 11h10',
  accounts: 'M7 1.5a2.5 2.5 0 1 0 0 5a2.5 2.5 0 1 0 0-5M2.5 12.5c.4-2.6 2.2-4 4.5-4s4.1 1.4 4.5 4',
  plugins: 'M5.5 1.5v3h-3v3h3v5h3v-5h3v-3h-3v-3z',
  about: 'M7 1.5a5.5 5.5 0 1 0 0 11a5.5 5.5 0 1 0 0-11M7 6.5v3.5M7 4.25h.01',
  more: 'M3 7h.01M7 7h.01M11 7h.01',
  brand: 'M7 1.5v7.5M4 6l3 3 3-3M2.5 12h9',
  add: 'M7 2v10M2 7h10',
  pause: 'M4.5 3v8M9.5 3v8',
  resume: 'M4 2.5l7 4.5-7 4.5z',
  cancel: 'M3.5 3.5l7 7M10.5 3.5l-7 7',
  retry: 'M11.5 7a4.5 4.5 0 1 1-1.4-3.3M11.5 1.5v2.5H9',
  up: 'M7 11.5v-9M3.5 6L7 2.5 10.5 6',
  down: 'M7 2.5v9M3.5 8L7 11.5 10.5 8',
  chevron: 'M4 5.5l3 3 3-3',
  check: 'M2.5 7.5l3 3 6-6.5',
  external: 'M8.5 2.5h3v3M11.5 2.5L6.5 7.5M10 8.5v3H2.5V4h3',
  search: 'M6 2.5a3.5 3.5 0 1 0 0 7a3.5 3.5 0 1 0 0-7M8.6 8.6l3 3',
};

/** @typedef {keyof typeof ICONS} IconName */

/**
 * The icon named `name`.
 * @param {IconName} name
 */
export function icon(name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 14 14');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', `icon icon-${name}`);
  const line = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  line.setAttribute('d', ICONS[name]);
  svg.append(line);
  return svg;
}
