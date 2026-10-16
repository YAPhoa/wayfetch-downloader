import assert from 'node:assert/strict';
import { test } from 'node:test';

import { settingsRequest } from '../../src/client/contract.js';
import { SECTIONS, describeSetting, parseCount, sectionPath, settingsSection } from '../../src/components/settings.js';

test('Settings paths name their section; others are not Settings', () => {
  assert.equal(settingsSection('/settings'), 'downloads');
  for (const { id } of SECTIONS) assert.equal(settingsSection(sectionPath(id)), id);
  assert.equal(settingsSection('/settings/nonsense'), 'downloads');
  assert.equal(settingsSection('/'), null);
  assert.equal(settingsSection('/jobs/job-1'), null);
  assert.equal(settingsSection('/settingsx'), null);
});

test('sections come in the order of the settings they hold', () => {
  assert.deepEqual(
    SECTIONS.map(({ label }) => label),
    ['Downloads', 'Download folders', 'Accounts & services', 'Display'],
  );
});

/** @type {import('../../src/client/contract.js').BoundedSetting} */
const LIMIT = { value: 4, source: 'built_in', default: 4, min: 1, max: 64, locked: false, conflict: false };

test('a setting says what it allows and where its value comes from', () => {
  assert.equal(describeSetting(LIMIT, false), 'Allowed: 1–64. Using the default.');
  assert.equal(
    describeSetting({ ...LIMIT, source: 'server', default: 6, value: 6, max: 8 }, false),
    'Allowed: 1–8. Using the server’s default.',
  );
  assert.equal(
    describeSetting({ ...LIMIT, source: 'server' }, true),
    'Allowed: 1–64. Using the configured default.',
  );
  assert.equal(
    describeSetting({ ...LIMIT, source: 'saved', saved: 10, value: 10 }, false),
    'Allowed: 1–64. Saved; the default is 4.',
  );
  assert.equal(
    describeSetting({ ...LIMIT, source: 'saved', saved: 12, value: 8, max: 8, conflict: true }, false),
    'Allowed: 1–8. Your saved value 12 is outside it, so 8 is used.',
  );
  assert.equal(
    describeSetting({ ...LIMIT, source: 'server', locked: true, value: 2, default: 2 }, false),
    'Managed by server configuration.',
  );
  assert.equal(
    describeSetting({ ...LIMIT, source: 'server', locked: true, value: 2, saved: 6, conflict: true }, false),
    'Managed by server configuration. Your saved value 6 is not used.',
  );
});

test('only whole numbers within the allowed range can be saved', () => {
  const bounded = { ...LIMIT, min: 2, max: 8 };
  assert.deepEqual(parseCount(' 8 ', bounded), { value: 8, error: null });
  for (const text of ['', '1', '9', '2.5', '-3', '1e1', 'four', '99999999999999999999']) {
    assert.deepEqual(parseCount(text, bounded), { value: null, error: 'Enter a whole number from 2 to 8.' }, text);
  }
});

test('a settings change sends only what changes', () => {
  const base = { idempotencyKey: 'k', target: 't', expectedRevision: 3 };
  assert.deepEqual(settingsRequest({ ...base, defaultRoot: 'root-2' }), {
    idempotency_key: 'k',
    target: 't',
    expected_revision: 3,
    default_root: 'root-2',
  });
  assert.deepEqual(settingsRequest({ ...base, maxActiveDownloads: { set: 6 } }), {
    idempotency_key: 'k',
    target: 't',
    expected_revision: 3,
    max_active_downloads: { set: 6 },
  });
  assert.deepEqual(settingsRequest({ ...base, maxActiveDownloads: 'use_default' }).max_active_downloads, 'use_default');
});
