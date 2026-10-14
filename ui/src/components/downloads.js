// The Downloads workflow: add direct links, page through jobs, inspect one,
// and pause, resume, or cancel. It talks to the engine only through a
// `WayfetchClient`, so any transport can host it.
//
// Routes: `/` lists jobs; `/jobs/<id>` shows one job.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { JobFeed } from '../client/feed.js';
import { availableActions, fileName, formatBytes, progress, stateLabel } from '../client/jobs.js';
import { h, show } from './dom.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').JobSummary} JobSummary
 * @typedef {import('../client/contract.js').JobId} JobId
 * @typedef {import('../client/contract.js').JobAction} JobAction
 * @typedef {import('../client/contract.js').ProgressSample} ProgressSample
 * @typedef {import('../client/feed.js').FeedStatus} FeedStatus
 */

/** Jobs per page. */
export const PAGE_SIZE = 50;

const ACTION_LABELS = { pause: 'Pause', resume: 'Resume', cancel: 'Cancel' };

const STATUS_LABELS = {
  connecting: 'Connecting…',
  live: 'Live',
  reconnecting: 'Reconnecting…',
  unauthenticated: 'Signed out',
  incompatible: 'Out of date',
  stopped: 'Stopped',
};

/**
 * What to tell the user about a refused command.
 * @param {WayfetchError} error
 */
function commandFailed(error) {
  switch (error.code) {
    case 'revision_conflict':
      return 'This download changed in the meantime. Check its state and try again.';
    case 'busy':
      return 'This download is being published. Try again in a moment.';
    case 'not_applicable':
      return 'That does not apply to this download any more.';
    case 'unavailable':
      return 'The server cannot be reached. Downloads continue on the server; try again shortly.';
    default:
      return error.message;
  }
}

/** @param {string} path */
function jobFromPath(path) {
  const match = /^\/jobs\/(job-[1-9][0-9]*)$/.exec(path);
  return match ? /** @type {JobId} */ (match[1]) : null;
}

export class Downloads {
  /**
   * @param {WayfetchClient} client
   * @param {{ onSignedOut: () => void }} options called when the client's
   *   credentials stop working
   */
  constructor(client, { onSignedOut }) {
    this.client = client;
    this.onSignedOut = onSignedOut;
    this.feed = new JobFeed(client, { pageSize: PAGE_SIZE, onChange: () => this.schedule() });
    /** `after` of each earlier page, for going back. */
    /** @type {(JobId | null)[]} */
    this.earlier = [];
    /** Rendered rows by job, reused while their job and sample are unchanged. */
    /** @type {Map<JobId, { job: JobSummary, sample: ProgressSample | undefined, row: HTMLTableRowElement }>} */
    this.rows = new Map();
    this.pending = false;
    /** Between `start` and `stop`; a stopped view never reports or renders. */
    this.active = false;

    /** Connection state, for the shell's header. */
    this.status = h('span', { class: 'status', role: 'status', 'data-status': 'connecting' });
    this.banner = h('div', { class: 'banner', role: 'alert', hidden: true });
    this.addMessage = h('p', { class: 'message', role: 'status' });
    this.tableBody = h('tbody');
    this.listStatus = h('p', { class: 'message', role: 'status' });
    this.pager = h('nav', { class: 'pager', 'aria-label': 'Pages' });
    /** An error to show with the list. */
    this.listMessage = '';
    /** The outcome of the last command on the job page. */
    this.detailMessage = '';
    this.listView = this.buildList();
    this.detailView = h('section', { class: 'card', hidden: true });
    this.element = h('main', null, this.banner, this.listView, this.detailView);
    this.onPopState = () => this.route();
  }

  /** Starts following the engine and shows the current route. */
  start() {
    this.active = true;
    window.addEventListener('popstate', this.onPopState);
    this.feed.start();
    this.route();
  }

  /** Stops following the engine; downloads continue. */
  stop() {
    this.active = false;
    window.removeEventListener('popstate', this.onPopState);
    this.feed.stop();
  }

  /** @param {string} path */
  navigate(path) {
    history.pushState(null, '', path);
    this.route();
  }

  route() {
    const job = jobFromPath(location.pathname);
    this.listView.hidden = job !== null;
    this.detailView.hidden = job === null;
    this.feed.watch(job).catch((error) => this.report(error));
    if (job !== null) this.detailMessage = '';
    this.render();
  }

  /** Renders once per frame, however many changes arrive. */
  schedule() {
    if (this.pending) return;
    this.pending = true;
    requestAnimationFrame(() => {
      this.pending = false;
      this.render();
    });
  }

