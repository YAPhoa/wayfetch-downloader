// Keeps a bounded, current view of jobs over any `WayfetchClient`
// (architecture brief sections 4.7 and 7.8): a snapshot, then the event
// stream from the snapshot's cursor.
//
// - Only one page of jobs and one inspected job are held, never the whole
//   queue, and only events about those are applied.
// - Each held job remembers the event position its copy reflects, so a
//   replayed or older event never overwrites a newer copy.
// - When a stream ends, the feed reconnects with backoff, after a fresh
//   handshake, and resumes from its cursor; a `resync` makes it take a new
//   snapshot instead. A handshake refused as `unauthenticated` stops the
//   feed with that status, so the shell can ask the user to sign in again.
// - Progress samples are shown only for the current boot epoch, the
//   job's current generation, and while it is transferring; a job that
//   stops transferring drops its sample, so a paused row never shows a
//   speed. Samples carry the bytes received between durable checkpoints,
//   so no held job is re-read on a timer.
// - Job counts per stage come with the snapshot and are read again after
//   job events, at most one read at a time and one per `READ_DELAY`, so a
//   burst of events costs one count. While the list is filtered by stage, a
//   job entering or leaving the filter makes the page be read again the
//   same way, since its rows may shift.
//
// Nothing here affects downloads: stopping the feed, or losing the page
// that holds it, only stops watching.

import { WayfetchError, serves } from './contract.js';

/**
 * @typedef {import('./contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('./contract.js').Hello} Hello
 * @typedef {import('./contract.js').JobId} JobId
 * @typedef {import('./contract.js').JobSummary} JobSummary
 * @typedef {import('./contract.js').JobCounts} JobCounts
 * @typedef {import('./contract.js').JobStage} JobStage
 * @typedef {import('./contract.js').EventCursor} EventCursor
 * @typedef {import('./contract.js').StreamItem} StreamItem
 * @typedef {import('./contract.js').EventKind} EventKind
 * @typedef {import('./contract.js').ProgressSample} ProgressSample
 * @typedef {import('./contract.js').Subscription} Subscription
 *
 * @typedef {'connecting' | 'live' | 'reconnecting' | 'unauthenticated' | 'incompatible' | 'stopped'} FeedStatus
 * @typedef {{ job: JobSummary, position: number }} Held
 */

/** Waits between reconnection attempts, in milliseconds; the last repeats. */
export const RETRY_DELAYS = Object.freeze([500, 1000, 2000, 4000, 8000]);

/** How often a snapshot older than the stream is taken again. */
const SNAPSHOT_ATTEMPTS = 3;

/** How long job events gather before the counts or a filtered page are
 * read again, in milliseconds. */
export const READ_DELAY = 1000;

/**
 * Runs `read` once `delay` has passed after a request, one run at a time;
 * requests made while it runs make it run once more afterwards. Failures
 * are left to the event stream, which notices a lost server or session.
 */
class Coalesced {
  /**
   * @param {() => Promise<unknown>} read
   * @param {number} delay
   */
  constructor(read, delay) {
    this.read = read;
    this.delay = delay;
    this.wanted = false;
    this.running = false;
    /** @type {ReturnType<typeof setTimeout> | undefined} */
    this.timer = undefined;
  }

  request() {
    this.wanted = true;
    if (this.running || this.timer !== undefined) return;
    this.timer = setTimeout(async () => {
      this.timer = undefined;
      this.running = true;
      this.wanted = false;
      try {
        await this.read();
      } catch {
        // The stream reports it.
      } finally {
        this.running = false;
      }
      if (this.wanted) this.request();
    }, this.delay);
  }

  cancel() {
    clearTimeout(this.timer);
    this.timer = undefined;
    this.wanted = false;
  }
}

/**
 * The copy to hold: `current` if it reflects a later position than a copy
 * of `job` read at `position`, which can happen when events overtook a
 * read in flight.
 * @param {Held | null | undefined} current
 * @param {JobSummary} job
 * @param {number} position
 * @returns {Held}
 */
function newer(current, job, position) {
  return current && current.job.id === job.id && current.position > position
    ? current
    : { job, position };
}

