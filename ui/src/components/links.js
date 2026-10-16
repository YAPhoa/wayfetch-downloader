// The Links page: links collected for review, grouped by the batch they
// were added in, until they are started or removed. The engine holds the
// list; this page shows one page of it and never keeps its own copy as
// the authority.
//
// - A valid link is "Not checked": collecting never contacts its server,
//   so only its host and the file name its path suggests are known.
// - Commands act on the selected links that are shown. A filter never
//   selects, starts, or removes links it hides; selected links it hides
//   are counted and left out.
// - Each command carries the revisions shown, so a link changed elsewhere
//   since is refused rather than started as it was. Links leave the page
//   only once the engine has confirmed the command; refused ones stay,
//   with the reason.
// - The page re-reads the list when shown, after events that can change
//   it, and now and then while shown, since removals and moves made in
//   another window have no event of their own.
//
// Route: `/links`.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import {
  batches,
  collectedWhen,
  count,
  linkName,
  linkStatus,
  startable,
  matches,
  refusals,
} from '../client/links.js';
import { openDialog } from './dialog.js';
import { copyText, focusedKey, h, refocus, show } from './dom.js';
import { icon } from './icons.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').Settings} Settings
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {import('../client/contract.js').LinkId} LinkId
 * @typedef {import('../client/contract.js').CollectedLink} CollectedLink
 * @typedef {import('../client/contract.js').LinksAction} LinksAction
 * @typedef {import('../client/contract.js').CommandResult} CommandResult
 * @typedef {import('./menu.js').Menu} Menu
 * @typedef {import('./menu.js').MenuEntry} MenuEntry
 * @typedef {import('./menu.js').Anchor} Anchor
 */

/**
 * What the page works with, from the workspace hosting it.
 * @typedef {{
 *   client: WayfetchClient,
 *   hello: () => Hello | null,
 *   settings: () => Settings | null,
 *   rootPath: (id: RootId) => string,
 *   navigate: (path: string) => void,
 *   onSignedOut: () => void,
 *   onTotal: (total: number) => void,
 *   toolbar: (tools: Node, search: Node) => Node,
 *   menu: Menu,
 * }} LinksContext
 */

/** The page's path. */
export const LINKS_PATH = '/links';

/** Links per page. */
export const LINKS_PAGE_SIZE = 100;

/** How often the list is read again while shown, in milliseconds. */
export const LINKS_REFRESH = 5000;

const NO_SUBMIT = 'You may only view links';
const NO_CONTROL = 'You may not change or remove links';

/**
 * What to tell the user about a command refused as a whole.
 * @param {WayfetchError} error
 */
function refused(error) {
  switch (error.code) {
    case 'not_found':
      return 'That folder is no longer a permitted destination. Choose another.';
    case 'not_applicable':
      return `That folder cannot be used: ${error.message}`;
    case 'permission_denied':
      return 'You may not change links.';
    case 'unavailable':
      return 'The engine cannot be reached. Nothing changed; try again shortly.';
    default:
      return error.message;
  }
}

