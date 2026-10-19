// The Downloads workspace: a toolbar to add links, switch between the
// Downloads and Links views, act on the ticked downloads (or else the one
// in the details), and open Settings; state filters; a table of one page
// of jobs, each with a box to tick and a menu of what can be done with it
// (with the ticked ones, when it is one of them); an inspector for the
// selected job; and a status bar. It talks to the engine only through a
// `WayfetchClient`, so any transport can host it.
//
// Adding links, on a page of its own, collects them on the Links page
// for review; nothing downloads until they are started there.
//
// Routes: `/` lists jobs; `/jobs/<id>` also selects one; `/links` shows
// the collected links; `/add` the Add links page; `/settings` and its
// sections the Settings page. Add links and Settings take the place of
// either view, while the workspace keeps following the engine.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { JobFeed } from '../client/feed.js';
import {
  GROUPS,
  availableActions,
  canRemove,
  canRename,
  describe,
  displayLink,
  explain,
  fileKind,
  fileName,
  formatBytes,
  groupCount,
  jobName,
  progress,
  restartNote,
  retryPlan,
} from '../client/jobs.js';
import { count, refusals } from '../client/links.js';
import { routeLabel } from '../client/routes.js';
import { Selection } from '../client/selection.js';
import { ADD_PATH, AddLinksPage, collectedText } from './collect.js';
import { openDialog } from './dialog.js';
import { copyText, focusedKey, h, refocus, show } from './dom.js';
import { icon } from './icons.js';
import { LINKS_PATH, LinksPage } from './links.js';
import { Menu, rowMenuHandlers } from './menu.js';
import { loadServices } from './services.js';
import { SETTINGS_PATH, SettingsPage, sectionPath, settingsSection } from './settings.js';
import { toolbar } from './toolbar.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').JobSummary} JobSummary
 * @typedef {import('../client/contract.js').JobId} JobId
 * @typedef {import('../client/contract.js').StateAction} StateAction
 * @typedef {import('../client/contract.js').ProgressSample} ProgressSample
 * @typedef {import('../client/contract.js').Settings} Settings
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {import('../client/contract.js').Accounts} Accounts
 * @typedef {import('./menu.js').MenuEntry} MenuEntry
 * @typedef {import('./settings.js').Density} Density
 * @typedef {import('../client/feed.js').FeedStatus} FeedStatus
 * @typedef {import('../client/jobs.js').Tone} Tone
 */

/** Jobs per page. */
export const PAGE_SIZE = 50;

/** @type {Record<StateAction, string>} */
const ACTION_LABELS = { pause: 'Pause', resume: 'Resume', cancel: 'Cancel', retry: 'Retry' };

/** The state commands, in toolbar order. @type {readonly StateAction[]} */
const STATE_ACTIONS = ['pause', 'resume', 'cancel', 'retry'];

/** What a state command did, and did not do, to several downloads. */
/** @type {Record<StateAction, [string, string]>} */
const ACTION_OUTCOMES = {
  pause: ['Paused', 'not paused'],
  resume: ['Resumed', 'not resumed'],
  cancel: ['Cancelled', 'not cancelled'],
  retry: ['Retrying', 'not retried'],
};

/** Why a menu item does not apply, when the job's state is the reason. */
const NOT_FINISHED = 'Once it has finished';
const PENDING = 'Waiting for the server to answer';
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
      if (error.retryAt !== undefined) {
        const at = new Date(error.retryAt).toLocaleString();
        return `The source asked Wayfetch to wait. You can retry after ${at}.`;
      }
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
 * Whether retrying `job` may ask its provider for a link again, which the
 * user confirms first: its last request's answer was lost.
 * @param {JobSummary} job
 */
