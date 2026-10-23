// Stroke icons shared by the views, drawn on a 24-unit grid in the text
// colour (paint comes from `.icon` in style.css; solid parts carry their own
// fill), always next to a visible or accessible label and hidden from
// assistive technology themselves.

/**
 * An SVG element: its tag, attributes and children.
 * @typedef {[string, Record<string, string>, ...Shape[]]} Shape
 */

/** Shapes by name, grouped by use. */
export const ICONS = /** @satisfies {Record<string, Shape[]>} */ ({
  // Transfers
  download: [['path', { d: 'M12 3.5v10.75M8 10.5l4 4 4-4M3.5 15.5v3.25A1.75 1.75 0 0 0 5.25 20.5h13.5a1.75 1.75 0 0 0 1.75-1.75V15.5' }]],
  'download-all': [['path', { d: 'M9 3.5v10M5.75 10.25 9 13.5l3.25-3.25M17 3.5v10M14.5 11l2.5 2.5 2.5-2.5M3.5 17v2a1.5 1.5 0 0 0 1.5 1.5h14a1.5 1.5 0 0 0 1.5-1.5v-2' }]],
  play: [['path', { d: 'M8.7 4.55c-.95-.58-2.2.1-2.2 1.22v12.46c0 1.12 1.25 1.8 2.2 1.22l10.1-6.23a1.43 1.43 0 0 0 0-2.44Z', fill: 'currentColor', stroke: 'none' }]],
  pause: [['rect', { x: '6', y: '4.5', width: '4', height: '15', rx: '1', fill: 'currentColor', stroke: 'none' }], ['rect', { x: '14', y: '4.5', width: '4', height: '15', rx: '1', fill: 'currentColor', stroke: 'none' }]],
  stop: [['rect', { x: '5.5', y: '5.5', width: '13', height: '13', rx: '1.75', fill: 'currentColor', stroke: 'none' }]],
  cancel: [['circle', { cx: '12', cy: '12', r: '8.5' }], ['path', { d: 'M6 18 18 6' }]],
  retry: [['path', { d: 'M19.75 9a8 8 0 1 0 .2 5.5M20 4.5v5h-5' }]],
  queue: [['path', { d: 'M4 5.5h16M4 11.5h9M4 17.5h6' }], ['path', { d: 'm16 13.25 4.5 3.25-4.5 3.25Z', fill: 'currentColor', stroke: 'none' }]],
  // Links
  link: [['path', { d: 'M10.1 7.9l2.4-2.4a4.24 4.24 0 0 1 6 6l-2.4 2.4M13.9 16.1l-2.4 2.4a4.24 4.24 0 0 1-6-6l2.4-2.4M8.8 15.2l6.4-6.4' }]],
  'add-link': [['g', { transform: 'translate(-.75 -.75) scale(.8)' }, ['path', { d: 'M10.1 7.9l2.4-2.4a4.24 4.24 0 0 1 6 6l-2.4 2.4M13.9 16.1l-2.4 2.4a4.24 4.24 0 0 1-6-6l2.4-2.4M8.8 15.2l6.4-6.4' }]], ['path', { d: 'M17.5 13.5v8M13.5 17.5h8' }]],
  unlink: [['path', { d: 'M13.1 5.1a4.2 4.2 0 0 1 5.8 6.1l-2.2 2.2M10.9 18.9a4.2 4.2 0 0 1-5.8-6.1l2.2-2.2M3.5 3.5l17 17' }]],
  'paste-links': [['path', { d: 'M8 5H6a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2h-2M8 11.5h8M8 16h6' }], ['rect', { x: '8', y: '2.5', width: '8', height: '5', rx: '1.5' }]],
  crawl: [['path', { d: 'M12 6v4M4.5 16v-4.5A1.5 1.5 0 0 1 6 10h12a1.5 1.5 0 0 1 1.5 1.5V16M12 10v6' }], ['circle', { cx: '12', cy: '4', r: '2' }], ['circle', { cx: '4.5', cy: '18.5', r: '2.25' }], ['circle', { cx: '12', cy: '18.5', r: '2.25' }], ['circle', { cx: '19.5', cy: '18.5', r: '2.25' }]],
  'external-link': [['path', { d: 'M10 4H5.5a2 2 0 0 0-2 2v12.5a2 2 0 0 0 2 2H18a2 2 0 0 0 2-2V14M13 3.5h7.5V11M20 4 10.5 13.5' }]],
  copy: [['path', { d: 'M8 6V4.5A1.5 1.5 0 0 1 9.5 3h10A1.5 1.5 0 0 1 21 4.5v10a1.5 1.5 0 0 1-1.5 1.5H18' }], ['rect', { x: '3.5', y: '8', width: '12.5', height: '13', rx: '2' }]],
  search: [['circle', { cx: '10.5', cy: '10.5', r: '6.5' }], ['path', { d: 'm15.25 15.25 5 5' }]],
  // Organization
  folder: [['path', { d: 'M3.5 7.5V5.75A2.25 2.25 0 0 1 5.75 3.5h3.5l2.5 3h6.5a2.25 2.25 0 0 1 2.25 2.25v9.5a2.25 2.25 0 0 1-2.25 2.25H5.75a2.25 2.25 0 0 1-2.25-2.25Z' }]],
  'folder-open': [['path', { d: 'M3 18V6a2 2 0 0 1 2-2h4l2.5 3H18a2 2 0 0 1 2 2v1M5 10.5h15.25a1.5 1.5 0 0 1 1.45 1.9l-1.65 6a2.15 2.15 0 0 1-2.07 1.6H5.15A2.15 2.15 0 0 1 3 17.85v-.35l1.05-5.8A1.15 1.15 0 0 1 5 10.5Z' }]],
  'folder-download': [['path', { d: 'M3.5 7.5V5.75A2.25 2.25 0 0 1 5.75 3.5h3.5l2.5 3h6.5a2.25 2.25 0 0 1 2.25 2.25v9.5a2.25 2.25 0 0 1-2.25 2.25H5.75a2.25 2.25 0 0 1-2.25-2.25Z' }], ['path', { d: 'M12 10v6.5M9.25 13.75 12 16.5l2.75-2.75' }]],
  'folder-add': [['path', { d: 'M3.5 7.5V5.75A2.25 2.25 0 0 1 5.75 3.5h3.5l2.5 3h6.5a2.25 2.25 0 0 1 2.25 2.25v9.5a2.25 2.25 0 0 1-2.25 2.25H5.75a2.25 2.25 0 0 1-2.25-2.25Z' }], ['path', { d: 'M12 10v7M8.5 13.5h7' }]],
  'move-to-folder': [['path', { d: 'M7 8V5.5A2 2 0 0 1 9 3.5h3l2.5 3h4A2 2 0 0 1 20.5 8.5v10a2 2 0 0 1-2 2H9a2 2 0 0 1-2-2V18M2.5 13h10M9 9.5l3.5 3.5L9 16.5' }]],
  archive: [['rect', { x: '3', y: '3.5', width: '18', height: '4.5', rx: '1.25' }], ['path', { d: 'M4.5 8v11a1.5 1.5 0 0 0 1.5 1.5h12a1.5 1.5 0 0 0 1.5-1.5V8M9.5 12h5' }]],
  extract: [['path', { d: 'M7 5H3.5v4H7M17 5h3.5v4H17M4.5 9v10a1.5 1.5 0 0 0 1.5 1.5h12a1.5 1.5 0 0 0 1.5-1.5V9M12 14V3M8.5 6.5 12 3l3.5 3.5' }]],
  trash: [['path', { d: 'M3.5 6.5h17M9 6.5V4.25A1.25 1.25 0 0 1 10.25 3h3.5A1.25 1.25 0 0 1 15 4.25V6.5M5.5 6.5l.75 12.25A2 2 0 0 0 8.25 20.5h7.5a2 2 0 0 0 2-1.75L18.5 6.5M9.5 10.5v6M14.5 10.5v6' }]],
  // Files
  file: [['path', { d: 'M13.5 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5ZM13 3v4.5A1.5 1.5 0 0 0 14.5 9H19' }]],
  'file-text': [['path', { d: 'M13.5 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5ZM13 3v4.5A1.5 1.5 0 0 0 14.5 9H19' }], ['path', { d: 'M8.5 12.5h7M8.5 16.5h5' }]],
  'file-image': [['path', { d: 'M13.5 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5ZM13 3v4.5A1.5 1.5 0 0 0 14.5 9H19' }], ['path', { d: 'm8 17 3-4 2.2 2.5 1.3-1.5 2 3Z' }], ['circle', { cx: '9', cy: '10.5', r: '0.9', fill: 'currentColor', stroke: 'none' }]],
  'file-video': [['path', { d: 'M13.5 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5ZM13 3v4.5A1.5 1.5 0 0 0 14.5 9H19' }], ['path', { d: 'm10 11.5 5.5 3.25L10 18Z', fill: 'currentColor', stroke: 'none' }]],
  'file-audio': [['path', { d: 'M13.5 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5ZM13 3v4.5A1.5 1.5 0 0 0 14.5 9H19' }], ['path', { d: 'M12.5 16.5v-5l3-1v3' }], ['ellipse', { cx: '10.75', cy: '17', rx: '1.75', ry: '1.25', fill: 'currentColor', stroke: 'none' }]],
  'file-code': [['path', { d: 'M13.5 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5ZM13 3v4.5A1.5 1.5 0 0 0 14.5 9H19' }], ['path', { d: 'm10 12-2.5 2.5L10 17M14 12l2.5 2.5L14 17' }]],
  'file-archive': [['path', { d: 'M13.5 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5ZM13 3v4.5A1.5 1.5 0 0 0 14.5 9H19' }], ['path', { d: 'M11 10h2M11 13h2' }], ['rect', { x: '10.5', y: '16', width: '3', height: '2.5', rx: '0.6' }]],
  'file-check': [['path', { d: 'M13.5 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5ZM13 3v4.5A1.5 1.5 0 0 0 14.5 9H19' }], ['path', { d: 'm8.5 14.5 2.5 2.5 4.5-5' }]],
  // Controls
  add: [['path', { d: 'M12 4v16M4 12h16' }]],
  minus: [['path', { d: 'M4 12h16' }]],
  check: [['path', { d: 'm4.5 12 5 5L20 6.5' }]],
  close: [['path', { d: 'm6 6 12 12M18 6 6 18' }]],
  more: [['circle', { cx: '5', cy: '12', r: '1.7', fill: 'currentColor', stroke: 'none' }], ['circle', { cx: '12', cy: '12', r: '1.7', fill: 'currentColor', stroke: 'none' }], ['circle', { cx: '19', cy: '12', r: '1.7', fill: 'currentColor', stroke: 'none' }]],
  filter: [['path', { d: 'M4 6h16M4 12h16M4 18h16' }], ['rect', { x: '7', y: '4', width: '3', height: '4', rx: '1', fill: 'currentColor', stroke: 'none' }], ['rect', { x: '14', y: '10', width: '3', height: '4', rx: '1', fill: 'currentColor', stroke: 'none' }], ['rect', { x: '8.5', y: '16', width: '3', height: '4', rx: '1', fill: 'currentColor', stroke: 'none' }]],
  sort: [['path', { d: 'M7 4v16M3.5 7.5 7 4l3.5 3.5M17 4v16M13.5 16.5 17 20l3.5-3.5' }]],
  refresh: [['path', { d: 'M19.75 9a8.1 8.1 0 0 0-13.2-3.4M20 4.5v5h-5M4.25 15a8.1 8.1 0 0 0 13.2 3.4M4 19.5v-5h5' }]],
  // Navigation
  'arrow-left': [['path', { d: 'M20 12H4M10 6l-6 6 6 6' }]],
  'arrow-right': [['path', { d: 'M4 12h16M14 6l6 6-6 6' }]],
  'arrow-up': [['path', { d: 'M12 20V4M6 10l6-6 6 6' }]],
  'arrow-down': [['path', { d: 'M12 4v16M6 14l6 6 6-6' }]],
  'chevron-left': [['path', { d: 'm14.5 6-6 6 6 6' }]],
  'chevron-right': [['path', { d: 'm9.5 6 6 6-6 6' }]],
  'chevron-up': [['path', { d: 'm6 14.5 6-6 6 6' }]],
  'chevron-down': [['path', { d: 'm6 9.5 6 6 6-6' }]],
  // Settings
  settings: [['path', { d: 'M9.671,3.307 14.329,3.307 14.246,5.476 16.527,6.793 18.364,5.636 20.693,9.671 18.773,10.683 18.773,13.317 20.693,14.329 18.364,18.364 16.527,17.207 14.246,18.524 14.329,20.693 9.671,20.693 9.754,18.524 7.473,17.207 5.636,18.364 3.307,14.329 5.227,13.317 5.227,10.683 3.307,9.671 5.636,5.636 7.473,6.793 9.754,5.476Z' }], ['circle', { cx: '12', cy: '12', r: '3' }]],
  display: [['rect', { x: '3', y: '3.5', width: '18', height: '13', rx: '2' }], ['path', { d: 'M8 21h8M12 16.5V21M7 8h10M7 12h6' }]],
  account: [['circle', { cx: '12', cy: '7', r: '3.5' }], ['path', { d: 'M4.5 20v-1.5a6 6 0 0 1 6-6h3a6 6 0 0 1 6 6V20' }]],
  plug: [['path', { d: 'M8.5 3v5M15.5 3v5M5.5 8h13M7 8v4a5 5 0 0 0 10 0V8M12 17v4' }]],
  services: [['circle', { cx: '12', cy: '12', r: '3' }], ['circle', { cx: '4.5', cy: '5', r: '2' }], ['circle', { cx: '19.5', cy: '5', r: '2' }], ['circle', { cx: '12', cy: '20', r: '2' }], ['path', { d: 'm6 6.5 3.7 3.4M18 6.5l-3.7 3.4M12 15v3' }]],
  info: [['circle', { cx: '12', cy: '12', r: '8.5' }], ['path', { d: 'M12 11v6' }], ['circle', { cx: '12', cy: '7.5', r: '1', fill: 'currentColor', stroke: 'none' }]],
  speed: [['path', { d: 'M4.1 18a9 9 0 1 1 15.8 0M5.5 18h13M12 4v2M5.6 7.1l1.5 1.5M18.4 7.1l-1.5 1.5M12 13l3.7-4.5' }], ['circle', { cx: '12', cy: '13.5', r: '1.5', fill: 'currentColor', stroke: 'none' }]],
  history: [['path', { d: 'M4.5 8A8.5 8.5 0 1 1 4 15M3.5 3.5v5h5M12 7v5l3.5 2' }]],
  // Feedback & security
  success: [['circle', { cx: '12', cy: '12', r: '8.5' }], ['path', { d: 'm7.5 12 3 3 6-6' }]],
  warning: [['path', { d: 'M10.4 4.5a1.85 1.85 0 0 1 3.2 0l7 12.2a1.85 1.85 0 0 1-1.6 2.8H5a1.85 1.85 0 0 1-1.6-2.8ZM12 9v4.5' }], ['circle', { cx: '12', cy: '16.5', r: '0.9', fill: 'currentColor', stroke: 'none' }]],
  error: [['path', { d: 'M8.5 3.5h7l5 5v7l-5 5h-7l-5-5v-7ZM12 7.5v5.5' }], ['circle', { cx: '12', cy: '16.5', r: '1', fill: 'currentColor', stroke: 'none' }]],
  help: [['circle', { cx: '12', cy: '12', r: '8.5' }], ['path', { d: 'M9.5 9a2.5 2.5 0 0 1 5 0c0 1.8-2.5 2-2.5 4' }], ['circle', { cx: '12', cy: '16.5', r: '1', fill: 'currentColor', stroke: 'none' }]],
  lock: [['path', { d: 'M7.5 10V7a4.5 4.5 0 0 1 9 0v3' }], ['rect', { x: '4.5', y: '10', width: '15', height: '11', rx: '2' }], ['path', { d: 'M12 14v3' }]],
  unlock: [['path', { d: 'M7.5 10V7a4.5 4.5 0 0 1 8.5-2' }], ['rect', { x: '4.5', y: '10', width: '15', height: '11', rx: '2' }], ['path', { d: 'M12 14v3' }]],
  eye: [['path', { d: 'M2.5 12s3.25-6 9.5-6 9.5 6 9.5 6-3.25 6-9.5 6S2.5 12 2.5 12Z' }], ['circle', { cx: '12', cy: '12', r: '2.75' }]],
  'eye-off': [['path', { d: 'M8 6.8A10.8 10.8 0 0 1 12 6c6.25 0 9.5 6 9.5 6a15 15 0 0 1-3 3.55M15.8 17.3A10.8 10.8 0 0 1 12 18c-6.25 0-9.5-6-9.5-6a15 15 0 0 1 3-3.55M3.5 3.5l17 17' }]],
  // Layout & themes
  list: [['path', { d: 'M8.5 5h12M8.5 12h12M8.5 19h12' }], ['circle', { cx: '3.5', cy: '5', r: '1.1', fill: 'currentColor', stroke: 'none' }], ['circle', { cx: '3.5', cy: '12', r: '1.1', fill: 'currentColor', stroke: 'none' }], ['circle', { cx: '3.5', cy: '19', r: '1.1', fill: 'currentColor', stroke: 'none' }]],
  grid: [['rect', { x: '4', y: '4', width: '6', height: '6', rx: '1.5' }], ['rect', { x: '14', y: '4', width: '6', height: '6', rx: '1.5' }], ['rect', { x: '4', y: '14', width: '6', height: '6', rx: '1.5' }], ['rect', { x: '14', y: '14', width: '6', height: '6', rx: '1.5' }]],
  'select-all': [['path', { d: 'M3.5 7V4.5a1 1 0 0 1 1-1H7M17 3.5h2.5a1 1 0 0 1 1 1V7M20.5 17v2.5a1 1 0 0 1-1 1H17M7 20.5H4.5a1 1 0 0 1-1-1V17M3.5 11v2M20.5 11v2M11 3.5h2M11 20.5h2M7.5 12l3 3 6-6' }]],
  'clear-selection': [['path', { d: 'M3.5 7V4.5a1 1 0 0 1 1-1H7M17 3.5h2.5a1 1 0 0 1 1 1V7M20.5 17v2.5a1 1 0 0 1-1 1H17M7 20.5H4.5a1 1 0 0 1-1-1V17M3.5 11v2M20.5 11v2M11 3.5h2M11 20.5h2M9 9l6 6M15 9l-6 6' }]],
  sun: [['circle', { cx: '12', cy: '12', r: '4' }], ['path', { d: 'M12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.25 5.25l1.5 1.5M17.25 17.25l1.5 1.5M18.75 5.25l-1.5 1.5M6.75 17.25l-1.5 1.5' }]],
  moon: [['path', { d: 'M20.5 14.25A8.75 8.75 0 0 1 9.75 3.5 8.75 8.75 0 1 0 20.5 14.25Z' }]],
  'system-theme': [['rect', { x: '3', y: '3.5', width: '18', height: '13', rx: '2' }], ['path', { d: 'M8 21h8M12 16.5V21' }], ['path', { d: 'M12 3.5h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-7Z', fill: 'currentColor', stroke: 'none' }]],
  network: [['path', { d: 'M4 8a13 13 0 0 1 16 0M7.5 12a7.5 7.5 0 0 1 9 0M10.25 15.75a3 3 0 0 1 3.5 0' }], ['circle', { cx: '12', cy: '20', r: '1.15', fill: 'currentColor', stroke: 'none' }]],
});

/** @typedef {keyof typeof ICONS} IconName */

const SVG = 'http://www.w3.org/2000/svg';

/**
 * Builds `shape` with the DOM API: the UI's Trusted Types policy rules out
 * parsing markup.
 * @param {Shape} shape
 * @returns {SVGElement}
 */
function draw([tag, attributes, ...children]) {
  const element = /** @type {SVGElement} */ (document.createElementNS(SVG, tag));
  for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, value);
  element.append(...children.map(draw));
  return element;
}

/**
 * The icon named `name`.
 * @param {IconName} name
 */
export function icon(name) {
  const svg = document.createElementNS(SVG, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  svg.setAttribute('class', `icon icon-${name}`);
  svg.append(.../** @type {readonly Shape[]} */ (ICONS[name]).map(draw));
  return svg;
}