export class LinksPage {
  /** @param {LinksContext} context */
  constructor(context) {
    this.context = context;
    this.shown = false;
    /** The page of links shown, as last read. @type {CollectedLink[]} */
    this.links = [];
    /** Links in the whole list, once read. @type {number | null} */
    this.total = null;
    /** Where the shown page starts, and where the next one would. */
    /** @type {LinkId | null} */
    this.pageAfter = null;
    /** @type {LinkId | null} */
    this.nextAfter = null;
    /** `pageAfter` of each earlier page, for going back. @type {(LinkId | null)[]} */
    this.earlier = [];
    /** @type {Set<LinkId>} */
    this.selected = new Set();
    this.tableState = '';
    /** Filter text, lower case. */
    this.query = '';
    /** The outcome of the last command or read. */
    this.notice = { text: '', ok: false, warn: false, downloads: false };
    /** A command is being sent; commands wait for it. */
    this.busy = false;
    /** A read in flight, and whether another was asked for meanwhile. */
    /** @type {Promise<void> | null} */
    this.reading = null;
    this.readAgain = false;
    /** @type {ReturnType<typeof setInterval> | undefined} */
    this.timer = undefined;

    this.buttons = {
      start: this.tool('start', 'resume', 'Start selected', () => this.act('start', this.chosen().filter(startable))),
      move: this.tool('move', 'folder', 'Save to…', () => this.chooseFolder(this.chosen().filter((l) => !l.job))),
      remove: this.tool('remove', 'cancel', 'Remove', () => this.act('remove', this.chosen())),
    };
    const search = h('input', {
      type: 'search',
      id: 'filter-links',
      placeholder: 'Filter links',
      autocomplete: 'off',
      spellcheck: 'false',
      oninput: (/** @type {InputEvent} */ event) => {
        this.query = /** @type {HTMLInputElement} */ (event.currentTarget).value.trim().toLowerCase();
        this.render();
      },
    });
    this.summary = h('p', { class: 'selection-summary', role: 'status' });
    this.noticeText = h('span');
    // Kept, not redrawn, so it stays put while the list is read again.
    this.viewDownloads = h(
      'button',
      { type: 'button', class: 'link', 'data-key': 'open-downloads', hidden: true, onclick: () => context.navigate('/') },
      'View downloads',
    );
    this.noticeLine = h('div', { class: 'notice', role: 'status' }, this.noticeText, this.viewDownloads);
    this.selectAll = h('input', {
      type: 'checkbox',
      'data-key': 'select-all-links',
      'aria-label': 'Select all links shown',
      onchange: () => this.selectShown(this.selectAll.checked),
    });
    this.table = h('table', { class: 'jobs links' });
    this.listStatus = h('p', { class: 'list-status', role: 'status' });
    this.pager = h('nav', { class: 'pager', 'aria-label': 'Pages of links' });
    this.element = h(
      'div',
      { class: 'links-page', hidden: true },
      context.toolbar(
        h('div', { class: 'tool-group', role: 'group', 'aria-label': 'Selected links' }, Object.values(this.buttons)),
        h(
          'div',
          { class: 'search' },
          icon('search'),
          h('label', { for: 'filter-links', class: 'visually-hidden' }, 'Filter links'),
          search,
        ),
      ),
      h(
        'main',
        { class: 'list' },
        this.noticeLine,
        this.summary,
        h('div', { class: 'table-scroll' }, this.table, this.listStatus),
        this.pager,
      ),
    );
  }

  /**
   * A toolbar button for the selected links.
   * @param {string} key
   * @param {import('./icons.js').IconName} name
   * @param {string} label
   * @param {() => void} run
   */
  tool(key, name, label, run) {
    return h(
      'button',
      { type: 'button', class: `tool links-${key}`, 'data-key': `links-${key}`, disabled: true, onclick: run },
      icon(name),
      label,
    );
  }

  get canSubmit() {
    return this.context.client.permissions.includes('submit');
  }

  /** Changing or removing links others may have collected needs control. */
  get canControl() {
    return this.context.client.permissions.includes('control');
  }

  /** Shows the page, reading the list afresh and then now and then. */
  open() {
    if (this.shown) return;
    this.shown = true;
    this.element.hidden = false;
    this.render();
    this.refresh();
    this.timer = setInterval(() => this.refresh(), LINKS_REFRESH);
  }

  /** Hides the page and stops reading the list. */
  close() {
    this.shown = false;
    this.element.hidden = true;
    clearInterval(this.timer);
  }

  /**
   * Reads the shown page of the list again: one read at a time, and one
   * more if asked meanwhile. Resolves once the last read is shown. Also
   * keeps the workspace's count current.
   * @returns {Promise<void>}
   */
  refresh() {
    if (this.reading) {
      this.readAgain = true;
      return this.reading;
    }
    this.reading = (async () => {
      do {
        this.readAgain = false;
        await this.read();
      } while (this.readAgain);
      this.reading = null;
      this.render();
    })();
    return this.reading;
  }

  /** One read of the shown page. */
  async read() {
    const after = this.pageAfter;
    let page;
    try {
      page = await this.context.client.listLinks({ after, pageSize: LINKS_PAGE_SIZE });
    } catch (error) {
      this.report(error);
      return;
    }
    if (after !== this.pageAfter) return;
    // A page emptied by others: show the one before it.
    if (page.items.length === 0 && this.earlier.length > 0) {
      this.pageAfter = this.earlier.pop() ?? null;
      this.readAgain = true;
      return;
    }
    this.links = page.items;
    this.total = Number(page.total);
    this.nextAfter = page.next_after ?? null;
    const present = new Set(page.items.map((l) => l.id));
    for (const id of this.selected) if (!present.has(id)) this.selected.delete(id);
    this.context.onTotal(this.total);
  }

