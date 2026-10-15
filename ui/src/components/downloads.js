// The Downloads workspace: a toolbar to add links, act on the selected
// download, and open Settings; state filters; a table of one page of jobs,
// each with a menu of what can be done with it; an inspector for the
// selected job; and a status bar. It talks to the engine only through a
// `WayfetchClient`, so any transport can host it.
//
// Routes: `/` lists jobs; `/jobs/<id>` also selects one.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { JobFeed } from '../client/feed.js';
import {
  GROUPS,
  availableActions,
  canRemove,
  canRename,
  describe,
  explain,
  fileName,
  formatBytes,
  groupCount,
  jobName,
  progress,
} from '../client/jobs.js';
import { openDialog } from './dialog.js';
import { focusedKey, h, refocus, show } from './dom.js';
import { Menu } from './menu.js';
import { openSettings } from './settings.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').JobSummary} JobSummary
 * @typedef {import('../client/contract.js').JobId} JobId
 * @typedef {import('../client/contract.js').StateAction} StateAction
 * @typedef {import('../client/contract.js').ProgressSample} ProgressSample
 * @typedef {import('../client/contract.js').Settings} Settings
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {import('./menu.js').MenuEntry} MenuEntry
 * @typedef {import('./settings.js').Density} Density
 * @typedef {import('../client/feed.js').FeedStatus} FeedStatus
 * @typedef {import('../client/jobs.js').Tone} Tone
 */

/** Jobs per page. */
export const PAGE_SIZE = 50;

/** @type {Record<StateAction, string>} */
const ACTION_LABELS = { pause: 'Pause', resume: 'Resume', cancel: 'Cancel' };

/** Why a menu item does not apply, when the job's state is the reason. */
const NOT_FINISHED = 'Once it has finished';
const NOT_ENDED = 'Once it has finished, failed, or been cancelled';
const NO_CONTROL = 'You may only view downloads';

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
      return error.message || 'That does not apply to this download any more.';
    case 'already_exists':
      return 'A file with that name is already in the folder. Choose another name.';
    case 'invalid_request':
      return `That name cannot be used: ${error.message}`;
    case 'permission_denied':
      return 'You may not do that.';
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