  render() {
    const { feed } = this;
    if (!this.active) return;
    if (feed.status === 'unauthenticated') {
      this.signedOut();
      return;
    }
    this.status.dataset.status = feed.status;
    show(this.status, STATUS_LABELS[feed.status]);
    this.renderBanner();
    if (this.listView.hidden) this.renderDetail();
    else this.renderList();
  }

  renderBanner() {
    const { feed } = this;
    let text = null;
    if (feed.status === 'incompatible') {
      text = 'This page does not match the server’s version. Reload the page.';
    } else if (feed.status === 'reconnecting') {
      text = 'The server cannot be reached. Downloads continue on the server; this page reconnects on its own.';
    }
    this.banner.hidden = text === null;
    show(this.banner, text);
  }

  /** Tells the shell, once, that the session is gone. */
  signedOut() {
    if (!this.active) return;
    this.stop();
    this.onSignedOut();
  }

  /**
   * Reports an error from a load; an ended session goes to the shell.
   * @param {unknown} error
   */
  report(error) {
    if (error instanceof WayfetchError && error.code === 'unauthenticated') {
      this.signedOut();
      return;
    }
    this.listMessage = error instanceof WayfetchError ? error.message : String(error);
    this.schedule();
  }

  // The list.

  buildList() {
    const urls = h('textarea', {
      name: 'urls',
      rows: 3,
      required: true,
      spellcheck: 'false',
      placeholder: 'https://example.com/file.zip',
      'aria-label': 'Links to download, one per line',
    });
    const add = h('button', { type: 'submit' }, 'Add');
    /** The key of the submission being retried, kept while the text stays the same. */
    let key = { text: '', value: '' };
    const form = h(
      'form',
      {
        class: 'card add',
        onsubmit: async (/** @type {SubmitEvent} */ event) => {
          event.preventDefault();
          const text = urls.value;
          const items = text.split(/\s+/).filter(Boolean);
          const hello = this.feed.hello;
          if (items.length === 0) return;
          if (!hello) {
            show(this.addMessage, 'Not connected yet. Try again in a moment.');
            return;
          }
          if (items.length > hello.limits.max_submit_items) {
            show(this.addMessage, `At most ${hello.limits.max_submit_items} links at a time.`);
            return;
          }
          if (key.text !== text) key = { text, value: idempotencyKey() };
          add.disabled = true;
          try {
            const result = await this.client.submit({
              idempotencyKey: key.value,
              target: hello.instance,
              items,
            });
            const accepted = result.type === 'submitted' ? result.accepted : items.length;
            urls.value = '';
            key = { text: '', value: '' };
            show(this.addMessage, `Added ${accepted} ${accepted === 1 ? 'download' : 'downloads'}.`);
          } catch (error) {
            if (error instanceof WayfetchError && error.code === 'unauthenticated') {
              this.signedOut();
              return;
            }
            show(this.addMessage, error instanceof WayfetchError ? error.message : String(error));
          } finally {
            add.disabled = false;
          }
        },
      },
      h('h2', null, 'Add downloads'),
      urls,
      h('div', { class: 'row' }, add, this.addMessage),
    );
    return h(
      'div',
      null,
      form,
      h(
        'section',
        { class: 'card' },
        h('h2', null, 'Downloads'),
        h(
          'table',
          null,
          h(
            'thead',
            null,
            h(
              'tr',
              null,
              ['Job', 'State', 'Progress', 'File', 'Problem', ''].map((title) =>
                h('th', { scope: 'col' }, title),
              ),
            ),
          ),
          this.tableBody,
        ),
        this.listStatus,
        this.pager,
      ),
    );
  }

  renderList() {
    const { feed } = this;
    const jobs = feed.jobs;
    const rows = new Map();
    for (const job of jobs) {
      const sample = feed.sample(job.id);
      const old = this.rows.get(job.id);
      const row = old && old.job === job && old.sample === sample ? old.row : this.row(job, sample);
      rows.set(job.id, { job, sample, row });
    }
    this.rows = rows;
    show(this.tableBody, [...rows.values()].map(({ row }) => row));
    const empty = jobs.length === 0 && feed.status === 'live' ? 'No downloads yet.' : '';
    show(this.listStatus, this.listMessage || empty);
    show(
      this.pager,
      this.earlier.length > 0 &&
        h('button', { type: 'button', onclick: () => this.page('back') }, 'Previous page'),
      feed.nextAfter !== null &&
        h('button', { type: 'button', onclick: () => this.page('next') }, 'Next page'),
    );
  }

  /** @param {'next' | 'back'} direction */
  page(direction) {
    const { feed } = this;
    let after;
    if (direction === 'next') {
      this.earlier.push(feed.pageAfter);
      after = feed.nextAfter;
    } else {
      after = this.earlier.pop() ?? null;
    }
    this.listMessage = '';
    feed.showPage(after).catch((error) => this.report(error));
  }

