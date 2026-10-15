// Download services Wayfetch may work with: a short, curated catalogue for
// the Accounts & services section of Settings, and its search.
//
// The catalogue only describes services. It never enables a download
// route: whether Wayfetch can use a service is the engine's to say, once an
// integration for it exists, and the section shows each entry as not
// available until then. The domains listed are for finding a service by a
// link or its address; they are not the engine's link matching, and they
// say nothing about which hosts a multi-host service covers, which comes
// from the service itself once integrated.
//
// Each service has a stable ID that a later integration keeps, and one
// entry however many domains it answers on.

import { openDialog } from './dialog.js';
import { h, show } from './dom.js';
import { icon } from './icons.js';

/**
 * @typedef {'multi_host' | 'file_hosting' | 'cloud_storage'} Category
 * @typedef {{
 *   id: string,
 *   name: string,
 *   category: Category,
 *   about: string,
 *   domains: readonly string[],
 *   planned?: boolean,
 * }} Service
 * @typedef {{ service: Service, alias: string | null }} Match
 */

/**
 * The categories, in display order.
 * @type {readonly { id: Category, label: string }[]}
 */
export const CATEGORIES = [
  { id: 'multi_host', label: 'Multi-host services' },
  { id: 'file_hosting', label: 'File hosting' },
  { id: 'cloud_storage', label: 'Cloud storage' },
];

/**
 * The catalogue, by category, then roughly by how widely each is used.
 * @type {readonly Service[]}
 */
export const SERVICES = [
  {
    id: 'real-debrid',
    name: 'Real-Debrid',
    category: 'multi_host',
    about: 'One premium account that downloads links from many file hosts.',
    domains: ['real-debrid.com'],
    planned: true,
  },
  {
    id: 'alldebrid',
    name: 'AllDebrid',
    category: 'multi_host',
    about: 'Premium account that downloads from many file hosts and streaming sites.',
    domains: ['alldebrid.com'],
  },
  {
    id: 'torbox',
    name: 'TorBox',
    category: 'multi_host',
    about: 'Premium account for file-host links, torrents and Usenet.',
    domains: ['torbox.app'],
  },
  {
    id: 'debrid-link',
    name: 'Debrid-Link',
    category: 'multi_host',
    about: 'Premium account that downloads from many file hosts.',
    domains: ['debrid-link.com', 'debrid-link.fr'],
  },
  {
    id: 'linksnappy',
    name: 'LinkSnappy',
    category: 'multi_host',
    about: 'Premium link generator for many file hosts.',
    domains: ['linksnappy.com'],
  },
  {
    id: 'premium-to',
    name: 'premium.to',
    category: 'multi_host',
    about: 'Premium account for file hosts and Usenet.',
    domains: ['premium.to'],
  },
  {
    id: '1fichier',
    name: '1fichier',
    category: 'file_hosting',
    about: 'French file host with free and premium downloads.',
    domains: [
      '1fichier.com',
      'alterupload.com',
      'cjoint.net',
      'desfichiers.com',
      'dfichiers.com',
      'megadl.fr',
      'mesfichiers.org',
      'piecejointe.net',
      'pjointe.com',
      'tenvoi.com',
      'dl4free.com',
    ],
  },
  {
    id: 'rapidgator',
    name: 'Rapidgator',
    category: 'file_hosting',
    about: 'File host with free and premium downloads.',
    domains: ['rapidgator.net', 'rg.to'],
  },
  {
    id: 'ddownload',
    name: 'Ddownload',
    category: 'file_hosting',
    about: 'File host with free and premium downloads.',
    domains: ['ddownload.com', 'ddl.to'],
  },
  {
    id: 'katfile',
    name: 'Katfile',
    category: 'file_hosting',
    about: 'File host with free and premium downloads.',
    domains: ['katfile.com'],
  },
  {
    id: 'nitroflare',
    name: 'NitroFlare',
    category: 'file_hosting',
    about: 'File host with free and premium downloads.',
    domains: ['nitroflare.com', 'nitro.download'],
  },
  {
    id: 'fikper',
    name: 'Fikper',
    category: 'file_hosting',
    about: 'File host with free and premium downloads.',
    domains: ['fikper.com'],
  },
  {
    id: 'filefactory',
    name: 'FileFactory',
    category: 'file_hosting',
    about: 'Long-running file host with free and premium downloads.',
    domains: ['filefactory.com'],
  },
  {
    id: 'uploadgig',
    name: 'Uploadgig',
    category: 'file_hosting',
    about: 'File host with free and premium downloads.',
    domains: ['uploadgig.com'],
  },
  {
    id: 'mega',
    name: 'MEGA',
    category: 'cloud_storage',
    about: 'End-to-end encrypted cloud storage with shared file and folder links.',
    domains: ['mega.nz', 'mega.io', 'mega.co.nz'],
  },
  {
    id: 'google-drive',
    name: 'Google Drive',
    category: 'cloud_storage',
    about: 'Google’s cloud storage, with shared file and folder links.',
    domains: ['drive.google.com', 'docs.google.com', 'drive.usercontent.google.com'],
  },
  {
    id: 'dropbox',
    name: 'Dropbox',
    category: 'cloud_storage',
    about: 'Cloud storage with shared file and folder links.',
    domains: ['dropbox.com', 'dropboxusercontent.com', 'db.tt'],
  },
  {
    id: 'mediafire',
    name: 'MediaFire',
    category: 'cloud_storage',
    about: 'Cloud storage and file sharing with free and paid plans.',
    domains: ['mediafire.com'],
  },
  {
    id: 'pixeldrain',
    name: 'Pixeldrain',
    category: 'cloud_storage',
    about: 'File storage and sharing with free and paid plans.',
    domains: ['pixeldrain.com', 'pixeldra.in'],
  },
  {
    id: 'gofile',
    name: 'Gofile',
    category: 'cloud_storage',
    about: 'File storage and sharing with free and premium accounts.',
    domains: ['gofile.io'],
  },
];