export class JobFeed {
  /**
   * @param {WayfetchClient} client
   * @param {{
   *   pageSize: number,
   *   onChange: () => void,
   *   onEvent?: (kind: EventKind | null) => void,
   *   retryDelays?: readonly number[],
   *   readDelay?: number,
   * }} options
   */
  constructor(client, options) {
    this.client = client;
    this.pageSize = options.pageSize;
    this.onChange = options.onChange;
    /** Told of each durable event, and of each (re)connection with `null`. */
    this.onEvent = options.onEvent ?? (() => {});
    this.retryDelays = options.retryDelays ?? RETRY_DELAYS;
    const readDelay = options.readDelay ?? READ_DELAY;
    /** @type {FeedStatus} */
    this.status = 'stopped';
    /** @type {Hello | null} */
    this.hello = null;
    /** Where the current page starts: after this job, or at the first. */
    /** @type {JobId | null} */
    this.pageAfter = null;
    /** The stages the list shows, or null for every stage. */
    /** @type {readonly JobStage[] | null} */
    this.stages = null;
    /** The current page's jobs, in order. */
    /** @type {JobId[]} */
    this.pageIds = [];
    /** @type {JobId | null} */
    this.nextAfter = null;
    /**
     * How many listed jobs come before this page, as the engine last said;
     * `null` from an engine before protocol 4.3.
     * @type {number | null}
     */
    this.before = null;
    /** The inspected job, if any. */
    /** @type {JobId | null} */
    this.watched = null;
    /** `missing` when the inspected job does not exist. */
    /** @type {'loading' | 'ready' | 'missing'} */
    this.watchedState = 'loading';
    /** The last error that stopped the feed or a reconnection attempt. */
    /** @type {WayfetchError | null} */
    this.error = null;
    /** @type {Map<JobId, Held>} */
    this.page = new Map();
    /** @type {Held | null} */
    this.detail = null;
    /** @type {Map<JobId, ProgressSample>} */
    this.samples = new Map();
    /** Jobs per stage, once read. */
    /** @type {JobCounts | null} */
    this.counts = null;
    /** Reads the counts again after job events. */
    this.recount = new Coalesced(() => this.loadCounts(this.run), readDelay);
    /** Reads a filtered page again after jobs enter or leave the filter. */
    this.repage = new Coalesced(() => this.loadPage(this.run), readDelay);
    /** The last durable event applied, or the snapshot's position. */
    /** @type {EventCursor | null} */
    this.cursor = null;
    /** @type {Subscription | null} */
    this.subscription = null;
    /** Increases on every start and stop, so stale work gives up. */
    this.run = 0;
    /** Increases when held state is forgotten, so reads issued before it
     * are dropped. */
    this.view = 0;
    /** Numbers page reads, so an older answer never replaces a newer one. */
    this.pageRequests = 0;
    /** The number of the page read last shown. */
    this.pageShown = 0;
    this.attempt = 0;
    /** @type {ReturnType<typeof setTimeout> | undefined} */
    this.timer = undefined;
  }

  /** The current page's jobs, in order. */
  get jobs() {
    return this.pageIds.flatMap((id) => {
      const held = this.page.get(id);
      return held ? [held.job] : [];
    });
  }

  /** The inspected job, once loaded. */
  get watchedJob() {
    return this.detail?.job ?? null;
  }

  /**
   * A live progress sample for `job`, if a current one arrived.
   * @param {JobId} job
   */
  sample(job) {
    return this.samples.get(job);
  }

  /** Connects, takes a snapshot, and follows changes. */
  start() {
    this.stop();
    const run = ++this.run;
    this.cursor = null;
    this.setStatus('connecting');
    void this.connect(run);
  }

  /** Stops following. Downloads are unaffected. */
  stop() {
    this.run += 1;
    clearTimeout(this.timer);
    this.recount.cancel();
    this.repage.cancel();
    this.subscription?.close();
    this.subscription = null;
    if (this.status !== 'unauthenticated' && this.status !== 'incompatible') {
      this.status = 'stopped';
    }
  }

