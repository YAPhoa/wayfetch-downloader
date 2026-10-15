// The feed's snapshot, replay, resync, and reconnection rules against an
// in-memory client.

import assert from 'node:assert/strict';
import { afterEach, test } from 'node:test';

import { PROTOCOL, WayfetchError } from '../../src/client/contract.js';
import { JobFeed } from '../../src/client/feed.js';

/**
 * @typedef {import('../../src/client/contract.js').JobSummary} JobSummary
 * @typedef {import('../../src/client/contract.js').StreamSink} StreamSink
 * @typedef {import('../../src/client/contract.js').EventCursor} EventCursor
 */

/**
 * @param {number} n
 * @param {Partial<JobSummary>} [fields]
 * @returns {JobSummary}
 */
function job(n, fields = {}) {
  return {
    id: /** @type {JobSummary['id']} */ (`job-${n}`),
    package: 'pkg-1',
    stage: 'queued',
    desired: 'running',
    display: 'queued',
    revision: 1,
    generation: 1,
    bytes_done: 0,
    ...fields,
  };
}

/** An engine in memory, with a durable event log. */
class FakeClient {
  constructor() {
    this.instance = 'instance-a';
    this.epoch = 1;
    this.protocol = { ...PROTOCOL };
    /** @type {Map<string, JobSummary>} */
    this.jobs = new Map();
    this.head = 0;
    /** @type {{ after: EventCursor | null, sink: StreamSink, closed: boolean, close: () => void }[]} */
    this.subscriptions = [];
    /** @type {WayfetchError | null} */
    this.refuse = null;
    this.pageReads = 0;
    this.countReads = 0;
    /** @type {readonly import('../../src/client/contract.js').Permission[]} */
    this.permissions = ['read'];
    this.local = false;
  }

  /** @returns {Promise<string>} */
  async jobSource() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').LinkPage>} */
  async listLinks() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').CommandResult>} */
  async collectLinks() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').CommandResult>} */
  async linksCommand() {
    throw new Error('not used');
  }

  /** @returns {Promise<string>} */
  async linkSource() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').Settings>} */
  async settings() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').CommandResult>} */
  async updateSettings() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').CommandResult>} */
  async removeRoot() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').Accounts>} */
  async accounts() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').CommandResult>} */
  async authorizeAccount() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').Authorization>} */
  async authorization() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').Authorization>} */
  async cancelAuthorization() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').CommandResult>} */
  async accountCommand() {
    throw new Error('not used');
  }

  /** @returns {Promise<import('../../src/client/contract.js').AccountSummary>} */
  async refreshAccount() {
    throw new Error('not used');
  }

  get live() {
    return this.subscriptions.filter((s) => !s.closed);
  }

  cursor() {
    return { instance: this.instance, position: this.head };
  }

  async hello() {
    if (this.refuse) throw this.refuse;
    return {
      instance: this.instance,
      boot_epoch: this.epoch,
      protocol: this.protocol,
      head: this.cursor(),
      limits: { max_page_size: 500, max_submit_items: 1000, max_url_bytes: 8192, max_collected_links: 10000 },
    };
  }

  /** @param {{ after?: string | null, pageSize: number, stages?: readonly string[] | null }} request */
  async listJobs({ after, pageSize, stages }) {
    if (this.refuse) throw this.refuse;
    this.pageReads += 1;
    const start = after ? Number(after.slice(4)) : 0;
    const all = [...this.jobs.values()].filter(
      (j) => Number(j.id.slice(4)) > start && (!stages || stages.includes(j.stage)),
    );
    const items = all.slice(0, pageSize);
    return {
      items,
      next_after: all.length > pageSize ? items.at(-1)?.id : undefined,
      as_of: this.cursor(),
    };
  }

  /** @param {string} id */
  async inspectJob(id) {
    const found = this.jobs.get(id);
    if (!found) throw new WayfetchError('not_found', 'no such job');
    return { job: found, as_of: this.cursor() };
  }

  async countJobs() {
    if (this.refuse) throw this.refuse;
    this.countReads += 1;
    /** @type {Record<string, number>} */
    const stages = {};
    for (const { stage } of this.jobs.values()) stages[stage] = (stages[stage] ?? 0) + 1;
    return { stages, as_of: this.cursor() };
  }

  /** @returns {Promise<never>} */
  async submit() {
    throw new Error('unused');
  }

  /** @returns {Promise<never>} */
  async jobCommand() {
    throw new Error('unused');
  }

  /**
   * @param {EventCursor | null} after
   * @param {StreamSink} sink
   */
  subscribe(after, sink) {
    const subscription = {
      after,
      sink,
      closed: false,
      close() {
        this.closed = true;
      },
    };
    this.subscriptions.push(subscription);
    queueMicrotask(() => sink.item({ type: 'hello', ...this.helloSync() }));
    return subscription;
  }

