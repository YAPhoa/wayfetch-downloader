import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';

import { CATEGORIES, isAvailable, parseCatalogue, searchServices } from '../../src/components/services.js';

/** The catalogue as shipped. */
const DATA = JSON.parse(await readFile(new URL('../../src/data/services.json', import.meta.url), 'utf8'));
const SERVICES = parseCatalogue(DATA);
const FEATURED = /** @type {import('../../src/components/services.js').Service} */ (
  SERVICES.find((s) => s.id === 'real-debrid')
);

/**
 * Searches the shipped catalogue.
 * @param {string} query
 * @param {import('../../src/components/services.js').Category | null} [category]
 */
const search = (query, category = null) => searchServices(query, category, SERVICES);

/** @param {import('../../src/components/services.js').Match[]} matches */
const ids = (matches) => matches.map((match) => match.service.id);

test('every service has a stable unique ID, a known category, and domains no other service claims', () => {
  const categories = new Set(CATEGORIES.map((c) => c.id));
  const seen = new Map();
  assert.equal(new Set(SERVICES.map((s) => s.id)).size, SERVICES.length);
  for (const service of SERVICES) {
    assert.match(service.id, /^[a-z0-9]+(-[a-z0-9]+)*$/);
    assert.ok(categories.has(service.category), service.id);
    assert.ok(service.about.length > 0 && service.domains.length > 0, service.id);
    for (const domain of service.domains) {
      assert.equal(domain, domain.toLowerCase());
      assert.ok(!seen.has(domain), `${domain} is listed by ${seen.get(domain)} and ${service.id}`);
      seen.set(domain, service.id);
    }
  }
  for (const { id } of CATEGORIES) assert.ok(SERVICES.some((s) => s.category === id), id);
});

test('Real-Debrid is the one planned integration, and a multi-host service', () => {
  assert.equal(FEATURED.id, 'real-debrid');
  assert.equal(FEATURED.category, 'multi_host');
  assert.deepEqual(SERVICES.filter((s) => s.planned).map((s) => s.id), ['real-debrid']);
});

test('an empty search lists every service once, in catalogue order', () => {
  assert.deepEqual(ids(search('')), SERVICES.map((s) => s.id));
  assert.deepEqual(ids(search('   ')), SERVICES.map((s) => s.id));
});

test('names match regardless of case, spaces and punctuation', () => {
  for (const query of ['Real-Debrid', 'real debrid', 'REALDEBRID', 'realdeb']) {
    assert.deepEqual(ids(search(query)), ['real-debrid'], query);
  }
  assert.deepEqual(ids(search('debrid')), ['real-debrid', 'alldebrid', 'debrid-link']);
  assert.deepEqual(ids(search('google drive')), ['google-drive']);
});

test('aliases find their one service and say which domain matched', () => {
  assert.deepEqual(search('rg.to'), [{ service: SERVICES.find((s) => s.id === 'rapidgator'), alias: 'rg.to' }]);
  assert.deepEqual(ids(search('ddl.to')), ['ddownload']);
  assert.deepEqual(ids(search('pixeldra.in')), ['pixeldrain']);
  assert.deepEqual(ids(search('debrid-link.fr')), ['debrid-link']);
  // A service with many domains is still one entry.
  const fichier = search('.fr');
  assert.deepEqual(ids(fichier), ['debrid-link', '1fichier']);
  assert.equal(fichier[1].alias, 'megadl.fr');
});

test('a pasted link or a subdomain finds the service of its host', () => {
  const cases = {
    'https://rg.to/file/abc123/name.rar.html': 'rapidgator',
    'https://www.1fichier.com/?abcdef': '1fichier',
    'HTTP://Alterupload.com/x': '1fichier',
    'https://drive.usercontent.google.com/download?id=1': 'google-drive',
    'https://dl.dropboxusercontent.com/s/abc/file.zip': 'dropbox',
    'mega.nz/file/abc#key': 'mega',
    'www.nitro.download': 'nitroflare',
    'gofile.io.': 'gofile',
  };
  for (const [query, id] of Object.entries(cases)) assert.deepEqual(ids(search(query)), [id], query);
  assert.equal(search('https://dl.dropboxusercontent.com/s/abc')[0].alias, 'dropboxusercontent.com');
});

test('name matches are listed before services found only by a domain', () => {
  // "mega" names MEGA and is part of 1fichier's megadl.fr.
  const matches = search('mega');
  assert.deepEqual(ids(matches), ['mega', '1fichier']);
  assert.deepEqual(matches.map((m) => m.alias), [null, 'megadl.fr']);
});

test('a category limits the search', () => {
  assert.deepEqual(
    ids(search('', 'multi_host')),
    ['real-debrid', 'alldebrid', 'torbox', 'debrid-link', 'linksnappy', 'premium-to'],
  );
  assert.equal(search('', 'file_hosting').length, 8);
  assert.equal(search('', 'cloud_storage').length, 6);
  assert.deepEqual(ids(search('mega', 'cloud_storage')), ['mega']);
  assert.deepEqual(ids(search('rapidgator', 'cloud_storage')), []);
});

test('nothing matches unknown names or hosts', () => {
  assert.deepEqual(search('no such service'), []);
  assert.deepEqual(search('https://example.com/file.zip'), []);
  assert.deepEqual(search('😀'), []);
});

test('a catalogue entry is never available by itself', () => {
  const none = new Set();
  for (const service of SERVICES) assert.equal(isAvailable(service, none), false, service.id);
  assert.equal(isAvailable(FEATURED, new Set(['real-debrid'])), true);
});

test('the catalogue file is checked: malformed or conflicting entries are refused', () => {
  const good = { id: 'example', name: 'Example', category: 'file_hosting', about: 'A host.', domains: ['example.com'] };
  assert.deepEqual(parseCatalogue({ services: [good] }), [{ ...good, planned: false }]);
  for (const services of [
    [{ ...good, id: 'Bad ID' }],
    [{ ...good, category: 'video' }],
    [{ ...good, name: ' ' }],
    [{ ...good, domains: [] }],
    [{ ...good, domains: ['Example.com'] }],
    [{ ...good, planned: 'yes' }],
    [good, { ...good, domains: ['other.com'] }],
    [good, { ...good, id: 'other' }],
    [null],
  ]) {
    assert.throws(() => parseCatalogue({ services }), /service \d+ in the catalogue is not valid/);
  }
  assert.throws(() => parseCatalogue([good]), /no list of services/);
});

test('a service added to the data is listed and found without code changes', () => {
  const added = { id: 'new-host', name: 'New Host', category: 'file_hosting', about: 'A host.', domains: ['nh.example'] };
  const services = parseCatalogue({ services: [...DATA.services, added] });
  assert.equal(services.length, SERVICES.length + 1);
  assert.deepEqual(searchServices('https://nh.example/f/1', null, services).map((m) => m.service.id), ['new-host']);
  assert.equal(searchServices('', 'file_hosting', services).at(-1)?.service.id, 'new-host');
});