function asksProviderAgain(job) {
  return !!job.route && job.failure?.code === 'operation_ambiguous';
}

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
    this.feed = new JobFeed(client, {
      pageSize: PAGE_SIZE,
      onChange: () => this.schedule(),
      // Collecting and starting links change the Links list, and so does
      // a video link's title arriving; reconnecting may have missed any.
      onEvent: (kind) => {
        if (kind === null || kind.type === 'batch_accepted' || kind.type === 'job_added' || kind.type === 'links_changed') {
          void this.linksPage.refresh();
        }
        // Another client, or window, changed the settings.
        if (kind === null || kind.type === 'settings_changed') {
          this.loadSettings();
          this.settingsPage.changed(kind === null ? null : kind.revision);
        }
      },
    });
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
    /**
     * The link of the job in the details, once read; `null` if it could
     * not be.
     * @type {{ job: JobId, url: string | null | undefined } | null}
     */
    this.link = null;
    /**
     * What names the account a job in the details goes through, once read:
     * the accounts (`null` without the `accounts` permission or if they
     * could not be read) and the services' names.
     * @type {{ job: JobId, accounts: Accounts | null, names: Map<string, string> } | null | undefined}
     */
    this.routeNames = undefined;

    /** Connection state, for the status bar. */
    this.status = h('span', { class: 'status', role: 'status', 'data-status': 'connecting' });
    this.speed = h('span', { class: 'speed' });
    this.shown = h('span', { class: 'shown' });
    this.banner = h('div', { class: 'banner', role: 'alert', hidden: true });
    this.noticeLine = h('p', { class: 'notice', role: 'status' });
    /**
     * Jobs with a command sent and not yet answered, which is not sent
     * again meanwhile.
     * @type {Set<JobId>}
     */
    this.inFlight = new Set();
    /** Downloads ticked for commands on several at once. @type {Selection<JobId>} */
    this.selection = new Selection();
    this.selectAll = h('input', {
      type: 'checkbox',
      'data-key': 'select-all-jobs',
      'aria-label': 'Select all downloads shown',
      onchange: () => this.tick(this.shownJobs, this.selectAll.checked),
    });
    this.selectionLine = h('p', { class: 'selection-summary', role: 'status' });
    this.actionButtons = STATE_ACTIONS.map((action) =>
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
        icon(action),
        ACTION_LABELS[action],
      ),
    );
    /** Links in the collected list, once read. @type {number | null} */
    this.linkTotal = null;
    /** Each toolbar's view switcher, kept current by `renderViews`. */
    /** @type {HTMLElement[]} */
    this.viewSwitchers = [];
    this.filters = h('nav', { class: 'filters', 'aria-label': 'Filter by status' });
    this.tableBody = h('tbody');
    this.listStatus = h('p', { class: 'list-status', role: 'status' });
    this.pager = h('nav', { class: 'pager', 'aria-label': 'Pages' });
    this.inspector = h('section', { class: 'inspector', 'aria-label': 'Selected download' });
    /** @type {Density} */
    this.density = 'compact';
    this.setDensity(savedDensity());

    /** The name filter, cleared when links just started are shown. */
    this.search = h('input', {
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

    /** The Downloads view: everything but the status bar. */
    this.view = h(
      'div',
      { class: 'downloads-view' },
      this.downloadsToolbar(),
      this.banner,
      h(
        'div',
        { class: 'body' },
        this.filters,
        h(
          'main',
          { class: 'list' },
          this.noticeLine,
          this.selectionLine,
          h('div', { class: 'table-scroll' }, this.buildTable(), this.listStatus),
          this.pager,
          this.inspector,
        ),
      ),
    );
    this.settingsPage = new SettingsPage({
      client,
      hello: () => this.feed.hello,
      density: () => this.density,
      setDensity: (density) => this.setDensity(density),
      onSettings: (settings) => {
        this.settings = settings;
        this.schedule();
      },
      onSignedOut: () => this.signedOut(),
      navigate: (path) => this.navigate(path),
      back: () => this.navigate(this.viewPath),
      backLabel: () => this.backLabel(),
    });
    this.addPage = new AddLinksPage({
      client,
      hello: () => this.feed.hello,
      settings: () => this.settings,
      onSettings: (settings) => {
        this.settings = settings;
        this.schedule();
        this.linksPage.render();
      },
      onCollected: (result) => this.collected(result),
      onSignedOut: () => this.signedOut(),
      back: () => this.navigate(this.viewPath),
      backLabel: () => this.backLabel(),
      settingsButton: this.settingsButton(),
    });
    /** The menu of commands for collected links. */
    this.linksMenu = new Menu('Link actions', () => this.linksPage.markMenu());
    this.linksPage = new LinksPage({
      client,
      hello: () => this.feed.hello,
      settings: () => this.settings,
      rootPath: (id) => this.rootPath(id),
      navigate: (path) => this.navigate(path),
      onSignedOut: () => this.signedOut(),
      onTotal: (total) => {
        this.linkTotal = total;
        this.renderViews();
      },
      onStarted: (text) => this.started(text),
      toolbar: (tools, search) => this.toolbar(tools, search),
      menu: this.linksMenu,
    });
    /** The Downloads or Links route last shown, which Settings goes back to. */
    this.viewPath = '/';
    /** The Downloads route last shown. */
    this.downloadsPath = '/';
    /** What had focus in that view when Settings or Add links opened, for coming back. */
    /** @type {string | null} */
    this.returnFocus = null;

    this.element = h(
      'div',
      { class: 'workspace' },
      this.view,
      this.linksPage.element,
      this.addPage.element,
      this.settingsPage.element,
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
      this.linksMenu.element,
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

  /** Reads the permitted folders, for adding links, the Links page, and the inspector. */
  loadSettings() {
    this.client.settings().then(
      (settings) => {
        this.settings = settings;
        this.schedule();
        this.linksPage.render();
      },
      (error) => this.report(error),
    );
  }

  /**
   * Shows the Settings page, at `section` if given; going back returns
   * focus to what had it.
   * @param {import('./settings.js').Section} [section]
   */
  openSettings(section) {
    this.navigate(section ? sectionPath(section) : SETTINGS_PATH);
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
    this.linksPage.close();
  }

  /** @param {string} path */
  navigate(path) {
    if (location.pathname !== path) history.pushState(null, '', path);
    this.route();
  }

  /** The view or page shown. */
  get current() {
    if (this.settingsPage.shown) return this.settingsPage.element;
    if (this.addPage.shown) return this.addPage.element;
    return this.linksPage.shown ? this.linksPage.element : this.view;
  }

  route() {
    const section = settingsSection(location.pathname);
    if (section !== null) {
      this.showSettings(section);
      return;
    }
    if (location.pathname === ADD_PATH) {
      this.showAdd();
      return;
    }
    const back = this.leavePage();
    const links = location.pathname === LINKS_PATH;
    if ((this.viewPath === LINKS_PATH) !== links) this.closeMenus();
    this.viewPath = location.pathname;
    this.view.hidden = links;
    if (links) {
      this.linksPage.open();
    } else {
      this.linksPage.close();
      this.downloadsPath = location.pathname;
      const job = jobFromPath(location.pathname);
      if (job !== this.feed.watched) this.notice = '';
      this.feed.watch(job).catch((error) => this.report(error));
    }
    this.current.querySelector('.toolbar')?.after(this.banner);
    this.render();
    if (back !== null) refocus(this.current, back);
  }

  closeMenus() {
    this.menu.close(false);
    this.linksMenu.close(false);
  }

  /**
   * Shows the Settings page at `section` in place of the view shown,
   * which keeps its state, as the feed keeps following the engine.
   * @param {import('./settings.js').Section} section
   */
  showSettings(section) {
    const path = sectionPath(section);
    if (location.pathname !== SETTINGS_PATH && location.pathname !== path) history.replaceState(null, '', path);
    if (!this.settingsPage.shown) {
      this.enterPage('open-settings');
      this.settingsPage.element.querySelector('.toolbar')?.after(this.banner);
    }
    this.settingsPage.open(section);
    this.render();
  }

  /** Shows the Add links page in place of the view shown. */
  showAdd() {
    if (!this.addPage.shown) {
      this.enterPage('add-links');
      this.addPage.element.querySelector('.toolbar')?.after(this.banner);
    }
    this.addPage.open();
    this.render();
  }

  /**
   * Hides the view or page shown, for Settings or Add links, noting what
   * to focus on coming back: what had focus in a view, else `fallback`.
   * @param {string} fallback
   */
  enterPage(fallback) {
    this.closeMenus();
    if (!this.settingsPage.shown && !this.addPage.shown) {
      this.returnFocus = focusedKey(this.current) ?? fallback;
    }
    this.settingsPage.close();
    this.addPage.close();
    this.view.hidden = true;
    this.linksPage.close();
  }

  /**
   * Leaves Settings or Add links, if shown, and says what to focus in the
   * view shown next.
   * @returns {string | null}
   */
  leavePage() {
    if (!this.settingsPage.shown && !this.addPage.shown) return null;
    this.settingsPage.close();
    this.addPage.close();
    return this.returnFocus;
  }

  /** What going back from Settings or Add links is called. */
  backLabel() {
    return this.viewPath === LINKS_PATH ? 'Back to Links' : 'Back to Downloads';
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
    const page = this.current;
    const focused = focusedKey(page);
    this.status.dataset.status = feed.status;
    show(this.status, STATUS_LABELS[feed.status]);
    show(this.noticeLine, this.notice);
    this.noticeLine.classList.toggle('ok', this.notice !== '' && this.notice === this.succeeded);
    this.renderBanner();
    this.renderViews();
    this.renderFilters();
    this.renderList();
    this.renderActions();
    this.renderInspector();
    refocus(page, focused);
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

  // Toolbars, views, and adding links.

  /**
   * A toolbar for the Downloads or Links view: adding links, switching
   * views, `tools` for what is selected there, `search`, and Settings.
   * @param {Node} tools
   * @param {Node} search
   */
  toolbar(tools, search) {
    const views = h('nav', { class: 'views', 'aria-label': 'Views' });
    this.viewSwitchers.push(views);
    const bar = toolbar(
      [
        h(
          'button',
          { type: 'button', class: 'primary', 'data-key': 'add-links', onclick: () => this.navigate(ADD_PATH) },
          icon('add'),
          'Add links',
        ),
        views,
        tools,
        search,
      ],
      this.settingsButton(),
    );
    bar.classList.add('queue-toolbar');
    return bar;
  }

  /** Opens Settings, at the end of each page's toolbar but its own. */
  settingsButton() {
    return h(
      'button',
      {
        type: 'button',
        class: 'tool settings-button',
        'data-key': 'open-settings',
        onclick: () => this.openSettings(),
      },
      icon('settings'),
      'Settings',
    );
  }

  downloadsToolbar() {
    const { search } = this;
    return this.toolbar(
      h('div', { class: 'tool-group', role: 'group', 'aria-label': 'Selected downloads' }, this.actionButtons),
      h(
        'div',
        { class: 'search' },
        icon('search'),
        h('label', { for: 'filter-names', class: 'visually-hidden' }, 'Filter this page by name'),
        search,
      ),
    );
  }

  /** Marks the view shown in each switcher, with the Links count. */
  renderViews() {
    const links = this.viewPath === LINKS_PATH;
    const total = this.linkTotal;
    for (const views of this.viewSwitchers) {
      const focused = focusedKey(views);
      show(
        views,
        this.viewLink('/', 'Downloads', !links, null),
        this.viewLink(LINKS_PATH, 'Links', links, total),
      );
      refocus(views, focused);
    }
  }

  /**
   * @param {string} path
   * @param {string} label
   * @param {boolean} current
   * @param {number | null} total
   */
  viewLink(path, label, current, total) {
    return h(
      'a',
      {
        href: path,
        class: 'view',
        'data-key': `view-${label.toLowerCase()}`,
        'aria-current': current ? 'page' : null,
        // Read as "Links (3)", not "Links3".
        'aria-label': total ? `${label} (${total})` : null,
        onclick: (/** @type {MouseEvent} */ event) => {
          if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey) return;
          event.preventDefault();
          // Downloads as last left, with its selected download.
          this.navigate(path === '/' ? this.downloadsPath : path);
        },
      },
      label,
      total ? h('span', { class: 'count' }, total) : null,
    );
  }

  /**
   * Shows links just collected on the Links page, selected and with focus
   * on the first of them.
   * @param {import('./collect.js').Collected} result
   */
  collected(result) {
    this.returnFocus = null;
    this.navigate(LINKS_PATH);
    this.linksPage.say(collectedText(result), { ok: result.invalid === 0, warn: result.invalid > 0 });
    void this.linksPage.refresh().then(() => this.linksPage.selectBatch(result.batch));
  }

  /**
   * Shows the whole Downloads list once links were started, saying how
   * many: no state or name filter hides the new downloads.
   * @param {string} text
   */
  started(text) {
    this.filter('all');
    this.query = '';
    this.search.value = '';
    this.navigate('/');
    this.done(text);
    this.schedule();
  }

  /** Whether this client may change downloads. */
  get canControl() {
    return this.client.permissions.includes('control');
  }

  /**
   * What the toolbar acts on: the ticked downloads shown, otherwise, with
   * nothing ticked at all, the one in the details. Ticks the name filter
   * hides leave nothing to act on rather than the download in the details.
   * @returns {JobSummary[]}
   */
  targets() {
    if (this.selection.size > 0) return this.selection.chosen(this.shownJobs);
    const job = this.feed.watchedJob;
    return job ? [job] : [];
  }

  /**
   * Those of `jobs` that take `action` from this view now.
   * @param {readonly JobSummary[]} jobs
   * @param {StateAction} action
   */
  accepting(jobs, action) {
    if (!this.canControl) return [];
    return jobs.filter((job) => !this.inFlight.has(job.id) && availableActions(job).includes(action));
  }

  /**
   * What a state command is called for `jobs`: with the name of one, or
   * with how many of several it applies to.
   * @param {StateAction} action
   * @param {readonly JobSummary[]} jobs
   */
  actionLabel(action, jobs) {
    const label = ACTION_LABELS[action];
    if (jobs.length === 1) return `${label} ${jobName(jobs[0])}`;
    const n = this.accepting(jobs, action).length;
    return jobs.length > 1 ? `${label} ${count(n || jobs.length, 'download')}` : label;
  }

  /** Enables the toolbar's commands that its downloads accept. */
  renderActions() {
    const jobs = this.targets();
    for (const button of this.actionButtons) {
      const action = /** @type {StateAction} */ (button.dataset.action);
      const accepting = this.accepting(jobs, action);
      button.disabled = accepting.length === 0;
      button.setAttribute('aria-label', this.actionLabel(action, jobs));
      if (action === 'retry') button.title = jobs.length === 1 && accepting.length > 0 ? retryPlan(jobs[0]) : '';
    }
  }

  /**
   * Sends a state command from the toolbar.
   * @param {StateAction} action
   * @param {HTMLButtonElement} button
   */
  async command(action, button) {
    const jobs = this.targets();
    if (jobs.length === 0) return;
    button.disabled = true;
    await this.sendAll(jobs, action);
  }

  /**
   * Sends `action` for each of `jobs` that takes it, and says in one line
   * what happened: how many it applied to, why the engine refused any,
   * and which were left alone. One job is handled as before, with its own
   * name in what is said. Retrying a download whose provider may be asked
   * again is confirmed for each such download on its own.
   * @param {readonly JobSummary[]} jobs
   * @param {StateAction} action
   */
  async sendAll(jobs, action) {
    if (jobs.length === 1) {
      if (action === 'retry') await this.retry(jobs[0]);
      else await this.send(jobs[0], action);
      return;
    }
    const accepting = this.accepting(jobs, action);
    const confirm = action === 'retry' ? accepting.filter(asksProviderAgain) : [];
    const sent = accepting.filter((job) => !confirm.includes(job));
    /** @type {{ message: string }[]} */
    const refused = [];
    const accepted = await Promise.all(
      sent.map((job) => this.send(job, action, { refused: (message) => refused.push({ message }) })),
    );
    const n = accepted.filter(Boolean).length;
    const [did, didNot] = ACTION_OUTCOMES[action];
    const skipped = jobs.length - accepting.length;
    const parts = [
      n > 0 && `${did} ${count(n, 'download')}.`,
      refused.length > 0 && `${count(refused.length, 'download')} ${didNot}: ${refusals(refused, 'downloads')}`,
      skipped > 0 && `${count(skipped, 'selected download')} ${skipped === 1 ? 'was' : 'were'} left as ${skipped === 1 ? 'it was' : 'they were'}: ${ACTION_LABELS[action]} does not apply.`,
      confirm.length > 0 &&
        `${count(confirm.length, 'download')} through an account must be retried on ${confirm.length === 1 ? 'its' : 'their'} own, since the provider may be asked again.`,
    ].filter(Boolean);
    const text = parts.join(' ');
    if (n > 0 && parts.length === 1) this.done(text);
    else this.notice = text;
    this.schedule();
  }

  /**
   * Sends one command for `job` at the revision shown, then re-reads it.
   * Resolves to whether the engine accepted it; a refusal is shown in the
   * notice line, or passed to `refused` if given. While the engine has not
   * answered, the job takes no other command from this view, so a second
   * click cannot send a second request.
   * @param {JobSummary} job
   * @param {StateAction | 'rename' | 'remove' | 'delete_files'} action
   * @param {{
   *   name?: string,
   *   repeatProviderOperation?: boolean,
   *   refused?: (text: string, error?: WayfetchError) => void,
   * }} [options]
   */
  async send(job, action, { name, repeatProviderOperation, refused } = {}) {
    const hello = this.feed.hello;
    if (!hello) {
      (refused ?? ((text) => (this.notice = text)))('Not connected yet. Try again in a moment.');
      this.schedule();
      return false;
    }
    if (this.inFlight.has(job.id)) return false;
    this.inFlight.add(job.id);
    this.notice = '';
    this.schedule();
    let accepted = false;
    try {
      await this.client.jobCommand(job.id, {
        idempotencyKey: idempotencyKey(),
        target: hello.instance,
        action,
        expectedRevision: job.revision,
        name,
        repeatProviderOperation,
      });
      accepted = true;
    } catch (error) {
      if (!(error instanceof WayfetchError)) throw error;
      if (error.code === 'unauthenticated') {
        this.signedOut();
        return false;
      }
      const text = commandFailed(error);
      if (refused) refused(text, error);
      else this.notice = `${jobName(job)}: ${text}`;
    } finally {
      this.inFlight.delete(job.id);
    }
    this.schedule();
    if ((action !== 'remove' && action !== 'delete_files') || !accepted) {
      await this.feed.refresh(job.id).catch((error) => this.report(error));
    }
    return accepted;
  }

  /**
   * The current summary of job `id`, if this view holds it.
   * @param {JobId} id
   */
  heldJob(id) {
    const watched = this.feed.watchedJob;
    return watched?.id === id ? watched : this.feed.jobs.find((j) => j.id === id);
  }

  /**
   * Starts a failed or cancelled job again, and says what that does: it
   * continues the saved data when the server still offers the same file.
   * A refusal because the job changed meanwhile is explained from its
   * state once re-read.
   * A download whose provider request may have gone through without its
   * answer arriving is retried only once the user accepts that the
   * provider may do that work again.
   * @param {JobSummary} job
   * @param {boolean} [repeat] the user accepted repeating it
   */
  async retry(job, repeat = false) {
    const name = jobName(job);
    if (asksProviderAgain(job) && !repeat) {
      if (this.client.permissions.includes('provider_downloads')) this.confirmRepeat(job);
      else this.notice = `${name}: retrying asks the provider for its link again, which needs permission to download through accounts.`;
      // The toolbar's Retry, disabled while acting, is drawn enabled again.
      this.schedule();
      return;
    }
    /** @type {WayfetchError | undefined} */
    let refusal;
    const accepted = await this.send(job, 'retry', {
      repeatProviderOperation: repeat || undefined,
      refused: (text, error) => {
        refusal = error;
        this.notice = `${name}: ${text}`;
      },
    });
    if (accepted) {
      this.done(
        job.bytes_done > 0
          ? `Retrying ${name}. It continues from the ${formatBytes(job.bytes_done)} saved if the server still offers the same file, and otherwise starts over and says why.`
          : `Retrying ${name} from the beginning.`,
      );
    } else if (refusal?.code === 'revision_conflict') {
      const now = this.heldJob(job.id);
      this.notice =
        now && availableActions(now).includes('retry')
          ? `${name} changed in the meantime and is still stopped. Check it, then choose Retry again.`
          : `${name} was already started again or changed state, so it was not retried twice.`;
    }
    this.schedule();
  }

  /**
   * Asks before retrying `job`, whose provider request may have gone
   * through: the provider may then make its download link a second time.
   * @param {JobSummary} job
   */
  confirmRepeat(job) {
    const name = jobName(job);
    const retry = h(
      'button',
      {
        type: 'button',
        class: 'primary',
        'data-key': 'confirm-repeat',
        onclick: () => {
          dialog.close();
          void this.retry(job, true);
        },
      },
      'Ask again',
    );
    const cancel = h('button', { type: 'button', onclick: () => dialog.close() }, 'Cancel');
    const dialog = openDialog(
      { title: `Retry ${name}?`, className: 'confirm-repeat' },
      h(
        'p',
        null,
        'Wayfetch asked the provider for a download link, but its answer was lost, so Wayfetch cannot tell whether the provider made one. Wayfetch first looks for that link once more. If it is not found, it asks the provider again, which may count against the account a second time.',
      ),
      h('div', { class: 'row' }, retry, cancel),
    );
    retry.focus();
  }

  // The row menu.

  /**
   * Opens the menu for `job` at `anchor`. Focus returns to the row's menu
   * button, which a redraw may have replaced.
   * @param {JobSummary} job
   * @param {import('./menu.js').Anchor} anchor
   */
  openMenu(job, anchor) {
    const targets = this.menuTargets(job);
    const restore = () => {
      const button = /** @type {HTMLElement | null} */ (
        this.tableBody.querySelector(`[data-key="menu-${job.id}"]`)
      );
      (button ?? this.tableBody.querySelector(`[data-key="open-${job.id}"]`))?.focus?.();
    };
    const entries = targets.length > 1 ? this.manyEntries(targets) : this.menuEntries(job);
    this.menu.show(entries, anchor, { owner: job.id, restore });
  }

  /**
   * The downloads a menu opened on `job` acts on: the ticked ones shown,
   * when `job` is one of them, otherwise `job` alone.
   * @param {JobSummary} job
   */
  menuTargets(job) {
    const ticked = this.selection.chosen(this.shownJobs);
    return ticked.some((j) => j.id === job.id) ? ticked : [job];
  }

  /**
   * What can be done with several downloads at once: the state commands
   * and removal, each for those it applies to.
   * @param {readonly JobSummary[]} jobs
   * @returns {MenuEntry[]}
   */
  manyEntries(jobs) {
    const control = this.canControl;
    const removable = jobs.filter(canRemove);
    /** @type {MenuEntry[]} */
    const entries = STATE_ACTIONS.map((action) => ({
      key: action,
      label: this.actionLabel(action, jobs),
      disabled: this.accepting(jobs, action).length === 0,
      hint: control ? undefined : NO_CONTROL,
      run: () => void this.sendAll(jobs, action),
    }));
    entries.push('separator', {
      key: 'remove',
      label: removable.length > 1 ? `Remove ${count(removable.length, 'download')}…` : 'Remove…',
      disabled: !control || removable.length === 0,
      danger: true,
      run: () => void this.remove(removable),
    });
    return entries;
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
    const busy = this.inFlight.has(job.id);
    const actions = new Set(control && !busy ? availableActions(job) : []);
    /** Why a state command is unavailable, or `fallback` for its state. */
    const why = (/** @type {string | undefined} */ fallback) => (!control ? NO_CONTROL : busy ? PENDING : fallback);
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
        hint: why(undefined),
        run: () => void this.send(job, toggle),
      },
      {
        key: 'cancel',
        label: 'Cancel download',
        disabled: !actions.has('cancel'),
        hint: why(undefined),
        run: () => void this.send(job, 'cancel'),
      },
      {
        key: 'retry',
        label: 'Retry',
        disabled: !actions.has('retry'),
        run: () => void this.retry(job),
      },
      {
        key: 'remove',
        label: 'Remove…',
        disabled: !control || !canRemove(job),
        danger: true,
        run: () => void this.remove([job]),
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
   * Asks whether to keep or explicitly delete ended downloads' files, then
   * removes them. Downloads not removed stay in the dialog, with why.
   * @param {readonly JobSummary[]} jobs
   */
  remove(jobs) {
    if (jobs.length === 0) return;
    const one = jobs.length === 1;
    const name = one ? jobName(jobs[0]) : count(jobs.length, 'download');
    /** Those not removed yet. */
    let left = [...jobs];
    const message = h('p', { class: 'message', role: 'alert' });
    const keep = h('input', { type: 'radio', name: 'removal', value: 'keep', checked: true });
    const mayDelete = this.client.permissions.includes('delete_files');
    const erase = h('input', { type: 'radio', name: 'removal', value: 'delete', disabled: !mayDelete });
    const save = h('button', { type: 'submit', class: 'primary' }, 'Remove');
    const cancel = h('button', { type: 'button', onclick: () => dialog.close() }, 'Cancel');
    let sending = false;
    const form = h('form', {
      class: 'rename-form',
      onsubmit: async (/** @type {SubmitEvent} */ event) => {
        event.preventDefault();
        if (sending) return;
        const deleting = erase.checked && mayDelete;
        const unfinished = deleting
          ? ' Deletion did not finish; some data may already have been deleted. You can retry or remove the entry only.'
          : '';
        sending = true;
        save.disabled = cancel.disabled = keep.disabled = erase.disabled = true;
        const close = dialog.querySelector('button.close');
        if (close instanceof HTMLButtonElement) close.disabled = true;
        show(message, '');
        /** @type {{ message: string }[]} */
        const refused = [];
        let unconfirmed = 0;
        const sent = left;
        const outcomes = await Promise.all(
          sent.map((job) =>
            this.send(job, deleting ? 'delete_files' : 'remove', {
              refused: (text) => refused.push({ message: text }),
            }).catch(() => {
              unconfirmed += 1;
              return false;
            }),
          ),
        );
        sending = false;
        save.disabled = cancel.disabled = keep.disabled = false;
        erase.disabled = !mayDelete;
        if (close instanceof HTMLButtonElement) close.disabled = false;
        const removed = sent.filter((_, i) => outcomes[i]);
        left = sent.filter((_, i) => !outcomes[i]);
        if (removed.some((job) => job.id === this.feed.watched)) this.navigate('/');
        if (left.length === 0) {
          dialog.close();
          this.done(
            one
              ? deleting
                ? `Deleted downloaded data for ${name} and removed it from the list.`
                : `Removed ${name} from the list. Remaining files stay in their folder.`
              : deleting
                ? `Deleted downloaded data for ${name} and removed them from the list.`
                : `Removed ${name} from the list. Remaining files stay in their folders.`,
          );
          this.schedule();
          return;
        }
        const notConfirmed = deleting
          ? 'Deletion could not be confirmed; some data may already have been deleted. Check the download, then retry or remove the entry only.'
          : 'The removal could not be confirmed. Check the download and try again.';
        if (one) {
          show(message, unconfirmed > 0 ? notConfirmed : `${refused[0]?.message ?? ''}${unfinished}`);
          return;
        }
        show(
          message,
          [
            removed.length > 0 && `Removed ${count(removed.length, 'download')}.`,
            `${count(left.length, 'download')} not removed${refused.length > 0 ? `: ${refusals(refused, 'downloads')}` : '.'}`,
            unconfirmed > 0 ? notConfirmed : unfinished.trim(),
          ]
            .filter(Boolean)
            .join(' '),
        );
      },
    },
    h('p', {}, `Remove ${name} from Downloads?`),
    h('label', { class: 'row' }, keep, 'Remove from list only (keep files)'),
    h('label', { class: 'row' }, erase, `Delete downloaded ${one ? 'file/data' : 'files/data'} and remove from list`),
    h('p', { class: 'hint' }, mayDelete
      ? `If you choose to delete files, the downloaded ${one ? 'file' : 'files'} and partial data will be permanently deleted. This cannot be undone.`
      : 'Your access does not allow deleting downloaded data.'),
    message,
    h('div', { class: 'row' }, save, cancel));
    const dialog = openDialog({ title: one ? 'Remove download' : `Remove ${name}`, closeLabel: 'Cancel removal' }, form);
    dialog.addEventListener('cancel', (event) => { if (sending) event.preventDefault(); });
    const choiceChanged = () => {
      save.textContent = erase.checked ? 'Delete files and remove' : 'Remove';
      save.className = erase.checked ? 'primary delete-confirm' : 'primary';
    };
    keep.addEventListener('change', choiceChanged);
    erase.addEventListener('change', choiceChanged);
    keep.focus();
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
      h('span', { class: 'filters-title' }, 'Status'),
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
          h('th', { scope: 'col', class: 'check' }, this.selectAll),
          columns.map(([title, kind]) => h('th', { scope: 'col', class: kind }, title)),
          actions,
        ),
      ),
      this.tableBody,
    );
  }

  /** The jobs of the page that the name filter shows. */
  get shownJobs() {
    return this.feed.jobs.filter((job) => this.query === '' || jobName(job).toLowerCase().includes(this.query));
  }

  /**
   * Ticks or unticks `jobs`.
   * @param {readonly JobSummary[]} jobs
   * @param {boolean} on
   */
  tick(jobs, on) {
    this.selection.pick(jobs, on);
    this.render();
  }

  /**
   * How many downloads are ticked, with a way to untick them, and how
   * many of them the name filter hides, which commands leave out. While
   * downloads are shown the line keeps its room, saying how many are
   * shown when none is ticked, so ticking one moves no row.
   * @param {readonly JobSummary[]} shown
   */
  renderSelection(shown) {
    const ticked = this.selection.chosen(shown).length;
    const hidden = this.selection.size - ticked;
    show(
      this.selectionLine,
      ticked > 0 && h('strong', null, `${count(ticked, 'download')} selected`),
      ticked === 0 && hidden === 0 && shown.length > 0 && h('span', null, `${count(shown.length, 'download')} shown`),
      hidden > 0 &&
        h('span', { class: 'warn-text' }, `${count(hidden, 'selected download')} hidden by the filter ${hidden === 1 ? 'is' : 'are'} left out.`),
      this.selection.size > 0 &&
        h(
          'button',
          { type: 'button', class: 'link', 'data-key': 'clear-selection', onclick: () => this.tick(this.feed.jobs, false) },
          'Clear selection',
        ),
    );
  }

  renderList() {
    const { feed } = this;
    const selected = feed.watched;
    // Ticks last only while their downloads are on the page.
    this.selection.keepOnly(feed.jobs.map((job) => job.id));
    const jobs = this.shownJobs;
    Object.assign(this.selectAll, this.selection.allState(jobs));
    this.renderSelection(jobs);
    const rows = new Map();
    for (const job of jobs) {
      const sample = feed.sample(job.id);
      const isSelected = job.id === selected;
      const old = this.rows.get(job.id);
      let row =
        old &&
        old.job === job &&
        old.sample === sample &&
        old.selected === isSelected
          ? old.row
          : this.row(job, sample, isSelected);
      if (old && row !== old.row) {
        // Progress and fresh snapshots must not remove the name link or
        // menu button between pointer-down and click.
        const name = old.row.querySelector('td.name');
        const link = old.row.querySelector('td.name a');
        const more = old.row.querySelector('button.more');
        if (name && link && more) {
          name.setAttribute('title', jobName(job));
          if (link.textContent !== jobName(job)) link.textContent = jobName(job);
          if (isSelected) link.setAttribute('aria-current', 'true');
          else link.removeAttribute('aria-current');
          more.setAttribute('aria-label', `Actions for ${jobName(job)}`);
          old.row.classList.toggle('selected', isSelected);
          // These cells contain display values only; the interactive
          // cells and their ancestors stay connected.
          const cells = [...row.cells];
          for (let i = 2; i < 8; i++) old.row.cells[i].replaceWith(cells[i]);
          row = old.row;
        }
      }
      // Kept up to date in place, so opening a menu or ticking redraws no row.
      const menuOpen = this.menu.owner === job.id;
      row.classList.toggle('menu-open', menuOpen);
      row.querySelector('button.more')?.setAttribute('aria-expanded', String(menuOpen));
      const ticked = this.selection.has(job.id);
      row.classList.toggle('ticked', ticked);
      /** @type {HTMLInputElement} */ (row.querySelector('td.check input')).checked = ticked;
      rows.set(job.id, { job, sample, selected: isSelected, row });
    }
    this.rows = rows;
    show(this.tableBody, [...rows.values()].map(({ row }) => row));

    let empty = '';
    if (feed.status === 'live' && jobs.length === 0) {
      if (feed.jobs.length > 0) empty = 'No downloads on this page match the filter.';
      else if (this.group !== 'all') empty = 'No downloads in this state.';
      else empty = 'No downloads yet. Use Add links to collect some, then start them on the Links page.';
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
    const current = () => this.rows.get(job.id)?.job ?? job;
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
        onclick: (/** @type {MouseEvent} */ event) =>
          this.menu.toggle(job.id, /** @type {HTMLElement} */ (event.currentTarget), (anchor) => this.openMenu(current(), anchor)),
      },
      icon('more'),
    );
    return h(
      'tr',
      {
        'data-job': job.id,
        class: selected ? 'selected' : null,
        onclick: (/** @type {MouseEvent} */ event) => {
          if (/** @type {Element} */ (event.target).closest('a, button, input')) return;
          // Ctrl- or Cmd-click ticks, as in file managers; a click shows details.
          if (event.ctrlKey || event.metaKey) this.tick([current()], !this.selection.has(job.id));
          else this.navigate(path);
        },
        // The row pressed, wherever in it; the browser's own menu otherwise.
        ...rowMenuHandlers(more, (anchor) => this.openMenu(current(), anchor)),
      },
      h(
        'td',
        { class: 'check' },
        h('input', {
          type: 'checkbox',
          'data-key': `select-${job.id}`,
          'aria-label': `Select ${name}`,
          onchange: (/** @type {Event} */ event) =>
            this.tick([current()], /** @type {HTMLInputElement} */ (event.currentTarget).checked),
        }),
      ),
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
        icon('cancel'),
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
    const name = fileName(job) ?? job.name;
    const where = this.location(job);
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
        ['File', name === undefined ? 'Named when it finishes' : h('span', { class: 'path', title: where.file }, name)],
        ['Type', name && fileKind(name)],
        ['Size', shown.size],
        [job.artifact ? 'Saved in' : 'Saves to', this.folder(job, where.folder)],
        ['Download via', this.via(job)],
        ...this.source(job),
      ]),
      job.failure && h('p', { class: `note tone-${state.tone}` }, explain(job.failure)),
      restartNote(job) && h('p', { class: 'note tone-warn', 'data-note': 'restart' }, restartNote(job)),
    );
  }

  /**
   * The folder holding `job`'s file, or the one it will save in, and the
   * file's full path once published. Paths are the engine's: a server's in
   * a browser.
   * @param {JobSummary} job
   * @returns {{ folder: string, file?: string }}
   */
  location(job) {
    const { artifact } = job;
    const id = artifact?.root ?? job.destination;
    if (id === undefined || id === null) return { folder: 'The download folder' };
    const root = this.settings?.roots.find((r) => r.id === id)?.path;
    if (root === undefined) return { folder: this.rootPath(id) };
    if (!artifact) return { folder: root };
    const separator = root.includes('\\') && !root.includes('/') ? '\\' : '/';
    const base = root.replace(/[\\/]+$/, '');
    const parts = artifact.path.split('/');
    return {
      folder: [base, ...parts.slice(0, -1)].join(separator) || root,
      file: [base, ...parts].join(separator),
    };
  }

  /**
   * `folder`, which opens in the file manager where the host can open it:
   * the desktop app, once the file is published. A browser cannot open a
   * server's folders, so there it is labelled as a server path.
   * @param {JobSummary} job
   * @param {string} folder
   */
  folder(job, folder) {
    const { revealFile } = this.client;
    const path =
      revealFile && job.stage === 'complete'
        ? h(
            'button',
            {
              type: 'button',
              class: 'link path',
              'data-key': 'reveal-folder',
              title: `Open ${folder}`,
              onclick: () => void this.local(job, () => revealFile.call(this.client, job.id)),
            },
            folder,
          )
        : h('span', { class: 'path', title: folder }, folder);
    return h('span', { class: 'location' }, path, !this.client.local && h('span', { class: 'tag' }, 'Server path'));
  }

  /**
   * Where `job` came from. With the `sources` permission, its link without
   * credentials, query, or fragment, which can carry a signed token, and a
   * button to copy the whole link; otherwise only its host.
   * @param {JobSummary} job
   * @returns {[string, Node | string | null | undefined][]}
   */
  source(job) {
    if (!this.client.permissions.includes('sources')) return [['Host', job.original_host]];
    if (this.link?.job !== job.id) {
      const link = { job: job.id, url: /** @type {string | null | undefined} */ (undefined) };
      this.link = link;
      this.client.jobSource(job.id).then(
        (url) => {
          link.url = url;
          if (this.link === link) this.schedule();
        },
        () => {
          link.url = null;
          if (this.link === link) this.schedule();
        },
      );
    }
    const shown = typeof this.link.url === 'string' ? displayLink(this.link.url) : null;
    const text = shown ? `${shown.text}${shown.hidden ? '?…' : ''}` : job.original_host;
    if (!text) return [];
    return [
      [
        'Source',
        h(
          'span',
          { class: 'location' },
          h('span', { class: 'path link-text', title: shown?.hidden ? `${text} (query hidden)` : text }, text),
          h(
            'button',
            { type: 'button', class: 'link', 'data-key': 'copy-source-detail', onclick: () => void this.copySource(job) },
            'Copy link',
          ),
        ),
      ],
    ];
  }

  /**
   * How `job` downloads: directly, or the service and account it goes
   * through, named once the accounts are read.
   * @param {JobSummary} job
   */
  via(job) {
    const route = job.route;
    if (!route) return 'Direct';
    if (this.routeNames?.job !== job.id) {
      /** @type {{ job: JobId, accounts: Accounts | null, names: Map<string, string> }} */
      const read = { job: job.id, accounts: null, names: new Map() };
      this.routeNames = read;
      const accounts = this.client.permissions.includes('accounts')
        ? this.client.accounts().catch(() => null)
        : Promise.resolve(null);
      void Promise.all([accounts, loadServices().catch(() => [])]).then(([list, services]) => {
        read.accounts = list;
        for (const service of services) read.names.set(service.id, service.name);
        if (this.routeNames === read) this.schedule();
      });
    }
    const { accounts, names } = this.routeNames;
    return routeLabel(route, accounts, (id) => names.get(id) ?? id);
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
        ['Restarted because', job.restart_reason],
      ]),
    );
  }
}
