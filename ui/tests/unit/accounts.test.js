// What the Accounts & services section says about an account and the
// vault, without a page.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { accountStatus, vaultProblem } from '../../src/components/accounts.js';

/** @type {import('../../src/client/contract.js').AccountSummary} */
const ACCOUNT = /** @type {any} */ ({
  id: 'acct-1',
  service: 'real-debrid',
  revision: 1,
  enabled: true,
  status: 'authenticated',
  added_at: 0,
});

test('statuses: locked and disabled come before what the service said', () => {
  assert.deepEqual(accountStatus(ACCOUNT, 'ready'), ['Signed in', 'ok']);
  assert.deepEqual(accountStatus({ ...ACCOUNT, status: 'invalid_credentials' }, 'ready'), ['Sign in again', 'bad']);
  assert.deepEqual(accountStatus({ ...ACCOUNT, status: 'unavailable' }, 'ready'), ['Could not check', 'warn']);
  assert.deepEqual(accountStatus({ ...ACCOUNT, status: 'not_checked' }, 'ready'), ['Not checked', null]);
  assert.deepEqual(accountStatus({ ...ACCOUNT, enabled: false }, 'ready'), ['Disabled', null]);
  assert.deepEqual(accountStatus(ACCOUNT, 'locked'), ['Locked', 'warn']);
});

test('the vault says why accounts cannot be used, without commands', () => {
  /** @type {import('../../src/client/contract.js').Accounts} */
  const accounts = { vault: 'ready', services: [], accounts: [] };
  assert.equal(vaultProblem(accounts, false), null);
  const missing = vaultProblem({ ...accounts, vault: 'not_configured' }, false);
  assert.match(String(missing), /the server has no key/);
  assert.match(String(vaultProblem({ ...accounts, vault: 'not_configured' }, true)), /this app has no key/);
  const locked = String(vaultProblem({ ...accounts, vault: 'locked', vault_problem: 'the key file is missing' }, false));
  assert.match(locked, /the key file is missing/);
  assert.match(locked, /disable or remove/);
  for (const text of [missing, locked]) assert.doesNotMatch(String(text), /--|wayfetch(d)? /);
});
