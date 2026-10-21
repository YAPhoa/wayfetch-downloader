import assert from 'node:assert/strict';
import { test } from 'node:test';

import { settingsRequest } from '../../src/client/contract.js';
import {
  SECTIONS,
  describeSetting,
  describeSpeedLimit,
  formatRate,
  formatSpeedLimit,
  parseCount,
  parseRate,
  sectionPath,
  settingsSection,
  speedFields,
} from '../../src/components/settings.js';

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
    ['Downloads', 'Download folders', 'Accounts & services', 'Plugins', 'Display', 'About'],
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
  assert.deepEqual(
    settingsRequest({
      ...base,
      maxActiveDownloads: { set: 2 },
      downloadSpeedLimit: { set: { bytes_per_second: 2097152 } },
    }),
    {
      idempotency_key: 'k',
      target: 't',
      expected_revision: 3,
      max_active_downloads: { set: 2 },
      download_speed_limit: { set: { bytes_per_second: 2097152 } },
    },
  );
  assert.deepEqual(settingsRequest({ ...base, downloadSpeedLimit: { set: 'unlimited' } }).download_speed_limit, {
    set: 'unlimited',
  });
  assert.equal('download_speed_limit' in settingsRequest(base), false);
});

const MIB = 1024 * 1024;

/** @type {import('../../src/client/contract.js').SpeedLimitSetting} */
const SPEED = {
  value: 'unlimited',
  source: 'built_in',
  default: 'unlimited',
  min: 1024,
  max: 2 ** 40,
  unlimited: true,
  locked: false,
  conflict: false,
};

test('rates read in binary units, and Unlimited is a word, not a number', () => {
  assert.equal(formatRate(1024), '1 KiB/s');
  assert.equal(formatRate(1536), '1.5 KiB/s');
  assert.equal(formatRate(2 * MIB), '2 MiB/s');
  assert.equal(formatRate(2 ** 40), '1 TiB/s');
  assert.equal(formatRate(500), '500 B/s');
  assert.equal(formatSpeedLimit('unlimited'), 'Unlimited');
  assert.equal(formatSpeedLimit({ bytes_per_second: 3 * MIB }), '3 MiB/s');
  assert.deepEqual(speedFields('unlimited'), { limited: false });
  assert.deepEqual(speedFields({ bytes_per_second: 2 * MIB }), { limited: true, amount: '2', unit: 'MiB' });
  assert.deepEqual(speedFields({ bytes_per_second: 1536 * 1024 }), { limited: true, amount: '1.5', unit: 'MiB' });
  assert.deepEqual(speedFields({ bytes_per_second: 512 * 1024 }), { limited: true, amount: '512', unit: 'KiB' });
});

test('a shown rate is exact, so a limit or bound always parses back to itself', () => {
  // 5,000,000 B/s is 4.76837158203125 MiB/s: no MiB/s form with three
  // decimals is exact, so it shows exactly in KiB/s.
  assert.equal(formatRate(5_000_000), '4882.8125 KiB/s');
  assert.equal(formatRate(1_000_001), '976.563 KiB/s');
  assert.deepEqual(speedFields({ bytes_per_second: 5_000_000 }), { limited: true, amount: '4882.8125', unit: 'KiB' });
  // A MiB/s value typed with three decimals shows as it was typed.
  assert.deepEqual(speedFields({ bytes_per_second: Math.round(2.345 * MIB) }), {
    limited: true,
    amount: '2.345',
    unit: 'MiB',
  });
  const pinned = { ...SPEED, min: 5_000_000, max: 5_000_000, unlimited: false };
  assert.equal(
    describeSpeedLimit(/** @type {any} */ ({ ...pinned, value: { bytes_per_second: 5_000_000 } }), false),
    'Allowed: 4882.8125 KiB/s–4882.8125 KiB/s; Unlimited is not allowed here. Using the default.',
  );
  assert.deepEqual(parseRate('4882.8125', 'KiB', pinned), { value: 5_000_000, error: null });
  // Across the whole range, the form's amount and every displayed rate
  // parse back to exactly the rate they show.
  const sizes = { 'KiB/s': 2 ** 10, 'MiB/s': 2 ** 20, 'GiB/s': 2 ** 30, 'TiB/s': 2 ** 40 };
  for (let i = 0; i < 2000; i += 1) {
    const bytes = 1024 + Math.floor(((i * 2654435761) % 2 ** 32) / 2 ** 32 * (2 ** 40 - 1024));
    const fields = speedFields({ bytes_per_second: bytes });
    assert.ok(fields.limited);
    assert.equal(parseRate(fields.amount, fields.unit, SPEED).value, bytes, `${bytes}`);
    const [amount, unit] = formatRate(bytes).split(' ');
    assert.equal(Math.round(Number(amount) * sizes[/** @type {keyof typeof sizes} */ (unit)]), bytes, `${bytes}`);
  }
});

