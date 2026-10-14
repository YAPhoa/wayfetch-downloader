// The Downloads workspace: a toolbar to add links and act on the selected
// download, state filters, a table of one page of jobs, an inspector for
// the selected job, and a status bar. It talks to the engine only through
// a `WayfetchClient`, so any transport can host it.
//
// Routes: `/` lists jobs; `/jobs/<id>` also selects one.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { JobFeed } from '../client/feed.js';
import {
  GROUPS,
  availableActions,
  describe,
  explain,
  fileName,
  formatBytes,
  jobName,
  progress,
} from '../client/jobs.js';
import { h, show } from './dom.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').JobSummary} JobSummary
 * @typedef {import('../client/contract.js').JobId} JobId
 * @typedef {import('../client/contract.js').JobAction} JobAction
 * @typedef {import('../client/contract.js').ProgressSample} ProgressSample
 * @typedef {import('../client/feed.js').FeedStatus} FeedStatus
 * @typedef {import('../client/jobs.js').Tone} Tone
 */

/** Jobs per page. */
export const PAGE_SIZE = 50;

/** @type {Record<JobAction, string>} */
const ACTION_LABELS = { pause: 'Pause', resume: 'Resume', cancel: 'Cancel' };

/** @type {Record<FeedStatus, string>} */
const STATUS_LABELS = {
  connecting: 'Connecting…',
  live: 'Live',
  reconnecting: 'Reconnecting…',
  unauthenticated: 'Signed out',
  incompatible: 'Out of date',
  stopped: 'Stopped',
};

/** Where the row density choice is kept, per browser. */
const DENSITY_KEY = 'wayfetch.density';

/**
 * What to tell the user about a refused command.
 * @param {WayfetchError} error
 */
function commandFailed(error) {
  switch (error.code) {
    case 'revision_conflict':
      return 'This download changed in the meantime. Check its state and try again.';
    case 'busy':
      return 'This download is being saved. Try again in a moment.';
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

/**
 * A small stroke icon, hidden from assistive technology.
 * @param {string} path SVG path data
 */
function icon(path) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 14 14');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', 'icon');
  const line = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  line.setAttribute('d', path);
  svg.append(line);
  return svg;
}

/** @type {Record<JobAction | 'add' | 'search' | 'brand', string>} */
const ICONS = {
  brand: 'M7 1.5v7.5M4 6l3 3 3-3M2.5 12h9',
  add: 'M7 2v10M2 7h10',
  pause: 'M4.5 3v8M9.5 3v8',
  resume: 'M4 2.5l7 4.5-7 4.5z',
  cancel: 'M3.5 3.5l7 7M10.5 3.5l-7 7',
  search: 'M6 2.5a3.5 3.5 0 1 0 0 7a3.5 3.5 0 1 0 0-7M8.6 8.6l3 3',
};

/** @param {Tone} tone */
function dot(tone) {
  return h('span', { class: `dot tone-${tone}`, 'aria-hidden': 'true' });
}

/** Reads the saved density; storage may be unavailable. */
function savedDensity() {
  try {
    return localStorage.getItem(DENSITY_KEY) === 'comfortable' ? 'comfortable' : 'compact';
  } catch {
    return 'compact';
  }
}

/** @param {string} density */
function saveDensity(density) {
  try {
    localStorage.setItem(DENSITY_KEY, density);
  } catch {
    // Only a convenience.
  }
}