  helloSync() {
    return {
      instance: this.instance,
      boot_epoch: this.epoch,
      protocol: this.protocol,
      head: this.cursor(),
      limits: { max_page_size: 500, max_submit_items: 1000, max_url_bytes: 8192, max_collected_links: 10000 },
    };
  }

  /**
   * Records a change durably and sends it to open streams.
   * @param {'job_added' | 'job_changed'} type
   * @param {JobSummary} summary
   */
  change(type, summary) {
    this.jobs.set(summary.id, summary);
    this.head += 1;
    const item = /** @type {const} */ ({
      type: 'event',
      cursor: this.cursor(),
      kind: { type, job: summary },
    });
    for (const s of this.live) s.sink.item(item);
    return item;
  }

  /**
   * Removes a job from lists durably and tells open streams.
   * @param {JobSummary['id']} id
   */
  remove(id) {
    this.jobs.delete(id);
    this.head += 1;
    const item = /** @type {const} */ ({
      type: 'event',
      cursor: this.cursor(),
      kind: { type: 'job_removed', job: id },
    });
    for (const s of this.live) s.sink.item(item);
  }
}

/** Lets pending promises and timers of zero delay run. */
async function settle() {
  for (let i = 0; i < 10; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
}

/** @type {JobFeed[]} */
const feeds = [];

/**
 * @param {FakeClient} client
 * @param {number} [pageSize]
 * @param {number} [readDelay]
 * @param {(kind: import('../../src/client/contract.js').EventKind | null) => void} [onEvent]
 */
async function started(client, pageSize = 2, readDelay = 0, onEvent = undefined) {
  const feed = new JobFeed(client, {
    pageSize,
    onChange: () => {},
    onEvent,
    retryDelays: [0],
    refreshInterval: 1e9,
    readDelay,
  });
  feeds.push(feed);
  feed.start();
  await settle();
  return feed;
}

afterEach(() => {
  for (const feed of feeds.splice(0)) feed.stop();
});

test('follows events after the snapshot and ignores replays', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  const feed = await started(client);
  assert.equal(feed.status, 'live');
  assert.deepEqual(client.live[0].after, { instance: 'instance-a', position: 1 });

  const changed = client.change('job_changed', job(1, { stage: 'transferring', revision: 2 }));
  assert.equal(feed.jobs[0].stage, 'transferring');
  // A replay of an applied event, then an older copy, change nothing.
  client.live[0].sink.item(changed);
  client.live[0].sink.item({
    type: 'event',
    cursor: { instance: 'instance-a', position: 1 },
    kind: { type: 'job_changed', job: job(1) },
  });
  assert.equal(feed.jobs[0].stage, 'transferring');
});

test('tells of each new event once, and of each connection', async () => {
  const client = new FakeClient();
  /** @type {(string | null)[]} */
  const told = [];
  await started(client, 2, 0, (kind) => told.push(kind && kind.type));
  assert.deepEqual(told, [null]);
  const added = client.change('job_added', job(1));
  client.live[0].sink.item(added);
  assert.deepEqual(told, [null, 'job_added']);
});

test('new jobs join the last page until it is full', async () => {
  const client = new FakeClient();
  const feed = await started(client);
  client.change('job_added', job(1));
  client.change('job_added', job(2));
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-1', 'job-2']);
  assert.equal(feed.nextAfter, null);
  client.change('job_added', job(3));
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-1', 'job-2']);
  assert.equal(feed.nextAfter, 'job-2');

  await feed.showPage('job-2');
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-3']);
  // Jobs the feed does not hold are not kept.
  client.change('job_changed', job(1, { stage: 'paused' }));
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-3']);
  assert.equal(feed.held('job-1'), undefined);
});

test('a removed job leaves the page, the counts, and the inspector', async () => {
  const client = new FakeClient();
  for (const n of [1, 2, 3]) client.change('job_added', job(n, { stage: 'complete' }));
  const feed = await started(client);
  await feed.watch('job-1');
  await settle();
  assert.equal(feed.watchedState, 'ready');
  const counted = client.countReads;
  client.remove('job-1');
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-2']);
  assert.equal(feed.watchedJob, null);
  assert.equal(feed.watchedState, 'missing');
  await settle();
  // The next job moves up, and the counts are read again.
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-2', 'job-3']);
  assert.ok(client.countReads > counted);
  assert.deepEqual(feed.counts?.stages, { complete: 2 });
});