  /**
   * Shows the page after `after`, or the first page.
   * @param {JobId | null} after
   */
  async showPage(after) {
    this.pageAfter = after;
    await this.loadPage(this.run);
  }

  /**
   * Lists only jobs in `stages`, or every job, from the first page.
   * @param {readonly JobStage[] | null} stages
   */
  async showStages(stages) {
    this.stages = stages;
    this.pageAfter = null;
    this.pageIds = [];
    this.page = new Map();
    this.nextAfter = null;
    this.changed();
    await this.loadPage(this.run);
  }

  /**
   * Whether the list's stage filter admits `job`.
   * @param {JobSummary} job
   */
  admits(job) {
    return this.stages === null || this.stages.includes(job.stage);
  }

  /**
   * Inspects `job`, or stops inspecting.
   * @param {JobId | null} job
   */
  async watch(job) {
    this.watched = job;
    this.detail = null;
    this.watchedState = 'loading';
    this.changed();
    if (job !== null) await this.loadDetail(this.run);
  }

  /**
   * Re-reads one job, for example after a command was refused because it
   * changed.
   * @param {JobId} job
   */
  async refresh(job) {
    const view = this.view;
    const snapshot = await this.client.inspectJob(job);
    if (!this.answers(view, snapshot.as_of)) return;
    this.hold(snapshot.job, snapshot.as_of.position);
    this.changed();
  }

  /** @param {FeedStatus} status */
  setStatus(status) {
    this.status = status;
    this.changed();
  }

  changed() {
    this.onChange();
  }

  /**
   * Handshake, snapshot if needed, subscribe; retried with backoff.
   * @param {number} run
   */
  async connect(run) {
    try {
      const hello = await this.client.hello();
      if (run !== this.run) return;
      if (!serves(hello.protocol)) {
        this.stop();
        this.setStatus('incompatible');
        return;
      }
      if (this.hello?.instance !== hello.instance) this.forget();
      this.acceptHello(hello);
      if (this.cursor === null) {
        await this.snapshot(run);
        if (run !== this.run) return;
      }
      this.subscribe(run);
    } catch (error) {
      if (run !== this.run) return;
      this.failed(run, /** @type {WayfetchError} */ (error));
    }
  }