  /**
   * Shows an error from a read; an ended session goes to the shell.
   * @param {unknown} error
   */
  report(error) {
    if (error instanceof WayfetchError && error.code === 'unauthenticated') {
      this.context.onSignedOut();
      return;
    }
    this.say(error instanceof WayfetchError ? error.message : String(error));
  }

  /**
   * @param {string} text
   * @param {{ ok?: boolean, warn?: boolean, downloads?: boolean }} [options]
   *   `ok` for a success, `warn` for one with parts left undone
   */
  say(text, { ok = false, warn = false, downloads = false } = {}) {
    this.notice = { text, ok, warn, downloads };
    if (this.shown) this.render();
  }

  /** The links shown under the filter. */
  get visible() {
    return this.links.filter((link) => matches(link, this.query));
  }

  /** The selected links that are shown: what commands act on. */
  chosen() {
    return this.visible.filter((link) => this.selected.has(link.id));
  }

  /** @param {boolean} on */
  selectShown(on) {
    for (const link of this.visible) {
      if (on) this.selected.add(link.id);
      else this.selected.delete(link.id);
    }
    this.render();
  }

  /**
   * @param {readonly CollectedLink[]} links
   * @param {boolean} on
   */
  select(links, on) {
    for (const link of links) {
      if (on) this.selected.add(link.id);
      else this.selected.delete(link.id);
    }
    this.render();
  }

  // Commands.