export class Downloads {
  /**
   * @param {WayfetchClient} client
   * @param {{ onSignedOut: () => void, account?: Node | null }} options
   *   `onSignedOut` is called when the client's credentials stop working;
   *   `account` is shown at the end of the status bar
   */
  constructor(client, { onSignedOut, account = null }) {
    this.client = client;
    this.onSignedOut = onSignedOut;
    this.feed = new JobFeed(client, { pageSize: PAGE_SIZE, onChange: () => this.schedule() });
    /** `after` of each earlier page, for going back. */
    /** @type {(JobId | null)[]} */
    this.earlier = [];
    /** Rendered rows by job, reused while nothing they show changed. */
    /**
     * @type {Map<JobId, {
     *   job: JobSummary,
     *   sample: ProgressSample | undefined,
     *   selected: boolean,
     *   row: HTMLTableRowElement,
     * }>}
     */
    this.rows = new Map();
    this.pending = false;
    /** Between `start` and `stop`; a stopped view never reports or renders. */
    this.active = false;
    /** The state filter's key. */
    this.group = 'all';
    /** Text that names must contain to be shown. */
    this.query = '';
    /** @type {'general' | 'engine'} */
    this.tab = 'general';
    /** The outcome of the last command or an error from a read. */
    this.notice = '';

    /** Connection state, for the status bar. */
    this.status = h('span', { class: 'status', role: 'status', 'data-status': 'connecting' });
    this.speed = h('span', { class: 'speed' });
    this.shown = h('span', { class: 'shown' });
    this.banner = h('div', { class: 'banner', role: 'alert', hidden: true });
    this.noticeLine = h('p', { class: 'notice', role: 'status' });
    this.actionButtons = /** @type {const} */ (['pause', 'resume', 'cancel']).map((action) =>
      h(
        'button',
        {
          type: 'button',
          class: `tool action-${action}`,
          'data-action': action,
          disabled: true,
          onclick: (/** @type {MouseEvent} */ event) =>
            this.command(action, /** @type {HTMLButtonElement} */ (event.currentTarget)),
        },
        icon(ICONS[action]),
        ACTION_LABELS[action],
      ),
    );
    this.addPanel = this.buildAdd();
    this.addToggle = h(
      'button',
      {
        type: 'button',
        class: 'primary',
        'aria-expanded': 'false',
        'aria-controls': 'add-links',
        onclick: () => this.toggleAdd(),
      },
      icon(ICONS.add),
      'Add links',
    );
    this.filters = h('nav', { class: 'filters', 'aria-label': 'Filter by state' });
    this.tableBody = h('tbody');
    this.listStatus = h('p', { class: 'list-status', role: 'status' });
    this.pager = h('nav', { class: 'pager', 'aria-label': 'Pages' });
    this.inspector = h('section', { class: 'inspector', 'aria-label': 'Selected download' });
    this.densityToggle = h(
      'button',
      {
        type: 'button',
        class: 'link',
        onclick: () => this.setDensity(this.density === 'compact' ? 'comfortable' : 'compact'),
      },
      'Comfortable rows',
    );
    this.density = 'compact';
    this.setDensity(savedDensity());

    this.element = h(
      'div',
      { class: 'workspace' },
      this.buildToolbar(),
      this.addPanel,
      this.banner,
      h(
        'div',
        { class: 'body' },
        this.filters,
        h(
          'main',
          { class: 'list' },
          this.noticeLine,
          h('div', { class: 'table-scroll' }, this.buildTable(), this.listStatus),
          this.pager,
          this.inspector,
        ),
      ),
      h(
        'footer',
        { class: 'statusbar' },
        this.status,
        this.speed,
        this.shown,
        h('span', { class: 'spacer' }),
        this.densityToggle,
        account,
      ),
    );
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
    if (location.pathname !== path) history.pushState(null, '', path);
    this.route();
  }

  route() {
    const job = jobFromPath(location.pathname);
    if (job !== this.feed.watched) this.notice = '';
    this.feed.watch(job).catch((error) => this.report(error));
    this.render();
  }

  /** @param {string} density */
  setDensity(density) {
    this.density = density === 'comfortable' ? 'comfortable' : 'compact';
    document.documentElement.dataset.density = this.density;
    this.densityToggle.setAttribute('aria-pressed', String(this.density === 'comfortable'));
    saveDensity(this.density);
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
    show(this.noticeLine, this.notice);
    this.renderBanner();
    this.renderFilters();
    this.renderList();
    this.renderActions();
    this.renderInspector();
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
    this.notice = error instanceof WayfetchError ? error.message : String(error);
    this.schedule();
  }

  // The toolbar and adding links.

  buildToolbar() {
    const search = h('input', {
      type: 'search',
      id: 'filter-names',
      placeholder: 'Filter this page by name',
      autocomplete: 'off',
      spellcheck: 'false',
      oninput: (/** @type {InputEvent} */ event) => {
        this.query = /** @type {HTMLInputElement} */ (event.currentTarget).value.trim().toLowerCase();
        this.schedule();
      },
    });
    return h(
      'header',
      { class: 'toolbar' },
      h('span', { class: 'brand' }, icon(ICONS.brand), 'Wayfetch'),
      this.addToggle,
      h(
        'div',
        { class: 'tool-group', role: 'group', 'aria-label': 'Selected download' },
        this.actionButtons,
      ),
      h(
        'div',
        { class: 'search' },
        icon(ICONS.search),
        h('label', { for: 'filter-names', class: 'visually-hidden' }, 'Filter this page by name'),
        search,
      ),
    );
  }

