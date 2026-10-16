import assert from 'node:assert/strict';
import { test } from 'node:test';

import { accountName, canChooseRoute, routeAccounts, routeLabel } from '../../src/client/routes.js';

/**
 * @param {Partial<import('../../src/client/contract.js').AccountSummary>} fields
 * @returns {import('../../src/client/contract.js').AccountSummary}
 */
function account(fields) {
  return {
    id: 'acct-1',
    service: 'real-debrid',
    revision: 1,
    enabled: true,
    status: 'authenticated',
    added_at: 0,
    ...fields,
  };
}

/**
 * @param {import('../../src/client/contract.js').AccountSummary[]} accounts
 * @returns {import('../../src/client/contract.js').Accounts}
 */
function listed(accounts, downloads = true) {
  return {
    vault: 'ready',
    services: [{ service: 'real-debrid', method: 'device_code', downloads }],
    accounts,
  };
}

const names = (/** @type {string} */ id) => (id === 'real-debrid' ? 'Real-Debrid' : id);

test('choosing a route needs both using and seeing accounts', () => {
  assert.equal(canChooseRoute(['read', 'submit', 'provider_downloads', 'accounts']), true);
  assert.equal(canChooseRoute(['read', 'submit', 'accounts']), false);
  assert.equal(canChooseRoute(['read', 'submit', 'provider_downloads']), false);
});

test('only enabled accounts of services that download are offered', () => {
  const on = account({ id: 'acct-1' });
  const off = account({ id: 'acct-2', enabled: false });
  const other = account({ id: 'acct-3', service: 'elsewhere' });
  assert.deepEqual(routeAccounts(listed([on, off, other])), [on]);
  assert.deepEqual(routeAccounts(listed([on], false)), []);
  assert.deepEqual(routeAccounts({ ...listed([on]), vault: 'locked' }), []);
  assert.deepEqual(routeAccounts(null), []);
});

test('routes are named without links or credentials', () => {
  assert.equal(routeLabel(undefined, null, names), 'Direct');
  const route = { service: 'real-debrid', account: 'acct-1' };
  assert.equal(routeLabel(route, null, names), 'Real-Debrid');
  assert.equal(routeLabel(route, listed([account({ alias: 'Main' })]), names), 'Real-Debrid, Main');
  assert.equal(routeLabel(route, listed([account({ username: 'someone' })]), names), 'Real-Debrid, someone');
  assert.equal(routeLabel(route, listed([]), names), 'Real-Debrid, removed account');
  assert.equal(accountName(account({}), 'Real-Debrid'), 'Real-Debrid account');
});