/** @type {Record<StateAction | 'add' | 'search' | 'brand' | 'settings' | 'more', string>} */
const ICONS = {
  settings:
    'M7 4.75a2.25 2.25 0 1 0 0 4.5a2.25 2.25 0 1 0 0-4.5M7 1v1.5M7 11.5V13M1 7h1.5M11.5 7H13M2.8 2.8l1 1M10.2 10.2l1 1M2.8 11.2l1-1M10.2 3.8l1-1',
  more: 'M3 7h.01M7 7h.01M11 7h.01',
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

/**
 * A problem's text while a file name is typed, or `null` if the engine
 * should be asked; it has the final say.
 * @param {string} name
 */
function nameProblem(name) {
  if (name === '') return 'Enter a name.';
  if (name === '.' || name === '..') return 'That name cannot be used.';
  if (/[/\\]/.test(name)) return 'A name cannot contain / or \\.';
  return null;
}

/**
 * Copies `text` to the clipboard: through the Clipboard API where the page
 * may use it, else through a selected, hidden field.
 * @param {string} text
 */
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // Not a secure context, or not allowed; fall back below.
  }
  const field = h('textarea', { class: 'visually-hidden', readonly: true, 'aria-hidden': 'true' });
  field.value = text;
  document.body.append(field);
  field.select();
  const copied = document.execCommand('copy');
  field.remove();
  if (!copied) throw new Error('The clipboard is not available.');
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
    /** The last notice that reported a success, shown as one. */
    this.succeeded = '';
    /** The engine's settings, once read: where new downloads go. */
    /** @type {Settings | null} */
    this.settings = null;
    /** The menu of commands for one row. */
    this.menu = new Menu('Download actions', () => this.schedule());

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
    this.destinationLine = h('p', { class: 'destination', 'data-key': 'add-destination' });
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
    this.settingsButton = h(
      'button',
      {
        type: 'button',
        class: 'tool settings-button',
        'data-key': 'open-settings',
        'aria-haspopup': 'dialog',
        onclick: () => this.openSettings(),
      },
      icon(ICONS.settings),
      'Settings',
    );
    /** @type {Density} */
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
        account,
      ),
      this.menu.element,
    );
    this.onPopState = () => this.route();
  }

  /** Starts following the engine and shows the current route. */
  start() {
    this.active = true;
    window.addEventListener('popstate', this.onPopState);
    this.feed.start();
    this.route();
    this.loadSettings();
  }

  /** Reads where new downloads go, for the add panel and the inspector. */
  loadSettings() {
    this.client.settings().then(
      (settings) => {
        this.settings = settings;
        this.schedule();
      },
      (error) => this.report(error),
    );
  }

  /** Opens the Settings dialog; focus comes back to its button. */
  openSettings() {
    this.menu.close(false);
    openSettings({
      client: this.client,
      hello: () => this.feed.hello,
      density: () => this.density,
      setDensity: (density) => this.setDensity(density),
      onSettings: (settings) => {
        this.settings = settings;
        this.schedule();
      },
      onSignedOut: () => this.signedOut(),
    });
  }

  /**
   * Where root `id` is, for display; a root no longer permitted is named
   * by its ID.
   * @param {RootId} id
   */
  rootPath(id) {
    return this.settings?.roots.find((r) => r.id === id)?.path ?? `folder ${id}, no longer permitted`;
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
    // Parts are rebuilt below; keyboard focus stays on the same control.
    const focused = focusedKey(this.element);
    this.status.dataset.status = feed.status;
    show(this.status, STATUS_LABELS[feed.status]);
    show(this.noticeLine, this.notice);
    this.noticeLine.classList.toggle('ok', this.notice !== '' && this.notice === this.succeeded);
    this.renderBanner();
    this.renderDestination();
    this.renderFilters();
    this.renderList();
    this.renderActions();
    this.renderInspector();
    refocus(this.element, focused);
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

  /** Says where new downloads go, next to the add form. */
  renderDestination() {
    const settings = this.settings;
    const root = settings?.roots.find((r) => r.id === settings.default_root);
    show(
      this.destinationLine,
      h('span', { class: 'label' }, 'Saving to'),
      root
        ? h('span', { class: 'path', title: root.path }, root.path)
        : h('span', { class: 'path unknown' }, settings ? 'the download folder' : 'Loading…'),
      root && !this.client.local && h('span', { class: 'tag' }, 'Server path'),
      root && !root.available && h('span', { class: 'problem' }, root.problem ?? 'Unavailable'),
      h(
        'button',
        { type: 'button', class: 'link', 'data-key': 'change-destination', onclick: () => this.openSettings() },
        'Change…',
      ),
    );
  }

  /** Tells the shell, once, that the session is gone. */
  signedOut() {
    if (!this.active) return;
    this.menu.close(false);
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
      this.settingsButton,
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
      this.destinationLine,
      h('div', { class: 'row' }, add, close, message),
    );
  }

  /** Whether this client may change downloads. */
  get canControl() {
    return this.client.permissions.includes('control');
  }

  /** Enables the toolbar's commands the selected job accepts. */
  renderActions() {
    const job = this.feed.watchedJob;
    const allowed = new Set(job && this.canControl ? availableActions(job) : []);
    for (const button of this.actionButtons) {
      const action = /** @type {StateAction} */ (button.dataset.action);
      button.disabled = !allowed.has(action);
      const label = ACTION_LABELS[action];
      button.setAttribute('aria-label', job ? `${label} ${job.id}` : label);
    }
  }

  /**
   * Sends a state command for the selected job from the toolbar.
   * @param {StateAction} action
   * @param {HTMLButtonElement} button
   */
  async command(action, button) {
    const job = this.feed.watchedJob;
    if (!job) return;
    button.disabled = true;
    await this.send(job, action);
  }

  /**
   * Sends one command for `job` at the revision shown, then re-reads it.
   * Resolves to whether the engine accepted it; a refusal is shown in the
   * notice line, or passed to `refused` if given.
   * @param {JobSummary} job
   * @param {StateAction | 'rename' | 'remove'} action
   * @param {{ name?: string, refused?: (text: string) => void }} [options]
   */
  async send(job, action, { name, refused } = {}) {
    const hello = this.feed.hello;
    if (!hello) {
      (refused ?? ((text) => (this.notice = text)))('Not connected yet. Try again in a moment.');
      this.schedule();
      return false;
    }
    this.notice = '';
    let accepted = false;
    try {
      await this.client.jobCommand(job.id, {
        idempotencyKey: idempotencyKey(),
        target: hello.instance,
        action,
        expectedRevision: job.revision,
        name,
      });
      accepted = true;
    } catch (error) {
      if (!(error instanceof WayfetchError)) throw error;
      if (error.code === 'unauthenticated') {
        this.signedOut();
        return false;
      }
      const text = commandFailed(error);
      if (refused) refused(text);
      else this.notice = `${jobName(job)}: ${text}`;
    }
    this.schedule();
    if (action !== 'remove' || !accepted) {
      await this.feed.refresh(job.id).catch((error) => this.report(error));
    }
    return accepted;
  }

  // The row menu.

  /**
   * Opens the menu for `job` at `anchor`. Focus returns to the row's menu
   * button, which a redraw may have replaced.
   * @param {JobSummary} job
   * @param {import('./menu.js').Anchor} anchor
   */
  openMenu(job, anchor) {
    const restore = () => {
      const button = /** @type {HTMLElement | null} */ (
        this.tableBody.querySelector(`[data-key="menu-${job.id}"]`)
      );
      (button ?? this.tableBody.querySelector(`[data-key="open-${job.id}"]`))?.focus?.();
    };
    this.menu.show(this.menuEntries(job), anchor, { owner: job.id, restore });
  }

  /**
   * What can be done with `job`: everything this client and host support,
   * with those its state or permissions rule out disabled and explained.
   * @param {JobSummary} job
   * @returns {MenuEntry[]}
   */
  menuEntries(job) {
    const { client } = this;
    const control = this.canControl;
    const finished = job.stage === 'complete';
    const actions = new Set(control ? availableActions(job) : []);
    /** @type {MenuEntry[]} */
    const entries = [];
    const { openFile, revealFile } = client;
    if (openFile && revealFile) {
      entries.push(
        {
          key: 'open-file',
          label: 'Open file',
          disabled: !finished,
          hint: finished ? undefined : NOT_FINISHED,
          run: () => void this.local(job, () => openFile.call(client, job.id)),
        },
        {
          key: 'reveal-file',
          label: 'Open containing folder',
          disabled: !finished,
          hint: finished ? undefined : NOT_FINISHED,
          run: () => void this.local(job, () => revealFile.call(client, job.id)),
        },
        'separator',
      );
    }
    entries.push({
      key: 'rename',
      label: 'Rename…',
      disabled: !control || !canRename(job),
      hint: !control ? NO_CONTROL : canRename(job) ? undefined : NOT_FINISHED,
      run: () => this.rename(job),
    });
    if (client.permissions.includes('sources')) {
      entries.push({ key: 'copy-source', label: 'Copy source link', run: () => void this.copySource(job) });
    }
    entries.push('separator');
    const toggle = actions.has('resume') ? 'resume' : 'pause';
    entries.push(
      {
        key: toggle,
        label: ACTION_LABELS[toggle],
        disabled: !actions.has(toggle),
        hint: !control ? NO_CONTROL : undefined,
        run: () => void this.send(job, toggle),
      },
      {
        key: 'cancel',
        label: 'Cancel download',
        disabled: !actions.has('cancel'),
        hint: !control ? NO_CONTROL : undefined,
        run: () => void this.send(job, 'cancel'),
      },
      {
        key: 'remove',
        label: 'Remove from list',
        disabled: !control || !canRemove(job),
        hint: !control ? NO_CONTROL : canRemove(job) ? 'Keeps the file' : NOT_ENDED,
        danger: true,
        run: () => void this.remove(job),
      },
    );
    return entries;
  }

  /**
   * Says that something succeeded.
   * @param {string} text
   */
  done(text) {
    this.notice = text;
    this.succeeded = text;
  }

  /**
   * Runs a request to the local host about `job`'s file, reporting a
   * refusal.
   * @param {JobSummary} job
   * @param {() => Promise<void>} request
   */
  async local(job, request) {
    this.notice = '';
    try {
      await request();
    } catch (error) {
      this.notice = `${jobName(job)}: ${error instanceof WayfetchError ? commandFailed(error) : String(error)}`;
    }
    this.schedule();
  }

  /** @param {JobSummary} job */
  async copySource(job) {
    try {
      await copyText(await this.client.jobSource(job.id));
      this.done(`Copied the link of ${jobName(job)}.`);
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        this.signedOut();
        return;
      }
      this.notice = `${jobName(job)}: the link could not be copied. ${
        error instanceof WayfetchError ? commandFailed(error) : String(/** @type {Error} */ (error).message)
      }`;
    }
    this.schedule();
  }

  /**
   * Hides an ended job from lists; its file stays in its folder.
   * @param {JobSummary} job
   */
  async remove(job) {
    const name = jobName(job);
    if (!(await this.send(job, 'remove'))) return;
    // Closing its details clears the notice, so this comes after.
    if (this.feed.watched === job.id) this.navigate('/');
    this.done(`Removed ${name} from the list. Its file stays in its folder.`);
    this.schedule();
  }

  /**
   * Asks for a new name for a finished job's file, in its folder.
   * @param {JobSummary} job
   */
  rename(job) {
    const current = fileName(job) ?? '';
    const message = h('p', { class: 'message', role: 'alert' });
    const field = h('input', {
      type: 'text',
      id: 'rename-name',
      name: 'name',
      value: current,
      required: true,
      autocomplete: 'off',
      spellcheck: 'false',
    });
    const save = h('button', { type: 'submit', class: 'primary' }, 'Rename');
    const cancel = h('button', { type: 'button', onclick: () => dialog.close() }, 'Cancel');
    const form = h(
      'form',
      {
        class: 'rename-form',
        onsubmit: async (/** @type {SubmitEvent} */ event) => {
          event.preventDefault();
          const name = field.value.trim();
          const problem = nameProblem(name);
          if (problem) {
            show(message, problem);
            return;
          }
          if (name === current) {
            dialog.close();
            return;
          }
          save.disabled = true;
          show(message, '');
          const done = await this.send(job, 'rename', { name, refused: (text) => show(message, text) });
          save.disabled = false;
          if (done) {
            dialog.close();
            this.done(`Renamed ${current} to ${name}.`);
            this.schedule();
          } else {
            field.focus();
          }
        },
      },
      h('label', { for: 'rename-name' }, 'New file name'),
      field,
      h('p', { class: 'hint' }, 'The file stays in its folder. An existing file is never replaced.'),
      message,
      h('div', { class: 'row' }, save, cancel),
    );
    const dialog = openDialog(
      {
        title: `Rename ${current}`,
        className: 'rename',
        closeLabel: 'Close without renaming',
        restore: () =>
          /** @type {HTMLElement | null} */ (
            this.tableBody.querySelector(`[data-key="menu-${job.id}"]`)
          )?.focus(),
      },
      form,
    );
    // The name without its extension, ready to replace.
    const dot = current.lastIndexOf('.');
    field.focus();
    field.setSelectionRange(0, dot > 0 ? dot : current.length);
  }

  // State filters.

  renderFilters() {
    const counts = this.feed.counts?.stages;
    show(
      this.filters,
      h('span', { class: 'filters-title' }, 'States'),
      GROUPS.map((group) => {
        const count = counts && groupCount(group, counts);
        return h(
          'button',
          {
            type: 'button',
            class: 'filter',
            'data-key': `filter-${group.key}`,
            // Read as "Done (3)", not "Done3".
            'aria-label': count === undefined ? null : `${group.label} (${count})`,
            'aria-pressed': String(group.key === this.group),
            onclick: () => this.filter(group.key),
          },
          dot(group.tone),
          h('span', { class: 'filter-label' }, group.label),
          count !== undefined && h('span', { class: 'count' }, count),
        );
      }),
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
      ['Host', 'host'],
      ['State', 'state'],
      ['Progress', 'progress'],
      ['Size', 'num'],
      ['Speed', 'num'],
      ['Left', 'num'],
    ];
    const actions = h('th', { scope: 'col', class: 'row-actions' }, h('span', { class: 'visually-hidden' }, 'Actions'));
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
          actions,
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
        old &&
        old.job === job &&
        old.sample === sample &&
        old.selected === isSelected
          ? old.row
          : this.row(job, sample, isSelected);
      // Kept up to date in place, so opening a menu redraws no row.
      const menuOpen = this.menu.owner === job.id;
      row.classList.toggle('menu-open', menuOpen);
      row.querySelector('button.more')?.setAttribute('aria-expanded', String(menuOpen));
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
        h(
          'button',
          { type: 'button', 'data-key': 'page-back', onclick: () => this.page('back') },
          'Previous page',
        ),
      feed.nextAfter !== null &&
        h(
          'button',
          { type: 'button', 'data-key': 'page-next', onclick: () => this.page('next') },
          'Next page',
        ),
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
    const more = h(
      'button',
      {
        type: 'button',
        class: 'more',
        'data-key': `menu-${job.id}`,
        'aria-label': `Actions for ${name}`,
        'aria-haspopup': 'menu',
        onclick: (/** @type {MouseEvent} */ event) => {
          const button = /** @type {HTMLElement} */ (event.currentTarget);
          if (this.menu.justDismissed(job.id, button)) return;
          if (this.menu.owner === job.id) {
            this.menu.close(true);
            return;
          }
          this.openMenu(job, button.getBoundingClientRect());
        },
      },
      icon(ICONS.more),
    );
    return h(
      'tr',
      {
        'data-job': job.id,
        class: selected ? 'selected' : null,
        onclick: (/** @type {MouseEvent} */ event) => {
          if (/** @type {Element} */ (event.target).closest('a, button')) return;
          this.navigate(path);
        },
        // The row pressed, wherever in it; the browser's own menu otherwise.
        oncontextmenu: (/** @type {MouseEvent} */ event) => {
          event.preventDefault();
          // A context-menu key press has no pointer position.
          const anchor =
            event.button === 0 && event.clientX === 0 && event.clientY === 0
              ? more.getBoundingClientRect()
              : { left: event.clientX, right: event.clientX, top: event.clientY, bottom: event.clientY };
          this.openMenu(job, anchor);
        },
        onkeydown: (/** @type {KeyboardEvent} */ event) => {
          if (!(event.key === 'ContextMenu' || (event.key === 'F10' && event.shiftKey))) return;
          event.preventDefault();
          this.openMenu(job, more.getBoundingClientRect());
        },
      },
      h(
        'td',
        { class: 'name', title: name },
        h(
          'a',
          {
            href: path,
            'data-key': `open-${job.id}`,
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
      h('td', { class: 'host', title: job.original_host ?? null }, job.original_host ?? ''),
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
      h('td', { class: 'row-actions' }, more),
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
          'data-key': `tab-${tab}`,
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
          'data-key': 'close-details',
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
    const planned = job.destination ?? job.artifact?.root;
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
        ['Saves to', planned === undefined || planned === null ? 'The download folder' : this.rootPath(planned)],
        ['Saved as', job.artifact && this.savedAs(job.artifact)],
        ['Host', job.original_host],
        ['Size', shown.size],
      ]),
      job.failure && h('p', { class: `note tone-${state.tone}` }, explain(job.failure)),
    );
  }

  /**
   * The published file's full path, labelled as a server path in a
   * browser.
   * @param {import('../client/generated/ArtifactSummary.js').ArtifactSummary} artifact
   */
  savedAs(artifact) {
    const root = this.settings?.roots.find((r) => r.id === artifact.root)?.path;
    const separator = root?.includes('\\') && !root.includes('/') ? '\\' : '/';
    const full =
      root === undefined
        ? `${artifact.path} in ${this.rootPath(artifact.root)}`
        : `${root.replace(/[\\/]+$/, '')}${separator}${artifact.path}`;
    return h(
      'span',
      { class: 'saved-as' },
      h('span', { class: 'path', title: full }, full),
      !this.client.local && h('span', { class: 'tag' }, 'Server path'),
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
