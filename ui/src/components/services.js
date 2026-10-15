// Download services Wayfetch may work with: the catalogue for the Accounts
// & services section of Settings, and its search. The services themselves
// are data, in `ui/src/data/services.json`: adding one is an edit there,
// checked when it is read, with no change to the code.
//
// The catalogue only describes services. It never enables a download
// route: whether Wayfetch can sign in to a service is the engine's to say,
// from the integrations it reports, and the section shows every other
// entry as not available. Signing in and downloading are separate: an
// integration may sign accounts in before downloads can go through them. The domains listed are for finding a service by a
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
 *   planned: boolean,
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

/** Where the catalogue's data is, beside the UI's code. */
const CATALOGUE = new URL('../data/services.json', import.meta.url);

/**
 * The services in `data`, the content of the catalogue's file, in its
 * order. Rejects data that is not a list of services with unique IDs,
 * known categories, and domains no other service claims, so a mistake in
 * the file shows at once instead of as a wrong entry.
 * @param {unknown} data
 * @returns {Service[]}
 */
export function parseCatalogue(data) {
  const services = /** @type {{ services?: unknown }} */ (data)?.services;
  if (!Array.isArray(services)) throw new Error('the service catalogue has no list of services');
  const categories = new Set(CATEGORIES.map((c) => c.id));
  const ids = new Set();
  const domains = new Set();
  return services.map((entry, at) => {
    const { id, name, category, about, domains: names, planned = false } = entry ?? {};
    const text = (/** @type {unknown} */ value) => typeof value === 'string' && value.trim() !== '';
    const valid =
      typeof id === 'string' &&
      /^[a-z0-9]+(-[a-z0-9]+)*$/.test(id) &&
      !ids.has(id) &&
      text(name) &&
      text(about) &&
      categories.has(category) &&
      typeof planned === 'boolean' &&
      Array.isArray(names) &&
      names.length > 0 &&
      names.every((domain) => typeof domain === 'string' && /^[a-z0-9.-]+$/.test(domain) && !domains.has(domain));
    if (!valid) throw new Error(`service ${at + 1} in the catalogue is not valid`);
    ids.add(id);
    for (const domain of names) domains.add(domain);
    return { id, name, category, about, domains: names, planned };
  });
}

/** @type {Promise<Service[]> | null} */
let loading = null;

/**
 * The catalogue's services, read once from its file, which is bundled
 * with the UI. It is imported as a JSON module, which both hosts' content
 * policies already allow for the UI's own files.
 * @returns {Promise<Service[]>}
 */
export function loadServices() {
  loading ??= import(CATALOGUE.href, { with: { type: 'json' } }).then((module) => parseCatalogue(module.default));
  return loading;
}

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
 * @param {Category | null} category
 * @param {readonly Service[]} services
 * @returns {Match[]}
 */
export function searchServices(query, category, services) {
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
 * Whether Wayfetch can sign in to `service`, given the services the engine
 * reports integrations for.
 * @param {Service} service
 * @param {ReadonlySet<string>} integrations
 */
export function isAvailable(service, integrations) {
  return integrations.has(service.id);
}

/**
 * What the catalogue can do: the integrations the engine reports, by
 * service ID; whether accounts can be added now, and if not why; and what
 * to do with a service picked.
 * @typedef {{
 *   integrations: ReadonlyMap<string, ServiceSupport>,
 *   blocked: string | null,
 *   onPick: (service: Service, support: ServiceSupport) => void,
 * }} Picking
 * @typedef {import('../client/contract.js').ServiceSupport} ServiceSupport
 */

/**
 * The availability tags of `service`.
 * @param {Service} service
 * @param {ReadonlySet<string>} integrations
 * @param {ServiceSupport | undefined} support
 */
function availability(service, integrations, support) {
  if (isAvailable(service, integrations)) {
    return [
      h('span', { class: 'tag available' }, 'Available'),
      support?.downloads === false && h('span', { class: 'tag' }, 'Sign-in only'),
    ];
  }
  return [
    service.planned && h('span', { class: 'tag current' }, 'Planned'),
    h('span', { class: 'tag' }, 'Not available yet'),
  ];
}

/**
 * Opens the catalogue to pick the service of an account to add: a search
 * field, category filters with how many services match each, and the
 * matching services, grouped by category until something is searched for.
 * Planned integrations lead. Only a service the engine has an integration
 * for can be picked, and only while accounts can be stored.
 * @param {Picking} picking
 */
export function pickService(picking) {
  const integrations = new Set(picking.integrations.keys());
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

  /** The catalogue, once read. */
  /** @type {Service[] | null} */
  let services = null;

  const render = () => {
    if (services === null) return;
    const list = services;
    const query = field.value;
    const picked = choices[radios.findIndex((radio) => radio.checked)] ?? null;
    choices.forEach((id, at) => show(counts[at], searchServices(query, id, list).length));
    const matches = searchServices(query, picked, list);
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
      show(results, h('ul', { class: 'service-list' }, matches.map((match) => serviceItem(match, picking, pick))));
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
          h('ul', { class: 'service-list' }, group.map((match) => serviceItem(match, picking, pick))),
        );
      }),
    );
  };

  /** Reads the catalogue and shows it, or why it could not be read. */
  const load = async () => {
    show(summary, 'Loading…');
    show(results);
    try {
      services = await loadServices();
      render();
    } catch (error) {
      show(summary, '');
      show(
        results,
        h('p', { class: 'message error', role: 'alert' }, `The list of services could not be loaded: ${String(error)}`),
      );
    }
  };

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
      h(
        'p',
        { class: 'hint' },
        integrations.size === 0
          ? 'No account can be added yet: Wayfetch does not support accounts with these services so far.'
          : (picking.blocked ?? 'Accounts can be added for the services marked Available.'),
      ),
    ),
  );

  /**
   * Closes the catalogue and hands on `service`.
   * @param {Service} service
   * @param {ServiceSupport} support
   */
  const pick = (service, support) => {
    dialog.close();
    picking.onPick(service, support);
  };
  field.focus();
  void load();
}

/**
 * One service in the catalogue; one with an integration can be picked
 * while accounts can be added.
 * @param {Match} match
 * @param {Picking} picking
 * @param {(service: Service, support: ServiceSupport) => void} pick
 */
function serviceItem({ service, alias }, picking, pick) {
  const integrations = new Set(picking.integrations.keys());
  const support = picking.integrations.get(service.id);
  return h(
    'li',
    { class: service.planned ? 'service-card featured' : 'service-card', 'data-service': service.id },
    h(
      'div',
      { class: 'service-title' },
      h('h4', null, service.name),
      h('span', { class: 'tag' }, categoryLabel(service.category)),
      ...availability(service, integrations, support),
      support &&
        picking.blocked === null &&
        h(
          'button',
          {
            type: 'button',
            class: 'pick-service',
            'data-key': `pick-${service.id}`,
            'aria-label': `Sign in to ${service.name}`,
            onclick: () => pick(service, support),
          },
          'Sign in…',
        ),
    ),
    h('p', null, service.about),
    support
      ? h(
          'p',
          { class: 'hint' },
          support.downloads
            ? 'Sign in on the service’s own site; Wayfetch never asks for your password.'
            : 'Sign in on the service’s own site; Wayfetch never asks for your password. Wayfetch can check the account, but cannot download through it yet.',
        )
      : service.planned && h('p', { class: 'hint' }, 'The first account integration planned for Wayfetch.'),
    h(
      'p',
      { class: 'domains' },
      alias !== null && h('span', { class: 'alias' }, `Matches ${alias}`),
      h('span', null, service.domains.join(', ')),
    ),
  );
}