  /**
   * Sends `action` for `links` at the revisions shown, then reads the
   * list again, so links leave it only once the engine took the command.
   * @param {LinksAction} action
   * @param {readonly CollectedLink[]} links
   * @param {RootId} [destination]
   */
  async act(action, links, destination) {
    const hello = this.context.hello();
    if (links.length === 0 || this.busy) return;
    if (!hello) {
      this.say('Not connected yet. Try again in a moment.');
      return;
    }
    this.busy = true;
    this.say(action === 'start' ? `Starting ${count(links.length, 'link')}…` : '');
    /** @type {CommandResult | null} */
    let result = null;
    try {
      result = await this.context.client.linksCommand({
        idempotencyKey: idempotencyKey(),
        target: hello.instance,
        action,
        links: links.map((l) => ({ link: l.id, revision: l.revision })),
        destination,
      });
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        this.context.onSignedOut();
        return;
      }
      this.say(error instanceof WayfetchError ? refused(error) : String(error));
    } finally {
      this.busy = false;
    }
    const outcome = result?.type === 'links_applied' ? this.applied(action, result, destination) : null;
    await this.refresh();
    // Said once the list shows it.
    if (outcome) this.say(outcome.text, outcome);
  }

  /**
   * What to say about what a command did; deselects the links it started
   * or removed.
   * @param {LinksAction} action
   * @param {Extract<CommandResult, { type: 'links_applied' }>} result
   * @param {RootId} [destination]
   */
  applied(action, { applied, refused: notApplied }, destination) {
    // Moved links stay selected, ready to start.
    if (action !== 'move') for (const { link } of applied) this.selected.delete(link);
    const n = applied.length;
    const done = {
      start: n > 0 && `Started ${count(n, 'download')}.`,
      remove: n > 0 && `Removed ${count(n, 'link')} from the list.`,
      move: n > 0 && destination && `${count(n, 'link')} will be saved to ${this.context.rootPath(destination)}.`,
    }[action];
    const not = {
      start: 'not started',
      remove: 'not removed',
      move: 'not changed',
    }[action];
    const problems =
      notApplied.length > 0 && `${count(notApplied.length, 'link')} ${not}: ${refusals(notApplied)}`;
    return {
      text: [done, problems].filter(Boolean).join(' '),
      ok: notApplied.length === 0,
      warn: n > 0 && notApplied.length > 0,
      downloads: action === 'start' && n > 0,
    };
  }

  /**
   * Asks where `links` will be saved, among the permitted folders.
   * @param {readonly CollectedLink[]} links
   */
  chooseFolder(links) {
    if (links.length === 0) return;
    const settings = this.context.settings();
    const roots = settings?.roots ?? [];
    const current = new Set(links.map((l) => l.destination));
    const only = current.size === 1 ? [...current][0] : undefined;
    const message = h('p', { class: 'message error', role: 'alert' });
    const save = h('button', { type: 'submit', class: 'primary' }, 'Save');
    const cancel = h('button', { type: 'button', onclick: () => dialog.close() }, 'Cancel');
    const form = h(
      'form',
      {
        onsubmit: (/** @type {SubmitEvent} */ event) => {
          event.preventDefault();
          const picked = /** @type {HTMLInputElement | null} */ (form.querySelector('input:checked'));
          if (!picked) {
            show(message, 'Choose a folder first.');
            return;
          }
          dialog.close();
          void this.act('move', links, /** @type {RootId} */ (picked.value));
        },
      },
      h(
        'fieldset',
        null,
        h('legend', null, `Save ${count(links.length, 'link')} to`),
        roots.length > 0
          ? roots.map((root) =>
              h(
                'label',
                { class: 'choice' },
                h('input', {
                  type: 'radio',
                  name: 'links-root',
                  value: root.id,
                  checked: root.id === only,
                  disabled: !root.available,
                }),
                h(
                  'span',
                  { class: 'choice-text' },
                  h('span', { class: 'path' }, root.path),
                  !this.context.client.local && h('span', { class: 'tag' }, 'Server path'),
                  root.id === only && h('span', { class: 'tag current' }, 'Current'),
                  !root.available &&
                    h('span', { class: 'problem' }, `Unavailable: ${root.problem ?? 'the folder cannot be opened'}`),
                ),
              ),
            )
          : h('p', { class: 'hint' }, 'There is no permitted folder.'),
      ),
      h(
        'p',
        { class: 'hint' },
        'Only these links change. Where new links are saved, and downloads already started, stay as they are. ',
        h(
          'button',
          {
            type: 'button',
            class: 'link',
            onclick: () => {
              dialog.close();
              this.context.navigate('/settings/folder');
            },
          },
          'Add folders in Settings',
        ),
      ),
      message,
      h('div', { class: 'row' }, save, cancel),
    );
    const dialog = openDialog({ title: 'Change where links are saved', className: 'move-links' }, form);
    /** @type {HTMLElement | null} */ (form.querySelector('input:checked, input:enabled'))?.focus();
  }

  /** @param {CollectedLink} link */
  async copyLink(link) {
    try {
      await copyText(await this.context.client.linkSource(link.id));
      this.say(`Copied the link of ${linkName(link) ?? link.host ?? link.id}.`, { ok: true });
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        this.context.onSignedOut();
        return;
      }
      this.say(`The link could not be copied: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  // The row menu.

  /**
   * The links a menu opened on `link` acts on: the selection shown, when
   * `link` is part of it, otherwise `link` alone.
   * @param {CollectedLink} link
   */
  menuTargets(link) {
    const chosen = this.chosen();
    return chosen.some((l) => l.id === link.id) ? chosen : [link];
  }

  /**
   * @param {CollectedLink} link
   * @param {Anchor} anchor
   */
  openMenu(link, anchor) {
    const targets = this.menuTargets(link);
    const n = targets.length;
    const many = n > 1 ? ` ${n} links` : '';
    const allowed = this.canSubmit && !this.busy;
    const changeable = this.canControl && !this.busy;
    const starts = targets.filter(startable);
    const pending = targets.filter((l) => !l.job);
    /** @type {MenuEntry[]} */
    const entries = [
      {
        key: 'start-links',
        label: n > 1 ? `Start${many}` : 'Start download',
        disabled: !allowed || starts.length === 0,
        hint: !this.canSubmit
          ? NO_SUBMIT
          : pending.length === 0
            ? 'Already in Downloads'
            : starts.length === 0
              ? 'Not a valid link'
              : undefined,
        run: () => void this.act('start', starts),
      },
      {
        key: 'move-links',
        label: `Save${many} to…`,
        disabled: !changeable || pending.length === 0,
        hint: !this.canControl ? NO_CONTROL : pending.length === 0 ? 'Already in Downloads' : undefined,
        run: () => this.chooseFolder(pending),
      },
    ];
    if (n === 1 && this.context.client.permissions.includes('sources')) {
      entries.push({ key: 'copy-link', label: 'Copy link', run: () => void this.copyLink(link) });
    }
    entries.push('separator', {
      key: 'remove-links',
      label: `Remove${many || ' link'} from list`,
      disabled: !changeable,
      hint: !this.canControl ? NO_CONTROL : 'Nothing is downloaded or deleted',
      danger: true,
      run: () => void this.act('remove', targets),
    });
    this.context.menu.show(entries, anchor, {
      owner: link.id,
      restore: () =>
        /** @type {HTMLElement | null} */ (this.table.querySelector(`[data-key="link-menu-${link.id}"]`))?.focus(),
    });
  }

  // Rendering.

  /** Marks the row whose menu is open, in place, so no row is redrawn. */
  markMenu() {
    const owner = this.context.menu.owner;
    for (const row of this.table.querySelectorAll('tr[data-link]')) {
      const open = row.getAttribute('data-link') === owner;
      row.classList.toggle('menu-open', open);
      row.querySelector('button.more')?.setAttribute('aria-expanded', String(open));
    }
  }

  render() {
    if (!this.shown) return;
    const focused = focusedKey(this.element);
    const visible = this.visible;
    const chosen = this.chosen();
    const allowed = this.canSubmit && !this.busy;
    const changeable = this.canControl && !this.busy;
    this.buttons.start.disabled = !allowed || !chosen.some(startable);
    this.buttons.move.disabled = !changeable || !chosen.some((l) => !l.job);
    this.buttons.remove.disabled = !changeable || chosen.length === 0;
    if (!this.canSubmit) this.buttons.start.title = NO_SUBMIT;
    if (!this.canControl) this.buttons.move.title = this.buttons.remove.title = NO_CONTROL;
    this.renderNotice();
    this.renderSummary(chosen);
    this.renderTable(visible);
    let empty = '';
    if (this.total === 0) {
      empty = 'No links collected. Use Add links to paste some; they wait here until you start them.';
    } else if (this.total !== null && visible.length === 0) {
      empty = 'No links on this page match the filter.';
    } else if (this.total === null && !this.notice.text) {
      empty = 'Loading…';
    }
    show(this.listStatus, empty);
    show(
      this.pager,
      this.earlier.length > 0 &&
        h('button', { type: 'button', 'data-key': 'links-page-back', onclick: () => this.page('back') }, 'Previous page'),
      this.nextAfter !== null &&
        h('button', { type: 'button', 'data-key': 'links-page-next', onclick: () => this.page('next') }, 'Next page'),
    );
    refocus(this.element, focused);
  }

  renderNotice() {
    const { text, ok, warn, downloads } = this.notice;
    this.noticeLine.classList.toggle('ok', ok);
    this.noticeLine.classList.toggle('warn', warn);
    this.noticeLine.hidden = text === '';
    this.noticeText.textContent = text;
    this.viewDownloads.hidden = !downloads;
  }

  /**
   * How many links are selected, where they will be saved, and what a
   * filter leaves out, so both are clear before starting.
   * @param {readonly CollectedLink[]} chosen
   */
  renderSummary(chosen) {
    const hidden = [...this.selected].filter((id) => !chosen.some((l) => l.id === id)).length;
    const hiddenNote = hidden > 0 && h('span', { class: 'warn-text' }, `${count(hidden, 'selected link')} hidden by the filter ${hidden === 1 ? 'is' : 'are'} left out.`);
    if (chosen.length === 0) {
      show(
        this.summary,
        h('span', null, this.total ? 'Select links to start them. Nothing downloads until you do.' : ''),
        hiddenNote,
      );
      return;
    }
    // Started links already have their folder; the rest say where they go.
    const pending = chosen.filter((l) => !l.job);
    const started = chosen.length - pending.length;
    const folders = [...new Set(pending.map((l) => this.linkFolder(l)))];
    const invalid = pending.filter((l) => l.check === 'invalid').length;
    const homeless = pending.filter((l) => l.check !== 'invalid' && this.removed(l.destination)).length;
    const where =
      folders.length === 1
        ? folders[0]
        : h('span', { title: folders.join('\n') }, `${folders.length} folders`);
    show(
      this.summary,
      h('strong', null, `${count(chosen.length, 'link')} selected`),
      folders.length > 0 && h('span', { class: 'label' }, 'Saving to'),
      folders.length > 0 && (typeof where === 'string' ? h('span', { class: 'path', title: where }, where) : where),
      !this.context.client.local && folders.length === 1 && h('span', { class: 'tag' }, 'Server path'),
      started > 0 && h('span', null, `${count(started, 'link is', 'links are')} already in Downloads.`),
      invalid > 0 && h('span', { class: 'problem' }, `${count(invalid, 'invalid link')} will not start.`),
      homeless > 0 &&
        h(
          'span',
          { class: 'problem' },
          `${count(homeless, 'link')} saving to a removed folder will not start; choose another with Save to.`,
        ),
      hiddenNote,
    );
  }

  /** @param {RootId | undefined} id */
  folderName(id) {
    if (id === undefined) return 'the download folder';
    return this.removed(id) ? 'A removed folder' : this.context.rootPath(id);
  }

  /**
   * Where `link` will be saved; a removed folder by the path it had.
   * @param {CollectedLink} link
   */
  linkFolder(link) {
    return (this.removed(link.destination) && link.destination_path) || this.folderName(link.destination);
  }

  /**
   * Whether links saving to `id` lost their folder: it was removed from
   * the permitted ones after they were collected. They stay, and start
   * once given another folder.
   * @param {RootId | undefined} id
   */
  removed(id) {
    const settings = this.context.settings();
    return id !== undefined && settings !== null && !settings.roots.some((r) => r.id === id);
  }

  /** @param {readonly CollectedLink[]} visible */
  renderTable(visible) {
    // Polling and settings reads often return the same table. Keep its
    // controls connected so a refresh cannot interrupt a checkbox click.
    // Only the current, bounded page is retained.
    const tableState = JSON.stringify([visible, [...this.selected], this.context.settings()?.roots, new Date().toDateString()]);
    if (tableState === this.tableState) return;
    this.tableState = tableState;
    const chosen = visible.filter((l) => this.selected.has(l.id)).length;
    this.selectAll.checked = visible.length > 0 && chosen === visible.length;
    this.selectAll.indeterminate = chosen > 0 && chosen < visible.length;
    this.selectAll.disabled = visible.length === 0;
    const head = h(
      'thead',
      null,
      h(
        'tr',
        null,
        h('th', { scope: 'col', class: 'check' }, this.selectAll),
        h('th', { scope: 'col', class: 'name' }, 'Name'),
        h('th', { scope: 'col', class: 'host' }, 'Host'),
        h('th', { scope: 'col', class: 'num' }, 'Size'),
        h('th', { scope: 'col', class: 'link-status' }, 'Status'),
        h('th', { scope: 'col', class: 'folder' }, 'Save to'),
        h('th', { scope: 'col', class: 'row-actions' }, h('span', { class: 'visually-hidden' }, 'Actions')),
      ),
    );
    show(
      this.table,
      h('caption', { class: 'visually-hidden' }, 'Collected links, by batch'),
      head,
      batches(visible).map((group) => this.batch(group)),
    );
  }

  /**
   * One batch: a heading row that selects its links, then their rows.
   * @param {{ batch: string, collectedAt: number, links: CollectedLink[] }} group
   */
  batch({ batch, collectedAt, links }) {
    const selected = links.filter((l) => this.selected.has(l.id)).length;
    const invalid = links.filter((l) => l.check === 'invalid').length;
    const label = `Added ${collectedWhen(collectedAt)}`;
    const box = h('input', {
      type: 'checkbox',
      'data-key': `select-${batch}`,
      'aria-label': `Select the ${count(links.length, 'link')} added ${collectedWhen(collectedAt)}`,
      checked: selected === links.length,
      onchange: (/** @type {Event} */ event) =>
        this.select(links, /** @type {HTMLInputElement} */ (event.currentTarget).checked),
    });
    box.indeterminate = selected > 0 && selected < links.length;
    return h(
      'tbody',
      { class: 'batch', 'data-batch': batch },
      h(
        'tr',
        { class: 'batch-head' },
        h('td', { class: 'check' }, box),
        h(
          'th',
          { scope: 'rowgroup', colspan: 6 },
          h('span', { class: 'batch-title' }, label),
          h('span', { class: 'batch-count' }, count(links.length, 'link')),
          invalid > 0 && h('span', { class: 'batch-count problem' }, `${invalid} invalid`),
        ),
      ),
      links.map((link) => this.row(link)),
    );
  }

  /** @param {CollectedLink} link */
  row(link) {
    const name = linkName(link);
    const status = linkStatus(link);
    const selected = this.selected.has(link.id);
    const shownName = name ?? link.host ?? 'Unknown name';
    const more = h(
      'button',
      {
        type: 'button',
        class: 'more',
        'data-key': `link-menu-${link.id}`,
        'aria-label': `Actions for ${shownName}`,
        'aria-haspopup': 'menu',
        'aria-expanded': String(this.context.menu.owner === link.id),
        onclick: (/** @type {MouseEvent} */ event) => {
          const button = /** @type {HTMLElement} */ (event.currentTarget);
          if (this.context.menu.justDismissed(link.id, button)) return;
          if (this.context.menu.owner === link.id) {
            this.context.menu.close(true);
            return;
          }
          this.openMenu(link, button.getBoundingClientRect());
        },
      },
      icon('more'),
    );
    const folder = this.linkFolder(link);
    const root = this.context.settings()?.roots.find((r) => r.id === link.destination);
    return h(
      'tr',
      {
        'data-link': link.id,
        class:
          [selected && 'selected', link.check === 'invalid' && 'invalid', link.job && 'started', this.context.menu.owner === link.id && 'menu-open']
            .filter(Boolean)
            .join(' ') || null,
        onclick: (/** @type {MouseEvent} */ event) => {
          if (/** @type {Element} */ (event.target).closest('a, button, input, label')) return;
          this.select([link], !selected);
        },
        oncontextmenu: (/** @type {MouseEvent} */ event) => {
          event.preventDefault();
          const anchor =
            event.button === 0 && event.clientX === 0 && event.clientY === 0
              ? more.getBoundingClientRect()
              : { left: event.clientX, right: event.clientX, top: event.clientY, bottom: event.clientY };
          this.openMenu(link, anchor);
        },
        onkeydown: (/** @type {KeyboardEvent} */ event) => {
          if (!(event.key === 'ContextMenu' || (event.key === 'F10' && event.shiftKey))) return;
          event.preventDefault();
          this.openMenu(link, more.getBoundingClientRect());
        },
      },
      h(
        'td',
        { class: 'check' },
        h('input', {
          type: 'checkbox',
          'data-key': `select-${link.id}`,
          'aria-label': `Select ${shownName}`,
          checked: selected,
          onchange: (/** @type {Event} */ event) =>
            this.select([link], /** @type {HTMLInputElement} */ (event.currentTarget).checked),
        }),
      ),
      h(
        'td',
        {
          class: name ? 'name' : 'name unknown',
          title: link.name_hint ? `${link.name_hint} (from the link; the server may name the file differently)` : name,
        },
        shownName,
      ),
      h('td', { class: 'host', title: link.host ?? null }, link.host ?? ''),
      h('td', { class: 'num unknown' }, 'Unknown'),
      h(
        'td',
        { class: `link-status tone-${status.tone}`, 'data-check': link.check, title: status.detail },
        h(
          'span',
          { class: 'status-label' },
          h('span', { class: 'dot', 'aria-hidden': 'true' }),
          link.job
            ? h(
                'a',
                {
                  href: `/jobs/${link.job}`,
                  'data-key': `link-job-${link.id}`,
                  onclick: (/** @type {MouseEvent} */ event) => {
                    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey) return;
                    event.preventDefault();
                    this.context.navigate(`/jobs/${link.job}`);
                  },
                },
                status.label,
              )
            : status.label,
        ),
        status.detail && h('span', { class: 'status-detail' }, status.detail),
      ),
      h(
        'td',
        { class: 'folder', title: folder },
        h('span', { class: 'path' }, folder),
        root && !root.available && h('span', { class: 'problem' }, ' (unavailable)'),
        this.removed(link.destination) && h('span', { class: 'problem' }, ' (removed; choose another)'),
      ),
      h('td', { class: 'row-actions' }, more),
    );
  }

  /** @param {'next' | 'back'} direction */
  page(direction) {
    if (direction === 'next') {
      if (this.nextAfter === null) return;
      this.earlier.push(this.pageAfter);
      this.pageAfter = this.nextAfter;
    } else {
      this.pageAfter = this.earlier.pop() ?? null;
    }
    this.selected.clear();
    this.say('');
    void this.refresh();
  }
}
