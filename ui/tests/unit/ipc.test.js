// The desktop transport against an in-memory stand-in for Tauri's IPC:
// arguments in the protocol's shape, refusals as `WayfetchError`s, and
// the subscription's acknowledgement, end, and resync rules. The real
// window is exercised by `apps/wayfetch-desktop/tests/shell.rs`.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { WayfetchError } from '../../src/client/contract.js';
import { ACK_EVERY, DesktopIpcClient } from '../../src/transports/ipc.js';

/**
 * @typedef {import('../../src/client/contract.js').StreamItem} StreamItem
 */

class FakeChannel {
  constructor() {
    /** @type {(message: any) => void} */
    this.onmessage = () => {};
  }
}

/** Records invocations and answers them from `answers`. */
class FakeCore {
  /** @param {Record<string, (args: any) => unknown>} answers */
  constructor(answers = {}) {
    this.answers = answers;
    /** @type {{ command: string, args: any }[]} */
    this.calls = [];
    this.Channel = FakeChannel;
  }

  /**
   * @param {string} command
   * @param {any} [args]
   */
  async invoke(command, args) {
    this.calls.push({ command, args });
    const answer = this.answers[command];
    if (!answer) throw new Error(`${command} not allowed`);
    return answer(args);
  }

  /** @param {string} command */
  calledWith(command) {
    return this.calls.filter((c) => c.command === command).map((c) => c.args);
  }
}

const settle = () => new Promise((resolve) => setImmediate(resolve));

/** @type {StreamItem} */
const HELLO = /** @type {any} */ ({ type: 'hello', instance: 'i', boot_epoch: 1 });

test('queries and commands carry the protocol JSON', async () => {
  const core = new FakeCore({ query: () => ({ type: 'jobs' }), job_command: () => ({}), submit: () => ({}) });
  const client = new DesktopIpcClient(core);
  assert.ok(client.permissions.includes('delete_files'));
  await client.listJobs({ after: /** @type {any} */ ('job-3'), pageSize: 50, stages: ['paused'] });
  await client.listJobs({ after: null, pageSize: 50, stages: null });
  await client.inspectJob(/** @type {any} */ ('job-1'));
  await client.countJobs();
  await client.submit({ idempotencyKey: 'k', target: 't', items: ['https://a.example/'] });
  await client.jobCommand(/** @type {any} */ ('job-1'), {
    idempotencyKey: 'k2',
    target: 't',
    action: 'pause',
    expectedRevision: 4,
  });
  assert.deepEqual(core.calledWith('query'), [
    { query: { type: 'list_jobs', page_size: 50, after: 'job-3', filter: { stages: ['paused'] } } },
    { query: { type: 'list_jobs', page_size: 50 } },
    { query: { type: 'inspect_job', job: 'job-1' } },
    { query: { type: 'count_jobs' } },
  ]);
  assert.deepEqual(core.calledWith('submit'), [
    { request: { idempotency_key: 'k', target: 't', items: ['https://a.example/'] } },
  ]);
  assert.deepEqual(core.calledWith('job_command'), [
    {
      job: 'job-1',
      request: { idempotency_key: 'k2', target: 't', action: 'pause', expected_revision: 4 },
    },
  ]);
});

test('refusals become WayfetchErrors with the protocol code', async () => {
  const core = new FakeCore({
    job_command: () => {
      throw { code: 'revision_conflict', message: 'changed', current_revision: 7 };
    },
  });
  const client = new DesktopIpcClient(core);
  await assert.rejects(
    client.jobCommand(/** @type {any} */ ('job-1'), {
      idempotencyKey: 'k',
      target: 't',
      action: 'resume',
      expectedRevision: 3,
    }),
    (/** @type {WayfetchError} */ error) =>
      error instanceof WayfetchError && error.code === 'revision_conflict' && error.currentRevision === 7,
  );
  // The host could not run the command at all.
  await assert.rejects(client.hello(), (/** @type {WayfetchError} */ error) => error.code === 'unavailable');
});

/** A subscription whose items the test pushes through its channel. */
async function subscription(core = new FakeCore({ subscribe: () => 9, acknowledge: () => {}, unsubscribe: () => {} })) {
  const client = new DesktopIpcClient(core);
  /** @type {(StreamItem | 'end')[]} */
  const seen = [];
  const handle = client.subscribe(null, {
    item: (item) => seen.push(item),
    end: () => seen.push('end'),
  });
  const channel = /** @type {FakeChannel} */ (core.calledWith('subscribe')[0].channel);
  await settle();
  return { core, seen, handle, channel };
}

test('items are acknowledged in batches the host can count on', async () => {
  const { core, seen, channel } = await subscription();
  for (let i = 0; i < ACK_EVERY * 2 + 3; i += 1) channel.onmessage(HELLO);
  assert.equal(seen.length, ACK_EVERY * 2 + 3);
  assert.deepEqual(core.calledWith('acknowledge'), [
    { subscription: 9, received: ACK_EVERY },
    { subscription: 9, received: ACK_EVERY * 2 },
  ]);
});