test('a typed speed is converted to whole bytes per second within the allowed range', () => {
  const capped = { ...SPEED, min: 64 * 1024, max: 4 * MIB, unlimited: false };
  assert.deepEqual(parseRate('2', 'MiB', capped), { value: 2 * MIB, error: null });
  // A fraction is rounded to the nearest byte, then checked.
  assert.deepEqual(parseRate(' 1.5 ', 'MiB', capped), { value: 1.5 * MIB, error: null });
  assert.deepEqual(parseRate('64.0004', 'KiB', capped), { value: 65536, error: null });
  assert.deepEqual(parseRate('4096', 'KiB', capped), { value: 4 * MIB, error: null });
  const error = 'Enter a speed from 64 KiB/s to 4 MiB/s.';
  for (const [text, unit] of [
    ['', 'MiB'],
    ['0', 'MiB'],
    ['63.9', 'KiB'],
    ['4.001', 'MiB'],
    ['-1', 'MiB'],
    ['1e3', 'KiB'],
    ['two', 'MiB'],
    ['1,5', 'MiB'],
    ['99999999999999999999', 'MiB'],
    ['1', 'GiB'],
  ]) {
    assert.deepEqual(parseRate(text, /** @type {any} */ (unit), capped), { value: null, error }, `${text} ${unit}`);
  }
});

test('the speed limit says what it allows, including whether Unlimited is', () => {
  assert.equal(describeSpeedLimit(SPEED, false), 'Allowed: Unlimited, or 1 KiB/s–1 TiB/s. Using the default.');
  const capped = { ...SPEED, max: 4 * MIB, unlimited: false, default: { bytes_per_second: MIB }, source: 'server' };
  assert.equal(
    describeSpeedLimit(/** @type {any} */ ({ ...capped, value: { bytes_per_second: MIB } }), false),
    'Allowed: 1 KiB/s–4 MiB/s; Unlimited is not allowed here. Using the server’s default.',
  );
  assert.equal(
    describeSpeedLimit(
      /** @type {any} */ ({ ...capped, source: 'saved', saved: 'unlimited', value: { bytes_per_second: 4 * MIB }, conflict: true }),
      false,
    ),
    'Allowed: 1 KiB/s–4 MiB/s; Unlimited is not allowed here. Your saved value Unlimited is outside it, so 4 MiB/s is used.',
  );
  assert.equal(
    describeSpeedLimit(/** @type {any} */ ({ ...SPEED, source: 'saved', saved: { bytes_per_second: MIB }, value: { bytes_per_second: MIB } }), true),
    'Allowed: Unlimited, or 1 KiB/s–1 TiB/s. Saved; the default is Unlimited.',
  );
  assert.equal(
    describeSpeedLimit(
      /** @type {any} */ ({ ...capped, locked: true, saved: 'unlimited', value: { bytes_per_second: MIB }, conflict: true }),
      false,
    ),
    'Managed by server configuration. Your saved value Unlimited is not used.',
  );
});