test('a resync takes a new snapshot and subscribes after it', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  const feed = await started(client);
  const first = client.live[0];
  // Changes the stream never delivered.
  first.closed = true;
  client.change('job_changed', job(1, { stage: 'failed' }));
  first.sink.item({ type: 'resync', reason: 'lagged' });
  await settle();
  assert.equal(feed.jobs[0].stage, 'failed');
  assert.equal(client.live.length, 1);
  assert.deepEqual(client.live[0].after, { instance: 'instance-a', position: 2 });
});

test('a lost stream reconnects from its cursor without a new snapshot', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  const feed = await started(client);
  const reads = client.pageReads;
  const first = client.live[0];
  first.closed = true;
  first.sink.end();
  assert.equal(feed.status, 'reconnecting');
  client.change('job_changed', job(1, { stage: 'paused' }));
  await settle();
  assert.equal(feed.status, 'live');
  assert.equal(client.pageReads, reads);
  assert.deepEqual(client.live[0].after, { instance: 'instance-a', position: 1 });
});

test('a refused handshake stops with the reason', async () => {
  const client = new FakeClient();
  client.refuse = new WayfetchError('unauthenticated', 'session ended');
  const feed = await started(client);
  assert.equal(feed.status, 'unauthenticated');
  assert.equal(client.subscriptions.length, 0);

  const newer = new FakeClient();
  newer.protocol = { major: PROTOCOL.major + 1, minor: 0 };
  assert.equal((await started(newer)).status, 'incompatible');
});

test('an unreachable engine is retried until it answers', async () => {
  const client = new FakeClient();
  client.refuse = new WayfetchError('unavailable', 'down');
  const feed = await started(client);
  assert.equal(feed.status, 'reconnecting');
  client.refuse = null;
  await settle();
  assert.equal(feed.status, 'live');
});

test('another instance at the same address means a new snapshot', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  const feed = await started(client);
  client.live[0].closed = true;
  client.instance = 'instance-b';
  client.jobs.clear();
  client.head = 0;
  client.change('job_added', job(7));
  client.subscriptions[0].sink.end();
  await settle();
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-7']);
  assert.deepEqual(client.live[0].after, { instance: 'instance-b', position: 1 });
});

test('only current progress samples are shown', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1, { stage: 'transferring', generation: 2 }));
  const feed = await started(client);
  const sink = client.live[0].sink;
  const sample = { job: /** @type {const} */ ('job-1'), bytes_received: 5, bytes_per_second: 1 };
  sink.item({ type: 'progress', boot_epoch: 0, generation: 2, ...sample });
  sink.item({ type: 'progress', boot_epoch: 1, generation: 1, ...sample });
  assert.equal(feed.sample('job-1'), undefined);
  sink.item({ type: 'progress', boot_epoch: 1, generation: 2, ...sample });
  assert.equal(feed.sample('job-1')?.bytes_received, 5);
  // A new generation makes the sample stale.
  client.change('job_changed', job(1, { stage: 'queued', generation: 3 }));
  assert.equal(feed.sample('job-1'), undefined);
});

test('a read overtaken by events keeps the newer copy', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  const feed = await started(client);
  // The page read answers as of position 1 but arrives after event 2.
  const list = client.listJobs.bind(client);
  client.listJobs = async (request) => {
    const page = await list(request);
    client.change('job_changed', job(1, { stage: 'paused' }));
    return { ...page, items: [job(1)], as_of: { instance: 'instance-a', position: 1 } };
  };
  await feed.showPage(null);
  assert.equal(feed.jobs[0].stage, 'paused');
});

test('an inspected job follows events and can be missing', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  client.change('job_added', job(2));
  client.change('job_added', job(3));
  const feed = await started(client, 1);
  await feed.watch('job-3');
  assert.equal(feed.watchedJob?.id, 'job-3');
  client.change('job_changed', job(3, { stage: 'complete' }));
  assert.equal(feed.watchedJob?.stage, 'complete');
  await feed.watch('job-9');
  assert.equal(feed.watchedState, 'missing');
  assert.equal(feed.watchedJob, null);
});

/**
 * Makes the client's next page read wait until the returned function is
 * called, answering as the client stood when the read was made.
 * @param {FakeClient} client
 */
function delayNextPageRead(client) {
  const list = client.listJobs.bind(client);
  /** @type {() => void} */
  let release = () => {};
  client.listJobs = async (request) => {
    client.listJobs = list;
    const page = await list(request);
    await new Promise((resolve) => {
      release = () => resolve(undefined);
    });
    return page;
  };
  return () => release();
}