  /** Shows or hides the add panel, focusing its field when shown. */
  toggleAdd() {
    const open = this.addPanel.hidden;
    this.addPanel.hidden = !open;
    this.addToggle.setAttribute('aria-expanded', String(open));
    if (open) this.addPanel.querySelector('textarea')?.focus();
  }

  buildAdd() {
    const message = h('p', { class: 'message', role: 'status' });
    const urls = h('textarea', {
      name: 'urls',
      rows: 3,
      required: true,
      spellcheck: 'false',
      placeholder: 'https://example.com/file.zip',
      'aria-label': 'Links to download, one per line',
    });
    const add = h('button', { type: 'submit', class: 'primary' }, 'Add');
    const close = h('button', { type: 'button', onclick: () => this.toggleAdd() }, 'Close');
    /** The key of the submission being retried, kept while the text stays the same. */
    let key = { text: '', value: '' };
    return h(
      'form',
      {
        id: 'add-links',
        class: 'add',
        hidden: true,
        onsubmit: async (/** @type {SubmitEvent} */ event) => {
          event.preventDefault();
          const text = urls.value;
          const items = text.split(/\s+/).filter(Boolean);
          const hello = this.feed.hello;
          if (items.length === 0) return;
          if (!hello) {
            show(message, 'Not connected yet. Try again in a moment.');
            return;
          }
          if (items.length > hello.limits.max_submit_items) {
            show(message, `At most ${hello.limits.max_submit_items} links at a time.`);
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
            show(message, `Added ${accepted} ${accepted === 1 ? 'download' : 'downloads'}.`);
          } catch (error) {
            if (error instanceof WayfetchError && error.code === 'unauthenticated') {
              this.signedOut();
              return;
            }
            show(message, error instanceof WayfetchError ? error.message : String(error));
          } finally {
            add.disabled = false;
          }
        },
      },
      urls,
      h('div', { class: 'row' }, add, close, message),
    );
  }

  /** Enables the toolbar's commands the selected job accepts. */
  renderActions() {
    const job = this.feed.watchedJob;
    const allowed = new Set(job ? availableActions(job) : []);
    for (const button of this.actionButtons) {
      const action = /** @type {JobAction} */ (button.dataset.action);
      button.disabled = !allowed.has(action);
      const label = ACTION_LABELS[action];
      button.setAttribute('aria-label', job ? `${label} ${job.id}` : label);
    }
  }