test('null from the host ends the stream once', async () => {
  const { seen, channel } = await subscription();
  channel.onmessage(HELLO);
  channel.onmessage(null);
  channel.onmessage(HELLO);
  assert.deepEqual(seen, [HELLO, 'end']);
});

test('a resync is delivered last and needs no end', async () => {
  const { core, seen, channel } = await subscription();
  /** @type {StreamItem} */
  const resync = { type: 'resync', reason: 'lagged' };
  channel.onmessage(resync);
  channel.onmessage(null);
  assert.deepEqual(seen, [resync]);
  assert.deepEqual(core.calledWith('unsubscribe'), [{ subscription: 9 }]);
});

test('closing unsubscribes, also when closed before the host answered', async () => {
  const { core, handle } = await subscription();
  handle.close();
  assert.deepEqual(core.calledWith('unsubscribe'), [{ subscription: 9 }]);

  const early = new FakeCore({ subscribe: () => 5, unsubscribe: () => {} });
  const client = new DesktopIpcClient(early);
  client.subscribe(null, { item: () => {}, end: () => {} }).close();
  await settle();
  assert.deepEqual(early.calledWith('unsubscribe'), [{ subscription: 5 }]);
});

test('a refused subscription ends like a lost stream', async () => {
  const core = new FakeCore({
    subscribe: () => {
      throw { code: 'overloaded', message: 'too many open event subscriptions' };
    },
  });
  const { seen } = await subscription(core);
  assert.deepEqual(seen, ['end']);
});

test('account commands carry no credentials, only the protocol JSON', async () => {
  const core = new FakeCore({
    query: () => ({ type: 'accounts' }),
    authorize_account: () => ({ type: 'authorization_started' }),
    cancel_authorization: () => ({}),
    account_command: () => ({}),
    refresh_account: () => ({}),
  });
  const client = new DesktopIpcClient(core);
  assert.ok(client.permissions.includes('accounts'));
  await client.accounts();
  await client.authorization(/** @type {any} */ ('6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a11'));
  await client.authorizeAccount({ idempotencyKey: 'k', target: 't', service: 'real-debrid', alias: 'Main' });
  await client.authorizeAccount({
    idempotencyKey: 'k2',
    target: 't',
    service: 'real-debrid',
    account: { account: /** @type {any} */ ('acct-1'), revision: 3 },
  });
  await client.cancelAuthorization(/** @type {any} */ ('6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a11'));
  await client.accountCommand(/** @type {any} */ ('acct-1'), {
    idempotencyKey: 'k3',
    target: 't',
    action: 'disable',
    expectedRevision: 3,
  });
  await client.refreshAccount(/** @type {any} */ ('acct-1'));
  assert.deepEqual(core.calledWith('query'), [
    { query: { type: 'accounts' } },
    { query: { type: 'authorization', id: '6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a11' } },
  ]);
  assert.deepEqual(core.calledWith('authorize_account'), [
    { request: { idempotency_key: 'k', target: 't', service: 'real-debrid', alias: 'Main' } },
    { request: { idempotency_key: 'k2', target: 't', service: 'real-debrid', account: { account: 'acct-1', revision: 3 } } },
  ]);
  assert.deepEqual(core.calledWith('cancel_authorization'), [{ authorization: '6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a11' }]);
  assert.deepEqual(core.calledWith('account_command'), [
    { account: 'acct-1', request: { idempotency_key: 'k3', target: 't', action: 'disable', expected_revision: 3 } },
  ]);
  assert.deepEqual(core.calledWith('refresh_account'), [{ account: 'acct-1' }]);
});

test('update checks are an engine query', async () => {
  const core = new FakeCore({ query: () => ({ type: 'updates' }) });
  await new DesktopIpcClient(core).checkForUpdates();
  assert.deepEqual(core.calledWith('query'), [{ query: { type: 'updates' } }]);
});

test('collected starts preserve automatic default and explicit route overrides over IPC', async () => {
  const core = new FakeCore({ links_command: () => ({ type: 'links_applied', applied: [], refused: [] }) });
  const client = new DesktopIpcClient(core);
  const request = { idempotencyKey: 'k', target: 't', action: /** @type {const} */ ('start'), links: [{ link: /** @type {any} */ ('src-1'), revision: 1 }] };
  await client.linksCommand(request);
  await client.linksCommand({ ...request, routing: 'direct' });
  await client.linksCommand({ ...request, account: /** @type {any} */ ('acct-1') });
  const base = { idempotency_key: 'k', target: 't', action: 'start', links: request.links };
  assert.deepEqual(core.calledWith('links_command'), [{ request: base }, { request: { ...base, routing: 'direct' } }, { request: { ...base, account: 'acct-1' } }]);
});