test('another instance with reused job IDs replaces everything held', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  for (let revision = 2; revision <= 5; revision += 1) {
    client.change('job_changed', job(1, { stage: 'transferring', revision }));
  }
  const feed = await started(client);
  assert.equal(feed.jobs[0].stage, 'transferring');
  // A new state directory at the same address, whose job-1 is at a lower
  // event position.
  client.live[0].closed = true;
  client.instance = 'instance-b';
  client.jobs.clear();
  client.head = 0;
  client.change('job_added', job(1, { package: 'pkg-9' }));
  client.subscriptions[0].sink.end();
  await settle();
  assert.equal(feed.status, 'live');
  assert.deepEqual(feed.jobs, [job(1, { package: 'pkg-9' })]);
  assert.deepEqual(client.live[0].after, { instance: 'instance-b', position: 1 });
});

test('a read answered by the previous instance is dropped', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1, { stage: 'transferring' }));
  const feed = await started(client);
  const release = delayNextPageRead(client);
  const stale = feed.showPage(null);
  await settle();
  client.live[0].closed = true;
  client.instance = 'instance-b';
  client.jobs.clear();
  client.head = 0;
  client.change('job_added', job(1));
  client.subscriptions[0].sink.end();
  await settle();
  assert.equal(feed.jobs[0].stage, 'queued');
  release();
  await stale;
  assert.equal(feed.jobs[0].stage, 'queued');
});

test('an older page read never replaces the page chosen after it', async () => {
  const client = new FakeClient();
  for (let n = 1; n <= 3; n += 1) client.change('job_added', job(n));
  const feed = await started(client);
  const release = delayNextPageRead(client);
  const second = feed.showPage('job-2');
  await settle();
  await feed.showPage(null);
  release();
  await second;
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-1', 'job-2']);
  assert.equal(feed.nextAfter, 'job-2');

});

test('of two reads of the same page, the later one stands', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  const feed = await started(client);
  client.live[0].closed = true;
  const release = delayNextPageRead(client);
  const first = feed.showPage(null);
  await settle();
  // Added after the first read, and not delivered by the stream.
  client.change('job_added', job(2));
  await feed.showPage(null);
  release();
  await first;
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-1', 'job-2']);
});

test('a stage filter lists, adds, and drops jobs by stage', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1, { stage: 'paused' }));
  client.change('job_added', job(2));
  const feed = await started(client);
  await feed.showStages(['queued']);
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-2']);
  client.change('job_added', job(3, { stage: 'paused' }));
  client.change('job_added', job(4));
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-2', 'job-4']);
  client.change('job_changed', job(2, { stage: 'transferring', revision: 2 }));
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-4']);
  await feed.showStages(null);
  assert.deepEqual(feed.jobs.map((j) => j.id), ['job-1', 'job-2']);
});

test('counts come with the snapshot and follow job events, one read per burst', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  client.change('job_added', job(2, { stage: 'transferring', display: 'transferring' }));
  const feed = await started(client, 2, 50);
  assert.deepEqual(feed.counts?.stages, { queued: 1, transferring: 1 });
  assert.equal(feed.counts?.as_of.position, 2);
  assert.equal(client.countReads, 1);

  client.change('job_added', job(3));
  client.change('job_changed', job(1, { stage: 'paused', display: 'paused', revision: 2 }));
  client.change('job_added', job(4));
  await settle();
  assert.equal(client.countReads, 1);
  await new Promise((resolve) => setTimeout(resolve, 80));
  assert.equal(client.countReads, 2);
  assert.deepEqual(feed.counts?.stages, { queued: 2, paused: 1, transferring: 1 });
});

test('counts are forgotten with the instance that answered them', async () => {
  const client = new FakeClient();
  client.change('job_added', job(1));
  const feed = await started(client);
  assert.deepEqual(feed.counts?.stages, { queued: 1 });

  client.instance = 'instance-b';
  client.jobs.clear();
  client.head = 0;
  for (const s of client.live) s.sink.item({ type: 'hello', ...client.helloSync() });
  await settle();
  assert.deepEqual(feed.counts?.stages, {});
  assert.equal(feed.counts?.as_of.instance, 'instance-b');
});

test('jobs entering or leaving a stage filter are listed or replaced', async () => {
  const client = new FakeClient();
  const done = /** @type {const} */ ({ stage: 'complete', display: 'complete' });
  client.change('job_added', job(1, { stage: 'transferring', display: 'transferring' }));
  for (const n of [2, 3, 4]) client.change('job_added', job(n, done));
  const feed = await started(client, 2);
  await feed.showStages(['complete']);
  const ids = () => feed.jobs.map((j) => j.id);
  assert.deepEqual(ids(), ['job-2', 'job-3']);

  client.change('job_changed', job(1, { ...done, revision: 2 }));
  await settle();
  assert.deepEqual(ids(), ['job-1', 'job-2']);

  client.change('job_changed', job(2, { stage: 'failed', display: 'failed', revision: 2 }));
  await settle();
  assert.deepEqual(ids(), ['job-1', 'job-3']);
  assert.equal(feed.nextAfter, 'job-3');
});