  /**
   * Sends one command for the selected job at the revision shown, then
   * re-reads it.
   * @param {JobAction} action
   * @param {HTMLButtonElement} button
   */
  async command(action, button) {
    const hello = this.feed.hello;
    const job = this.feed.watchedJob;
    if (!hello || !job) return;
    button.disabled = true;
    this.notice = '';
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
      this.notice = `${jobName(job)}: ${commandFailed(error)}`;
    }
    this.schedule();
    await this.feed.refresh(job.id).catch((error) => this.report(error));
  }

  // State filters.

  renderFilters() {
    show(
      this.filters,
      h('span', { class: 'filters-title' }, 'States'),
      GROUPS.map((group) =>
        h(
          'button',
          {
            type: 'button',
            class: 'filter',
            'aria-pressed': String(group.key === this.group),
            onclick: () => this.filter(group.key),
          },
          dot(group.tone),
          group.label,
        ),
      ),
    );
  }

  /** @param {string} key */
  filter(key) {
    if (key === this.group) return;
    const group = GROUPS.find((g) => g.key === key);
    if (!group) return;
    this.group = key;
    this.earlier = [];
    this.notice = '';
    this.feed.showStages(group.stages).catch((error) => this.report(error));
    this.render();
  }

  // The table.

  buildTable() {
    const columns = [
      ['Name', 'name'],
      ['State', 'state'],
      ['Progress', 'progress'],
      ['Size', 'num'],
      ['Speed', 'num'],
      ['Left', 'num'],
    ];
    return h(
      'table',
      { class: 'jobs' },
      h('caption', { class: 'visually-hidden' }, 'Downloads'),
      h(
        'thead',
        null,
        h(
          'tr',
          null,
          columns.map(([title, kind]) => h('th', { scope: 'col', class: kind }, title)),
        ),
      ),
      this.tableBody,
    );
  }

  renderList() {
    const { feed } = this;
    const selected = feed.watched;
    const jobs = feed.jobs.filter(
      (job) => this.query === '' || jobName(job).toLowerCase().includes(this.query),
    );
    const rows = new Map();
    for (const job of jobs) {
      const sample = feed.sample(job.id);
      const isSelected = job.id === selected;
      const old = this.rows.get(job.id);
      const row =
        old && old.job === job && old.sample === sample && old.selected === isSelected
          ? old.row
          : this.row(job, sample, isSelected);
      rows.set(job.id, { job, sample, selected: isSelected, row });
    }
    this.rows = rows;
    show(this.tableBody, [...rows.values()].map(({ row }) => row));

    let empty = '';
    if (feed.status === 'live' && jobs.length === 0) {
      if (feed.jobs.length > 0) empty = 'No downloads on this page match the filter.';
      else if (this.group !== 'all') empty = 'No downloads in this state.';
      else empty = 'No downloads yet. Use Add links to start one.';
    }
    show(this.listStatus, empty);
    show(
      this.pager,
      this.earlier.length > 0 &&
        h('button', { type: 'button', onclick: () => this.page('back') }, 'Previous page'),
      feed.nextAfter !== null &&
        h('button', { type: 'button', onclick: () => this.page('next') }, 'Next page'),
    );

    const rates = feed.jobs.flatMap((job) => feed.sample(job.id)?.bytes_per_second ?? []);
    show(this.speed, rates.length > 0 && `Down ${formatBytes(rates.reduce((a, b) => a + b, 0))}/s`);
    const page = this.earlier.length + 1;
    show(
      this.shown,
      `${feed.jobs.length} ${feed.jobs.length === 1 ? 'download' : 'downloads'} on page ${page}`,
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
    this.notice = '';
    feed.showPage(after).catch((error) => this.report(error));
  }

  /**
   * @param {JobSummary} job
   * @param {ProgressSample | undefined} sample
   * @param {boolean} selected
   */
  row(job, sample, selected) {
    const shown = progress(job, sample);
    const state = describe(job);
    const name = jobName(job);
    const path = `/jobs/${job.id}`;
    return h(
      'tr',
      {
        'data-job': job.id,
        class: selected ? 'selected' : null,
        onclick: (/** @type {MouseEvent} */ event) => {
          if (/** @type {Element} */ (event.target).closest('a')) return;
          this.navigate(path);
        },
      },
      h(
        'td',
        { class: 'name', title: name },
        h(
          'a',
          {
            href: path,
            'aria-current': selected ? 'true' : null,
            onclick: (/** @type {MouseEvent} */ event) => {
              if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey) return;
              event.preventDefault();
              this.navigate(path);
            },
          },
          name,
        ),
      ),
      h(
        'td',
        { class: `state tone-${state.tone}`, 'data-state': job.display },
        dot(state.tone),
        state.label,
      ),
      h(
        'td',
        { class: 'progress' },
        h(
          'span',
          { class: 'bar-cell' },
          h('progress', {
            class: `tone-${state.tone}`,
            max: 1,
            value: shown.fraction ?? 0,
            'aria-label': `${name} progress`,
          }),
          h('span', { class: 'percent' }, shown.percent ?? ''),
        ),
        h('span', { class: 'bytes visually-hidden' }, shown.text),
      ),
      h('td', { class: 'num' }, shown.size ?? 'Unknown'),
      h('td', { class: 'num' }, shown.rate ?? ''),
      h('td', { class: 'num' }, shown.left ?? ''),
    );
  }

  // The inspector.

  renderInspector() {
    const { feed } = this;
    const job = feed.watchedJob;
    if (feed.watched === null) {
      show(this.inspector, h('p', { class: 'empty' }, 'Select a download to see its details.'));
      return;
    }
    if (!job) {
      const text = feed.watchedState === 'missing' ? 'There is no such download.' : 'Loading…';
      show(
        this.inspector,
        this.inspectorHead(text, null),
        h('p', { class: 'empty', role: 'status' }, text),
      );
      return;
    }
    const name = jobName(job);
    show(
      this.inspector,
      this.inspectorHead(name, job),
      this.tab === 'general' ? this.general(job) : this.engine(job),
    );
  }

  /**
   * The inspector's title bar: the job's name, the tabs, and a close button.
   * @param {string} title
   * @param {JobSummary | null} job
   */
  inspectorHead(title, job) {
    /** @param {'general' | 'engine'} tab @param {string} label */
    const tab = (tab, label) =>
      h(
        'button',
        {
          type: 'button',
          role: 'tab',
          id: `tab-${tab}`,
          'aria-selected': String(this.tab === tab),
          'aria-controls': 'inspector-panel',
          tabindex: this.tab === tab ? 0 : -1,
          onclick: () => {
            this.tab = tab;
            this.render();
            this.focusTab();
          },
          onkeydown: (/** @type {KeyboardEvent} */ event) => {
            if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return;
            event.preventDefault();
            this.tab = this.tab === 'general' ? 'engine' : 'general';
            this.render();
            this.focusTab();
          },
        },
        label,
      );
    return h(
      'div',
      { class: 'inspector-head' },
      h('h2', { title }, title),
      job &&
        h(
          'div',
          { role: 'tablist', 'aria-label': 'Download details' },
          tab('general', 'General'),
          tab('engine', 'Engine details'),
        ),
      h(
        'button',
        {
          type: 'button',
          class: 'close',
          'aria-label': 'Close details',
          onclick: () => this.navigate('/'),
        },
        icon(ICONS.cancel),
      ),
    );
  }

  focusTab() {
    const tab = /** @type {HTMLElement | null} */ (this.inspector.querySelector(`#tab-${this.tab}`));
    tab?.focus();
  }

  /**
   * @param {[string, string | number | Node | null | undefined][]} facts
   */
  facts(facts) {
    return h(
      'dl',
      null,
      facts
        .filter(([, value]) => value !== undefined && value !== null && value !== '')
        .flatMap(([term, value]) => [
          h('dt', null, term),
          h('dd', { 'data-fact': term.toLowerCase().replaceAll(' ', '-') }, value),
        ]),
    );
  }

  /** @param {JobSummary} job */
  general(job) {
    const shown = progress(job, this.feed.sample(job.id));
    const state = describe(job);
    const file = fileName(job);
    return h(
      'div',
      {
        class: 'panel general',
        role: 'tabpanel',
        id: 'inspector-panel',
        'aria-labelledby': 'tab-general',
      },
      h(
        'div',
        { class: 'big-progress' },
        h(
          'div',
          { class: 'figure' },
          h('span', { class: 'percent' }, shown.percent ?? '—'),
          h('span', { class: 'saved' }, `${shown.text} saved`),
        ),
        h('progress', {
          class: `tone-${state.tone}`,
          max: 1,
          value: shown.fraction ?? 0,
          'aria-label': 'Saved',
        }),
      ),
      this.facts([
        ['State', h('span', { class: `tone-${state.tone}` }, state.label)],
        ['Speed', shown.rate && [shown.rate, shown.left && `, ${shown.left} left`].join('')],
        ['File', file],
        ['Size', shown.size],
      ]),
      job.failure && h('p', { class: `note tone-${state.tone}` }, explain(job.failure)),
    );
  }

  /** @param {JobSummary} job */
  engine(job) {
    return h(
      'div',
      { class: 'panel', role: 'tabpanel', id: 'inspector-panel', 'aria-labelledby': 'tab-engine' },
      this.facts([
        ['Job', job.id],
        ['Stage', job.stage],
        ['Requested', job.desired],
        ['Package', job.package],
        ['Revision', job.revision],
        ['Generation', job.generation],
        ['Root', job.artifact?.root],
        ['Path', job.artifact?.path],
        ['Failure code', job.failure?.code],
        ['Diagnostic', job.failure?.message],
        ['Next retry', job.failure?.retry_at && new Date(job.failure.retry_at).toLocaleString()],
      ]),
    );
  }
}