  /**
   * @param {JobSummary} job
   * @param {ProgressSample | undefined} sample
   */
  row(job, sample) {
    const shown = progress(job, sample);
    return h(
      'tr',
      { 'data-job': job.id },
      h('td', null, this.link(job.id)),
      h('td', { 'data-state': job.display }, stateLabel(job.display)),
      h(
        'td',
        null,
        shown.fraction !== undefined &&
          h('progress', { max: 1, value: shown.fraction, 'aria-label': `${job.id} progress` }),
        h('span', { class: 'bytes' }, shown.text),
        shown.rate && h('span', { class: 'rate' }, shown.rate),
      ),
      h('td', { class: 'file' }, fileName(job) ?? ''),
      h('td', null, job.failure ? job.failure.code : ''),
      h('td', { class: 'actions' }, this.actions(job, (text) => {
        this.listMessage = text;
        this.schedule();
      })),
    );
  }

  /** @param {JobId} job */
  link(job) {
    return h(
      'a',
      {
        href: `/jobs/${job}`,
        onclick: (/** @type {MouseEvent} */ event) => {
          if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey) return;
          event.preventDefault();
          this.navigate(`/jobs/${job}`);
        },
      },
      job,
    );
  }

  /**
   * Buttons for the commands `job` accepts.
   * @param {JobSummary} job
   * @param {(message: string) => void} say
   */
  actions(job, say) {
    return availableActions(job).map((action) =>
      h(
        'button',
        {
          type: 'button',
          class: action === 'cancel' ? 'secondary' : null,
          'data-action': action,
          'aria-label': `${ACTION_LABELS[action]} ${job.id}`,
          onclick: (/** @type {MouseEvent} */ event) =>
            this.command(job, action, /** @type {HTMLButtonElement} */ (event.currentTarget), say),
        },
        ACTION_LABELS[action],
      ),
    );
  }

  /**
   * Sends one command at the revision shown, then re-reads the job.
   * @param {JobSummary} job
   * @param {JobAction} action
   * @param {HTMLButtonElement} button
   * @param {(message: string) => void} say
   */
  async command(job, action, button, say) {
    const hello = this.feed.hello;
    if (!hello) return;
    button.disabled = true;
    say('');
    try {
      await this.client.jobCommand(job.id, {
        idempotencyKey: idempotencyKey(),
        target: hello.instance,
        action,
        expectedRevision: job.revision,
      });
    } catch (error) {
      if (!(error instanceof WayfetchError)) throw error;
      if (error.code === 'unauthenticated') {
        this.signedOut();
        return;
      }
      say(`${job.id}: ${commandFailed(error)}`);
    } finally {
      button.disabled = false;
    }
    await this.feed.refresh(job.id).catch((error) => this.report(error));
  }

  // One job.

  renderDetail() {
    const { feed } = this;
    const job = feed.watchedJob;
    const back = h(
      'a',
      {
        href: '/',
        onclick: (/** @type {MouseEvent} */ event) => {
          event.preventDefault();
          this.navigate('/');
        },
      },
      '← All downloads',
    );
    if (!job) {
      const text = feed.watchedState === 'missing' ? 'There is no such download.' : 'Loading…';
      show(this.detailView, back, h('p', { class: 'message', role: 'status' }, text));
      return;
    }
    const shown = progress(job, feed.sample(job.id));
    /** @type {[string, string | number | Node | null | undefined][]} */
    const facts = [
      ['State', stateLabel(job.display)],
      ['Requested', job.desired],
      ['Progress', shown.text],
      ['Rate', shown.rate],
      ['File', job.artifact?.path],
      ['Size', job.artifact && formatBytes(job.artifact.size)],
      ['Root', job.artifact?.root],
      ['Problem', job.failure?.code],
      ['Details', job.failure?.message],
      ['Next retry', job.failure?.retry_at && new Date(job.failure.retry_at).toLocaleString()],
      ['Package', job.package],
      ['Revision', job.revision],
      ['Generation', job.generation],
    ];
    show(
      this.detailView,
      back,
      h('h2', null, job.id),
      shown.fraction !== undefined && h('progress', { max: 1, value: shown.fraction }),
      h(
        'dl',
        null,
        facts
          .filter(([, value]) => value !== undefined && value !== null && value !== '')
          .flatMap(([term, value]) => [
            h('dt', null, term),
            h('dd', { 'data-fact': term.toLowerCase() }, value),
          ]),
      ),
      h('p', { class: 'message', role: 'status' }, this.detailMessage),
      h(
        'div',
        { class: 'row actions' },
        this.actions(job, (text) => {
          this.detailMessage = text;
          this.schedule();
        }),
      ),
    );
  }
}