/** The service Wayfetch plans to integrate first, featured in the catalogue. */
export const FEATURED = /** @type {Service} */ (SERVICES.find((service) => service.planned));

/**
 * The label of `category`.
 * @param {Category} category
 */
export function categoryLabel(category) {
  return /** @type {{ label: string }} */ (CATEGORIES.find((c) => c.id === category)).label;
}

/**
 * Letters and digits only, lower-cased, so "Real Debrid", "real-debrid"
 * and "RealDebrid" compare equal.
 * @param {string} text
 */
function letters(text) {
  return text.toLowerCase().replace(/[^\p{L}\p{N}]/gu, '');
}

/**
 * The host a search names, if it is a link or an address: lower-cased,
 * without `www.`, a port, a path, or a trailing dot.
 * @param {string} query
 * @returns {string}
 */
function searchedHost(query) {
  let text = query.trim().toLowerCase();
  if (/^[a-z][a-z0-9+.-]*:\/\//.test(text)) {
    try {
      text = new URL(text).hostname;
    } catch {
      // Not a link after all; search for it as typed.
    }
  }
  return text
    .replace(/[/?#:].*$/, '')
    .replace(/\.$/, '')
    .replace(/^www\./, '');
}

/**
 * The domain of `service` that `host` matches: `host` is that domain or
 * one of its subdomains, or, while it is being typed, part of it. `null`
 * if none does.
 * @param {Service} service
 * @param {string} host
 */
function matchedDomain(service, host) {
  if (host === '') return null;
  return (
    service.domains.find((domain) => host === domain || host.endsWith(`.${domain}`)) ??
    service.domains.find((domain) => domain.includes(host)) ??
    null
  );
}

/**
 * The services in `category` (any if `null`) that match `query` by name or
 * by one of their domains, each once. Name matches come first; a service
 * found only by a domain carries that domain as `alias`, so the result can
 * say why it is listed. An empty query matches every service.
 * @param {string} query
 * @param {Category | null} [category]
 * @param {readonly Service[]} [services]
 * @returns {Match[]}
 */
export function searchServices(query, category = null, services = SERVICES) {
  const all = query.trim() === '';
  const name = letters(query);
  const host = searchedHost(query);
  /** @type {Match[]} */
  const byName = [];
  /** @type {Match[]} */
  const byDomain = [];
  for (const service of services) {
    if (category !== null && service.category !== category) continue;
    if (all || (name !== '' && letters(service.name).includes(name))) {
      byName.push({ service, alias: null });
      continue;
    }
    const alias = matchedDomain(service, host);
    if (alias !== null) byDomain.push({ service, alias });
  }
  return [...byName, ...byDomain];
}

/**
 * Whether Wayfetch can use `service`, given the integrations the engine
 * reports. None exists yet, so every service is unavailable.
 * @param {Service} service
 * @param {ReadonlySet<string>} integrations
 */
export function isAvailable(service, integrations) {
  return integrations.has(service.id);
}

/**
 * The availability tags of `service`.
 * @param {Service} service
 * @param {ReadonlySet<string>} integrations
 */
function availability(service, integrations) {
  if (isAvailable(service, integrations)) return [h('span', { class: 'tag available' }, 'Available')];
  return [
    service.planned && h('span', { class: 'tag current' }, 'Planned'),
    h('span', { class: 'tag' }, 'Not available yet'),
  ];
}

/**
 * Opens the catalogue to pick the service of an account to add: a search field, category
 * filters with how many services match each, and the matching services,
 * grouped by category until something is searched for. The planned
 * integration leads. A service can be picked only once the engine has an
 * integration for it, and none has yet, so no account can be added.
 * @param {ReadonlySet<string>} integrations
 */
export function pickService(integrations) {
  const field = h('input', {
    type: 'search',
    id: 'service-search',
    placeholder: 'Name or address, such as rg.to',
    autocomplete: 'off',
    spellcheck: 'false',
    'data-key': 'service-search',
    oninput: () => render(),
    // Escape clears a search, then closes the dialog. Done here because a
    // search field's own Escape handling differs between engines: WebKit
    // keeps it from closing the dialog at all.
    onkeydown: (/** @type {KeyboardEvent} */ event) => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      if (field.value === '') {
        dialog.close();
        return;
      }
      field.value = '';
      render();
    },
  });
  /** @type {(Category | null)[]} */
  const choices = [null, ...CATEGORIES.map((c) => c.id)];
  const radios = choices.map((id) =>
    h('input', {
      type: 'radio',
      name: 'service-category',
      class: 'visually-hidden',
      value: id ?? 'all',
      checked: id === null,
      'data-key': `service-category-${id ?? 'all'}`,
      onchange: () => render(),
    }),
  );
  const counts = choices.map(() => h('span', { class: 'count' }));
  const filters = h(
    'fieldset',
    { class: 'chips' },
    h('legend', { class: 'visually-hidden' }, 'Category'),
    choices.map((id, at) =>
      h('label', { class: 'chip' }, radios[at], h('span', null, id === null ? 'All' : categoryLabel(id)), counts[at]),
    ),
  );
  const summary = h('p', { class: 'message', role: 'status' });
  const results = h('div', {
    class: 'service-results',
    role: 'region',
    'aria-label': 'Services',
    tabindex: 0,
  });

  const render = () => {
    const query = field.value;
    const picked = choices[radios.findIndex((radio) => radio.checked)] ?? null;
    choices.forEach((id, at) => show(counts[at], searchServices(query, id).length));
    const matches = searchServices(query, picked);
    if (matches.length === 0) {
      show(summary, '');
      show(
        results,
        h(
          'div',
          { class: 'empty-state' },
          h('p', null, `No services match “${query.trim()}”${picked ? ` in ${categoryLabel(picked)}` : ''}.`),
          h('p', null, 'Links you add directly are still downloaded without an account.'),
        ),
      );
      return;
    }
    show(summary, matches.length === 1 ? '1 service' : `${matches.length} services`);
    // Search results keep their order, best first; the whole catalogue is
    // grouped by category.
    if (query.trim() !== '') {
      show(results, h('ul', { class: 'service-list' }, matches.map((match) => serviceItem(match, integrations))));
      return;
    }
    show(
      results,
      CATEGORIES.map(({ id, label }) => {
        const group = matches.filter((match) => match.service.category === id);
        if (group.length === 0) return null;
        return h(
          'section',
          { class: 'service-group', 'aria-labelledby': `service-group-${id}` },
          h('h3', { id: `service-group-${id}` }, label),
          h('ul', { class: 'service-list' }, group.map((match) => serviceItem(match, integrations))),
        );
      }),
    );
  };

  render();
  const dialog = openDialog(
    { title: 'Add account', className: 'services' },
    h(
      'div',
      { class: 'service-browser' },
      h(
        'div',
        { class: 'search' },
        icon('search'),
        h('label', { for: 'service-search', class: 'visually-hidden' }, 'Search services'),
        field,
      ),
      filters,
      summary,
      results,
      h('p', { class: 'hint' }, 'No account can be added yet: Wayfetch does not support accounts with these services so far.'),
    ),
  );
  field.focus();
}

/**
 * One service in the catalogue.
 * @param {Match} match
 * @param {ReadonlySet<string>} integrations
 */
function serviceItem({ service, alias }, integrations) {
  return h(
    'li',
    { class: service.planned ? 'service-card featured' : 'service-card', 'data-service': service.id },
    h(
      'div',
      { class: 'service-title' },
      h('h4', null, service.name),
      h('span', { class: 'tag' }, categoryLabel(service.category)),
      ...availability(service, integrations),
    ),
    h('p', null, service.about),
    service.planned && h('p', { class: 'hint' }, 'The first account integration planned for Wayfetch.'),
    h(
      'p',
      { class: 'domains' },
      alias !== null && h('span', { class: 'alias' }, `Matches ${alias}`),
      h('span', null, service.domains.join(', ')),
    ),
  );
}