  /**
   * @param {number} run
   * @param {WayfetchError} error
   */
  failed(run, error) {
    this.error = error;
    if (error.code === 'unauthenticated') {
      this.stop();
      this.setStatus('unauthenticated');
      return;
    }
    this.subscription?.close();
    this.subscription = null;
    this.setStatus('reconnecting');
    const delays = this.retryDelays;
    const delay = delays[Math.min(this.attempt, delays.length - 1)];
    this.attempt += 1;
    clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      if (run === this.run) void this.connect(run);
    }, delay);
  }

  /** @param {Hello} hello */
  acceptHello(hello) {
    const epoch = this.hello?.boot_epoch;
    this.hello = hello;
    if (epoch !== hello.boot_epoch) this.samples.clear();
  }

  /**
   * Drops everything held from another engine instance: its event
   * positions and job IDs mean nothing to the new one. Reads still in
   * flight are ignored when they answer.
   */
  forget() {
    this.view += 1;
    this.cursor = null;
    this.page = new Map();
    this.pageIds = [];
    this.pageAfter = null;
    this.nextAfter = null;
    this.detail = null;
    this.watchedState = 'loading';
    this.samples.clear();
    this.counts = null;
    this.changed();
  }

  /**
   * Whether a read issued while `view` was current, and answered as of
   * `asOf`, still describes what the feed follows.
   * @param {number} view
   * @param {EventCursor} asOf
   */
  answers(view, asOf) {
    return view === this.view && asOf.instance === this.hello?.instance;
  }

  /**
   * Takes a consistent snapshot of the page and the inspected job, and
   * sets the cursor to the older of their positions; each copy ignores
   * events it already reflects.
   * @param {number} run
   */
  async snapshot(run) {
    this.cursor = null;
    let page;
    for (let attempt = 1; page === undefined; attempt += 1) {
      // Undefined if the read is no longer wanted, such as when a newer
      // page read answered first; read again.
      page = await this.loadPage(run);
      if (run !== this.run) return;
      if (page === undefined && attempt >= SNAPSHOT_ATTEMPTS) {
        throw new WayfetchError('unavailable', 'no consistent snapshot');
      }
    }
    const detail = await this.loadDetail(run);
    if (run !== this.run || page === undefined) return;
    // Read last, so the counts reflect at least what the cursor does.
    await this.loadCounts(run);
    if (run !== this.run) return;
    const instance = /** @type {Hello} */ (this.hello).instance;
    this.cursor = { instance, position: Math.min(page, detail ?? page) };
  }

  /**
   * Loads the current page; returns the position it reflects, or nothing
   * if the answer is no longer wanted: another page was chosen meanwhile,
   * a later read of it answered first, or the instance changed.
   * @param {number} run
   */
  async loadPage(run) {
    const request = ++this.pageRequests;
    const { pageAfter: after, stages, view } = this;
    for (let attempt = 1; ; attempt += 1) {
      const page = await this.client.listJobs({ after, pageSize: this.pageSize, stages });
      if (run !== this.run || !this.answers(view, page.as_of)) return undefined;
      if (after !== this.pageAfter || stages !== this.stages || request < this.pageShown) {
        return undefined;
      }
      if (this.isBehind(page.as_of) && attempt < SNAPSHOT_ATTEMPTS) continue;
      this.pageShown = request;
      const position = page.as_of.position;
      this.page = new Map(
        page.items.map((job) => [job.id, newer(this.page.get(job.id), job, position)]),
      );
      this.pageIds = page.items.map((job) => job.id);
      this.nextAfter = page.next_after ?? null;
      this.before = page.before ?? null;
      this.prune();
      this.changed();
      return page.as_of.position;
    }
  }

  /**
   * Loads the inspected job; returns the position it reflects, or
   * nothing if there is no such job or it is no longer wanted.
   * @param {number} run
   */
  async loadDetail(run) {
    const job = this.watched;
    const view = this.view;
    if (job === null) return undefined;
    for (let attempt = 1; ; attempt += 1) {
      let snapshot;
      try {
        snapshot = await this.client.inspectJob(job);
      } catch (error) {
        if (/** @type {WayfetchError} */ (error).code !== 'not_found') throw error;
        if (run !== this.run || view !== this.view || job !== this.watched) return undefined;
        this.watchedState = 'missing';
        this.changed();
        return undefined;
      }
      if (run !== this.run || !this.answers(view, snapshot.as_of) || job !== this.watched) {
        return undefined;
      }
      if (this.isBehind(snapshot.as_of) && attempt < SNAPSHOT_ATTEMPTS) continue;
      this.detail = newer(this.detail, snapshot.job, snapshot.as_of.position);
      this.watchedState = 'ready';
      this.prune();
      this.changed();
      return snapshot.as_of.position;
    }
  }

  /**
   * Reads the job counts, unless a later read already answered.
   * @param {number} run
   */
  async loadCounts(run) {
    const view = this.view;
    const counts = await this.client.countJobs();
    if (run !== this.run || !this.answers(view, counts.as_of)) return;
    if (this.counts && counts.as_of.position < this.counts.as_of.position) return;
    this.counts = counts;
    this.changed();
  }

  /**
   * Whether a snapshot at `asOf` misses events the stream already
   * delivered, which would never reach it.
   * @param {EventCursor} asOf
   */
  isBehind(asOf) {
    return this.cursor !== null && asOf.position < this.cursor.position;
  }

  /** @param {number} run */
  subscribe(run) {
    this.subscription = this.client.subscribe(this.cursor, {
      item: (item) => {
        if (run === this.run) this.receive(run, item);
      },
      end: () => {
        if (run !== this.run) return;
        this.subscription = null;
        this.failed(run, new WayfetchError('unavailable', 'the event stream ended'));
      },
    });
  }

  /**
   * @param {number} run
   * @param {StreamItem} item
   */
  receive(run, item) {
    switch (item.type) {
      case 'hello':
        if (item.instance !== this.hello?.instance) {
          // Another engine answers at this address now.
          this.resync(run);
          return;
        }
        this.acceptHello(item);
        this.attempt = 0;
        this.error = null;
        this.setStatus('live');
        this.onEvent(null);
        return;
      case 'event': {
        const cursor = this.cursor;
        if (cursor === null || item.cursor.instance !== cursor.instance) return;
        if (item.cursor.position <= cursor.position) return;
        this.cursor = item.cursor;
        const { kind } = item;
        this.onEvent(kind);
        if (kind.type === 'job_added') this.added(kind.job, item.cursor.position);
        if (kind.type === 'job_added' || kind.type === 'job_changed') {
          const listed = this.page.has(kind.job.id);
          this.hold(kind.job, item.cursor.position);
          // A job that entered or left the filter shifts the page's rows.
          // So may one off this page entering or leaving it before the page.
          if (
            kind.type === 'job_changed' &&
            this.stages !== null &&
            (listed !== this.admits(kind.job) || (!listed && this.pageAfter !== null))
          ) {
            this.repage.request();
          }
          this.recount.request();
        }
        if (kind.type === 'job_removed') {
          // A later job moves up into the page; an earlier one moves the
          // page's place in the list.
          if (this.removed(kind.job) || this.pageAfter !== null) this.repage.request();
          this.recount.request();
        }
        this.changed();
        return;
      }
      case 'progress': {
        if (!this.current(item)) return;
        this.samples.set(item.job, item);
        this.changed();
        return;
      }
      case 'resync':
        this.resync(run);
        return;
    }
  }

  /**
   * Drops the stream and starts again from a new snapshot.
   * @param {number} run
   */
  resync(run) {
    this.subscription?.close();
    this.subscription = null;
    this.cursor = null;
    void this.connect(run);
  }

  /**
   * A new job joins the current page if it is the last page and has room;
   * if it is full, the page gains a next page instead.
   * @param {JobSummary} job
   * @param {number} position
   */
  added(job, position) {
    // New jobs have the highest IDs, so they belong at the end of the
    // last page.
    if (this.page.has(job.id) || this.nextAfter !== null || !this.admits(job)) return;
    const last = this.pageIds.at(-1);
    if (last !== undefined && this.pageIds.length >= this.pageSize) {
      this.nextAfter = last;
      return;
    }
    this.pageIds.push(job.id);
    this.page.set(job.id, { job, position });
  }

  /**
   * Replaces held copies of `job` that are older than `position`.
   * @param {JobSummary} job
   * @param {number} position
   */
  hold(job, position) {
    for (const held of [this.page.get(job.id), this.detail]) {
      if (held && held.job.id === job.id && position > held.position) {
        held.job = job;
        held.position = position;
      }
    }
    // A job that left the listed stages leaves the list.
    const listed = this.page.get(job.id);
    if (listed && !this.admits(listed.job)) {
      this.page.delete(job.id);
      this.pageIds = this.pageIds.filter((id) => id !== job.id);
    }
    this.prune();
  }

  /**
   * Drops a job removed from the list. Returns whether the page held it.
   * @param {JobId} job
   */
  removed(job) {
    this.samples.delete(job);
    if (this.detail?.job.id === job) {
      this.detail = null;
      this.watchedState = 'missing';
    }
    if (!this.page.delete(job)) return false;
    this.pageIds = this.pageIds.filter((id) => id !== job);
    return true;
  }

  /** @param {JobId} job */
  held(job) {
    return this.page.get(job) ?? (this.detail?.job.id === job ? this.detail : undefined);
  }

  /**
   * Whether `sample` belongs to this engine run and to the held copy of
   * its job, which must be transferring under the same generation.
   * @param {ProgressSample} sample
   */
  current(sample) {
    const held = this.held(sample.job);
    return (
      held !== undefined &&
      this.hello !== null &&
      sample.boot_epoch === this.hello.boot_epoch &&
      sample.generation === held.job.generation &&
      held.job.stage === 'transferring'
    );
  }

  /** Forgets samples that no longer match a held job, after any change to
   * the held jobs: an event, or a page or detail read after a resync that
   * missed the job's events. */
  prune() {
    for (const sample of this.samples.values()) {
      if (!this.current(sample)) this.samples.delete(sample.job);
    }
  }
}
