import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createTheme } from '../../src/theme.js';

/** @param {string | null} saved @param {boolean} dark @param {boolean} [blocked] */
function device(saved, dark, blocked = false) {
  const media = new EventTarget();
  const system = Object.assign(media, { matches: dark });
  const root = /** @type {HTMLElement} */ (/** @type {unknown} */ ({ dataset: {} }));
  const host = /** @type {Window} */ (/** @type {unknown} */ ({
    matchMedia: () => system,
    localStorage: {
      getItem() { if (blocked) throw Error('blocked'); return saved; },
      /** @param {string} key @param {string} value */
      setItem(key, value) { assert.equal(key, 'wayfetch.theme'); if (blocked) throw Error('blocked'); saved = value; },
    },
  }));
  return {
    root, host, saved: () => saved,
    /** @param {boolean} dark */
    os(dark) { system.matches = dark; system.dispatchEvent(new Event('change')); },
  };
}

test('default and invalid preferences follow live system appearance', () => {
  for (const saved of [null, 'invalid', 'system']) {
    const d = device(saved, false);
    const theme = createTheme(d.host, d.root);
    assert.equal(theme.get(), 'system');
    assert.equal(d.root.dataset.theme, 'light');
    d.os(true);
    assert.equal(d.root.dataset.theme, 'dark');
    d.os(false);
    assert.equal(d.root.dataset.theme, 'light');
  }
});

test('explicit themes override system, persist across initialization, and return to System', () => {
  const d = device(null, true);
  const theme = createTheme(d.host, d.root);
  for (const choice of /** @type {const} */ (['light', 'dark'])) {
    theme.set(choice);
    d.os(false); d.os(true);
    assert.equal(d.root.dataset.theme, choice);
    assert.equal(d.saved(), choice);
    assert.equal(createTheme(device(d.saved(), false).host, d.root).get(), choice);
  }
  theme.set('system');
  assert.equal(d.saved(), 'system');
  assert.equal(d.root.dataset.theme, 'dark');
  d.os(false);
  assert.equal(d.root.dataset.theme, 'light');
});

test('blocked storage falls back to System and allows window-local overrides', () => {
  const d = device('dark', false, true);
  const theme = createTheme(d.host, d.root);
  assert.equal(theme.get(), 'system');
  theme.set('dark');
  assert.equal(d.root.dataset.theme, 'dark');
  theme.set('system');
  assert.equal(d.root.dataset.theme, 'light');
});
