// The Settings page: how many downloads run at once, where new downloads
// are saved, accounts with download services, and how the list looks.
// Only what Wayfetch supports today can be changed here. A menu of
// sections sits beside the selected section, as in a desktop app's
// preferences.
//
// Downloads and Download folders are the engine's settings, shared by
// every client: each is saved with Save at the revision shown, within the
// limits of the server's configuration, which the page explains. A draft
// survives a failed save and a change made elsewhere, which every client
// hears of; a section with an unsaved draft is marked, and leaving
// Settings asks first. In a browser the folders are on the server, among
// those its operator allowed; the desktop app can also add a folder
// through its native dialog. A folder the user added can be removed
// again, which only stops offering it: its files stay. Theme and row
// density are this device's own and apply at once.
//
// Routes: `/settings` shows the first section; `/settings/<section>` shows
// that one. The page is part of the Downloads workspace, which keeps
// following the engine while it is shown.

import { theme } from '../theme.js';
import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { openDialog } from './dialog.js';
import { focusedKey, h, refocus, show } from './dom.js';
import { browseServerFolders } from './folders.js';
import { icon } from './icons.js';
import { accountsSection } from './accounts.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').Settings} Settings
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {import('../client/contract.js').RootSummary} RootSummary
 * @typedef {import('../client/contract.js').BoundedSetting} BoundedSetting
 * @typedef {import('../client/contract.js').PreferenceChange} PreferenceChange
 * @typedef {'compact' | 'comfortable'} Density
 * @typedef {'downloads' | 'folder' | 'display' | 'accounts'} Section
 */

/**
 * What the Settings page works with.
 * @typedef {{
 *   client: WayfetchClient,
 *   hello: () => Hello | null,
 *   density: () => Density,
 *   setDensity: (density: Density) => void,
 *   onSettings: (settings: Settings) => void,
 *   onSignedOut: () => void,
 *   navigate: (path: string) => void,
 *   back: () => void,
 *   backLabel: () => string,
 *   brand: Node,
 * }} SettingsContext
 */

/**
 * The sections, in menu order.
 * @type {{ id: Section, label: string, about: string, icon: 'brand' | 'folder' | 'display' | 'accounts' }[]}
 */
export const SECTIONS = [
  { id: 'downloads', label: 'Downloads', about: 'How downloads run.', icon: 'brand' },
  { id: 'folder', label: 'Download folders', about: 'Where new downloads are saved.', icon: 'folder' },
  {
    id: 'accounts',
    label: 'Accounts & services',
    about: 'Download services you can use with an account, such as multi-host services.',
    icon: 'accounts',
  },
  { id: 'display', label: 'Display', about: 'Appearance and downloads list layout.', icon: 'display' },
];

/** The path of Settings, which shows its first section. */
export const SETTINGS_PATH = '/settings';

/**
 * The path of `section`.
 * @param {Section} section
 */
export function sectionPath(section) {
  return `${SETTINGS_PATH}/${section}`;
}

/**
 * Which section `path` shows: `null` outside Settings, the first section
 * for `/settings` or a section that does not exist.
 * @param {string} path
 * @returns {Section | null}
 */
export function settingsSection(path) {
  if (path !== SETTINGS_PATH && !path.startsWith(`${SETTINGS_PATH}/`)) return null;
  const id = path.slice(SETTINGS_PATH.length + 1);
  return SECTIONS.find((section) => section.id === id)?.id ?? SECTIONS[0].id;
}

/**
 * Why settings were not saved, in plain words.
 * @param {WayfetchError} error
 */
function notSaved(error) {
  switch (error.code) {
    case 'revision_conflict':
      return 'The settings were changed elsewhere and have been reloaded. Choose again and save.';
    case 'not_found':
      return 'That folder is no longer a permitted destination.';
    case 'permission_denied':
      return 'You may not change settings.';
    case 'unavailable':
      return 'The engine cannot be reached. Try again shortly.';
    default:
      return `Not saved: ${error.message}`;
  }
}

/**
 * Shows `error` with `say`; an ended session goes to the shell instead.
 * @param {SettingsContext} context
 * @param {unknown} error
 * @param {(text: string) => void} say
 * @param {(error: WayfetchError) => string} [explain]
 */
function failure(context, error, say, explain = notSaved) {
  if (error instanceof WayfetchError && error.code === 'unauthenticated') {
    context.onSignedOut();
    return;
  }
  say(error instanceof WayfetchError ? explain(error) : String(error));
}

/**
 * What the server's configuration and the user's choice mean for an
 * engine preference: the range it allows, where its value comes from, and
 * any conflict with a saved value.
 * @param {BoundedSetting} setting
 * @param {boolean} local whether the engine runs in this app
 */
export function describeSetting(setting, local) {
  if (setting.locked) {
    const managed = local ? 'Managed by this app’s configuration.' : 'Managed by server configuration.';
    return setting.conflict ? `${managed} Your saved value ${setting.saved} is not used.` : managed;
  }
  const range = `Allowed: ${setting.min}–${setting.max}.`;
  if (setting.conflict) {
    return `${range} Your saved value ${setting.saved} is outside it, so ${setting.value} is used.`;
  }
  switch (setting.source) {
    case 'saved':
      return `${range} Saved; the default is ${setting.default}.`;
    case 'server':
      return `${range} Using the ${local ? 'configured' : 'server’s'} default.`;
    default:
      return `${range} Using the default.`;
  }
}

/**
 * The whole number typed for `setting`, or why it cannot be saved.
 * @param {string} text
 * @param {BoundedSetting} setting
 * @returns {{ value: number, error: null } | { value: null, error: string }}
 */
export function parseCount(text, setting) {
  const range = `Enter a whole number from ${setting.min} to ${setting.max}.`;
  const trimmed = text.trim();
  if (!/^\d+$/.test(trimmed)) return { value: null, error: range };
  const value = Number(trimmed);
  if (!Number.isSafeInteger(value) || value < setting.min || value > setting.max) {
    return { value: null, error: range };
  }
  return { value, error: null };
}

/**
 * Why a folder was not removed, in plain words.
 * @param {WayfetchError} error
 */
function notRemoved(error) {
  if (error.code !== 'not_applicable') return notSaved(error);
  return `Not removed: ${error.message}.`;
}

export class SettingsPage {
  /** @param {SettingsContext} context */
  constructor(context) {
    this.context = context;
    /** @type {Section} */
    this.section = SECTIONS[0].id;
    /** Whether the page is shown; each showing reads the settings afresh. */
    this.shown = false;
    /** Sections with changes not saved yet. */
    /** @type {Set<Section>} */
    this.drafts = new Set();
    /** What each engine section does when the settings change elsewhere. */
    /** @type {((revision: number | null) => void)[]} */
    this.refreshers = [];
    this.onBeforeUnload = (/** @type {BeforeUnloadEvent} */ event) => {
      if (this.drafts.size > 0) event.preventDefault();
    };
    this.backLabel = h('span', null, 'Back');
    this.back = h(
      'button',
      { type: 'button', class: 'tool back', 'data-key': 'settings-back', onclick: () => this.leave() },
      icon('back'),
      this.backLabel,
    );
    this.title = h('h1', { tabindex: -1, 'data-key': 'settings-title' }, 'Settings');
    this.links = SECTIONS.map(({ id, label, icon: name }) =>
      h(
        'a',
        {
          href: sectionPath(id),
          class: 'settings-link',
          'data-key': `settings-section-${id}`,
          'data-section': id,
          onclick: (/** @type {MouseEvent} */ event) => {
            // A new tab or window opens the link as it is.
            if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
            event.preventDefault();
            context.navigate(sectionPath(id));
          },
        },
        icon(name),
        h('span', null, label),
        h('span', { class: 'unsaved', hidden: true }, h('span', { class: 'visually-hidden' }, ', unsaved changes')),
      ),
    );
    this.nav = h(
      'nav',
      { class: 'settings-nav', 'aria-label': 'Settings sections', onkeydown: (/** @type {KeyboardEvent} */ e) => this.key(e) },
      h('ul', null, this.links.map((link) => h('li', null, link))),
    );
    this.content = h('main', { class: 'settings-content' });
    this.element = h(
      'div',
      {
        class: 'settings-page',
        hidden: true,
        onkeydown: (/** @type {KeyboardEvent} */ event) => {
          if (event.key === 'Escape' && !event.defaultPrevented) {
            event.preventDefault();
            this.leave();
          }
        },
      },
      h(
        'header',
        { class: 'toolbar' },
        context.brand,
        this.back,
        this.title,
      ),
      h('div', { class: 'settings-body' }, this.nav, this.content),
    );
  }

  /**
   * Shows `section`. Coming from Downloads, the settings are read afresh
   * and focus moves to the page's title; between sections it stays put.
   * @param {Section} section
   */
  open(section) {
    const arriving = !this.shown;
    this.shown = true;
    this.element.hidden = false;
    if (arriving) {
      this.backLabel.textContent = this.context.backLabel();
      window.addEventListener('beforeunload', this.onBeforeUnload);
      this.build();
    }
    this.section = section;
    for (const link of this.links) {
      if (link.dataset.section === section) link.setAttribute('aria-current', 'page');
      else link.removeAttribute('aria-current');
    }
    for (const panel of this.content.children) {
      /** @type {HTMLElement} */ (panel).hidden = /** @type {HTMLElement} */ (panel).dataset.section !== section;
    }
    if (arriving) this.title.focus({ preventScroll: true });
  }

  /** Hides the page; unsaved choices are dropped. */
  close() {
    this.shown = false;
    this.element.hidden = true;
    this.content.replaceChildren();
    this.refreshers = [];
    this.drafts.clear();
    for (const link of this.links) /** @type {HTMLElement} */ (link.querySelector('.unsaved')).hidden = true;
    window.removeEventListener('beforeunload', this.onBeforeUnload);
  }

  /** Goes back, after asking whether to drop changes not saved yet. */
  leave() {
    if (this.drafts.size === 0) {
      this.context.back();
      return;
    }
    const names = SECTIONS.filter(({ id }) => this.drafts.has(id)).map(({ label }) => label);
    const keep = h('button', { type: 'button', class: 'primary', onclick: () => dialog.close() }, 'Keep editing');
    const discard = h(
      'button',
      {
        type: 'button',
        onclick: () => {
          dialog.close();
          this.context.back();
        },
      },
      'Discard changes',
    );
    const dialog = openDialog(
      { title: 'Discard unsaved changes?' },
      h('p', null, `${names.join(' and ')} ${names.length === 1 ? 'has' : 'have'} changes that are not saved.`),
      h('div', { class: 'row' }, keep, discard),
    );
    keep.focus();
  }

  /**
   * Marks `section` as holding unsaved changes, or not.
   * @param {Section} section
   * @param {boolean} unsaved
   */
  draft(section, unsaved) {
    if (unsaved) this.drafts.add(section);
    else this.drafts.delete(section);
    const link = this.links.find((l) => l.dataset.section === section);
    const mark = /** @type {HTMLElement | undefined} */ (link?.querySelector('.unsaved'));
    if (mark) mark.hidden = !unsaved;
  }

  /**
   * The settings changed, to `revision`, or may have while the engine was
   * out of reach (`null`): sections not showing it read them again,
   * keeping drafts.
   * @param {number | null} revision
   */
  changed(revision) {
    if (!this.shown) return;
    for (const refresh of this.refreshers) refresh(revision);
  }

  /**
   * Up and down arrows, Home and End move between the sections' links.
   * @param {KeyboardEvent} event
   */
  key(event) {
    const at = this.links.indexOf(/** @type {HTMLAnchorElement} */ (document.activeElement));
    if (at < 0) return;
    const last = this.links.length - 1;
    /** @type {Record<string, number>} */
    const to = { ArrowDown: at === last ? 0 : at + 1, ArrowUp: at === 0 ? last : at - 1, Home: 0, End: last };
    if (!(event.key in to)) return;
    event.preventDefault();
    this.links[to[event.key]].focus();
  }

  /** Builds each section's panel. */
  build() {
    const panels = {
      downloads: this.downloadsSection(),
      folder: this.destinationSection(),
      display: this.displaySection(),
      accounts: accountsSection({
        client: this.context.client,
        hello: this.context.hello,
        onSignedOut: this.context.onSignedOut,
      }),
    };
    show(
      this.content,
      SECTIONS.map(({ id, label, about }) =>
        h(
          'section',
          { class: 'settings-panel', 'data-section': id, 'aria-labelledby': `settings-${id}`, hidden: true },
          h('header', { class: 'settings-head' }, h('h2', { id: `settings-${id}` }, label), h('p', { class: 'hint' }, about)),
          panels[id],
        ),
      ),
    );
  }

  /**
   * The engine's download preferences: how many downloads run at once,
   * within what the server's configuration allows.
   */
  downloadsSection() {
    const { context } = this;
    const { client } = context;
    const section = h('div', { class: 'settings-section' });
    show(section, h('p', { class: 'message', role: 'status' }, 'Loading…'));
    const canChange = client.permissions.includes('control');
    /** The settings the form shows, as last read. */
    /** @type {Settings | null} */
    let current = null;
    /** Whether a save is under way. */
    let saving = false;

    const input = h('input', {
      type: 'number',
      id: 'max-active-downloads',
      class: 'count',
      'data-key': 'max-active',
      inputmode: 'numeric',
      step: 1,
      required: true,
      'aria-describedby': 'max-active-about max-active-policy max-active-problem',
      oninput: () => update(),
    });
    const policy = h('span', { class: 'hint', id: 'max-active-policy' });
    const problem = h('span', { class: 'problem', id: 'max-active-problem' });
    const message = h('p', { class: 'message', role: 'status' });
    const useDefault = h(
      'button',
      { type: 'button', 'data-key': 'downloads-default', onclick: () => void save('use_default') },
      'Use default',
    );
    const cancel = h(
      'button',
      {
        type: 'button',
        'data-key': 'downloads-cancel',
        onclick: () => {
          if (!current) return;
          input.value = String(current.max_active_downloads.value);
          update();
          show(message, 'Changes discarded.');
          input.focus();
        },
      },
      'Cancel',
    );
    const saveButton = h('button', { type: 'submit', class: 'primary', 'data-key': 'downloads-save' }, 'Save');
    const form = h(
      'form',
      {
        class: 'downloads-form',
        novalidate: true,
        onsubmit: (/** @type {SubmitEvent} */ event) => {
          event.preventDefault();
          if (!current) return;
          const { value } = parseCount(input.value, current.max_active_downloads);
          if (value !== null) void save({ set: value });
        },
      },
      h(
        'fieldset',
        { class: 'settings-group' },
        h(
          'legend',
          { class: 'group-head' },
          h('span', { class: 'group-title' }, 'Transfers'),
          h('span', { class: 'group-note' }, client.local ? 'This app’s downloads' : 'Applies to the server'),
        ),
        h(
          'div',
          { class: 'group-rows' },
          h(
            'div',
            { class: 'setting-row' },
            h(
              'div',
              { class: 'setting-text' },
              h('label', { class: 'setting-name', for: 'max-active-downloads' }, 'Maximum simultaneous downloads'),
              h(
                'span',
                { class: 'hint', id: 'max-active-about' },
                'More wait in the queue. Lowering it never stops a download already running.',
              ),
              policy,
              problem,
            ),
            input,
          ),
        ),
        h('div', { class: 'group-foot' }, message, useDefault, cancel, saveButton),
      ),
      h(
        'p',
        { class: 'hint' },
        client.local
          ? 'Saved in this app, for every window.'
          : 'Saved on the server, for every browser and app connected to it.',
      ),
    );

    /** The value typed, whether it differs from what applies, and whether it can be saved. */
    const draft = () => {
      const setting = /** @type {Settings} */ (current).max_active_downloads;
      const parsed = parseCount(input.value, setting);
      return { parsed, changed: input.value.trim() !== String(setting.value) };
    };

    /** Enables what can be done now, and says what is wrong with the draft. */
    const update = () => {
      if (!current) return;
      const setting = current.max_active_downloads;
      const editable = canChange && !setting.locked;
      const { parsed, changed } = draft();
      const invalid = changed && parsed.error !== null;
      show(problem, invalid ? parsed.error : '');
      input.setAttribute('aria-invalid', String(invalid));
      // Typing is held while a save is under way, so its answer never
      // replaces a newer edit.
      input.disabled = !editable || saving;
      saveButton.hidden = !editable;
      cancel.hidden = !editable;
      saveButton.disabled = saving || !changed || invalid;
      cancel.disabled = saving || !changed;
      useDefault.hidden = !canChange;
      useDefault.disabled = saving || setting.saved === undefined;
      this.draft('downloads', changed);
    };

    /**
     * Shows `settings`, keeping a changed draft if `keep`. An answer older
     * than the settings shown, which arrived late, only reports `said`.
     * @param {Settings} settings
     * @param {string} said
     * @param {boolean} [keep]
     */
    const apply = (settings, said, keep = false) => {
      if (current !== null && settings.revision < current.revision) {
        show(message, said);
        update();
        return;
      }
      context.onSettings(settings);
      const kept = keep && current !== null && draft().changed;
      current = settings;
      const setting = settings.max_active_downloads;
      input.min = String(setting.min);
      input.max = String(setting.max);
      if (!kept) input.value = String(setting.value);
      show(policy, describeSetting(setting, client.local), !canChange && ' You may not change settings.');
      show(message, said);
      update();
      if (!section.contains(form)) show(section, form);
    };

    /** @param {PreferenceChange} change */
    const save = async (change) => {
      const hello = context.hello();
      if (!hello || !current) {
        show(message, 'Not connected yet. Try again in a moment.');
        return;
      }
      const working = focusedKey(section);
      saving = true;
      update();
      show(message, 'Saving…');
      try {
        await client.updateSettings({
          idempotencyKey: idempotencyKey(),
          target: hello.instance,
          expectedRevision: current.revision,
          maxActiveDownloads: change,
        });
        const saved = await client.settings();
        saving = false;
        const count = saved.max_active_downloads.value;
        apply(
          saved,
          change === 'use_default'
            ? `Using the default: ${count} at once.`
            : `Saved. Up to ${count} downloads run at once.`,
        );
      } catch (error) {
        saving = false;
        if (error instanceof WayfetchError && error.code === 'revision_conflict') {
          await reload(
            'The settings were changed elsewhere and have been reloaded. Your value is kept; save again to apply it.',
          );
        } else {
          failure(context, error, (text) => show(message, text));
          update();
        }
      }
      refocus(section, working);
    };

    /**
     * Reads the settings again, keeping a changed draft.
     * @param {string} said
     */
    const reload = async (said = '') => {
      try {
        const settings = await client.settings();
        const changed = current !== null && draft().changed;
        apply(
          settings,
          said || (changed ? 'The settings were changed elsewhere. Your unsaved value is kept.' : ''),
          true,
        );
      } catch (error) {
        failure(context, error, (text) => {
          if (current) {
            show(message, `Settings could not be loaded. ${text}`);
            return;
          }
          show(
            section,
            h('p', { class: 'message error', role: 'alert' }, `Settings could not be loaded. ${text}`),
            h('button', { type: 'button', onclick: () => void reload() }, 'Try again'),
          );
        });
      }
    };
    void reload();
    this.refreshers.push((revision) => {
      if (!saving && (revision === null || current === null || current.revision < revision)) void reload();
    });
    return section;
  }

  /** The default destination, read from the engine. */
  destinationSection() {
    const { context } = this;
    const { client } = context;
    const destination = h('div', { class: 'settings-section' });
    show(destination, h('p', { class: 'message', role: 'status' }, 'Loading…'));
    /** What had focus before a control was disabled while it works. */
    /** @type {string | null} */
    let working = null;
    /** Whether a save, removal, or addition is under way. */
    let busy = false;
    /** The settings the form shows. */
    /** @type {Settings | null} */
    let shown = null;
    /** The folder checked, if any. */
    const checked = () =>
      /** @type {RootId | undefined} */ (
        /** @type {HTMLInputElement | null} */ (destination.querySelector('input[name="default-root"]:checked'))?.value
      );
    const track = () => this.draft('folder', shown !== null && checked() !== undefined && checked() !== shown.default_root);

    /**
     * The destination form for `settings`, keeping `selected` checked if
     * it is still a choice, and saying `said`.
     * @param {Settings} settings
     * @param {RootId | null | undefined} selected
     * @param {string} said
     */
    const render = (settings, selected = settings.default_root, said = '') => {
      context.onSettings(settings);
      shown = settings;
      const canChange = client.permissions.includes('control');
      const message = h('p', { class: 'message', role: 'status' }, said);
      const choices = settings.roots.map((root) =>
        h(
          'div',
          { class: 'choice-row' },
          h(
            'label',
            { class: 'choice' },
            h('input', {
              type: 'radio',
              name: 'default-root',
              value: root.id,
              'data-key': `default-root-${root.id}`,
              checked: root.id === selected,
              disabled: !canChange || (!root.available && root.id !== selected),
            }),
            h(
              'span',
              { class: 'choice-text' },
              icon('folder'),
              h('span', { class: 'path' }, root.path),
              root.id === settings.default_root && h('span', { class: 'tag current' }, 'Default'),
              !root.available &&
                h('span', { class: 'problem' }, `Unavailable: ${root.problem ?? 'the folder cannot be opened'}`),
            ),
          ),
          // Only a folder the user added, and not the one new downloads go
          // to; the operator's folders stay.
          canChange &&
            root.added &&
            root.id !== settings.default_root &&
            h(
              'button',
              {
                type: 'button',
                class: 'remove-root',
                'data-key': `remove-root-${root.id}`,
                'aria-label': `Remove ${root.path}`,
                onclick: (/** @type {MouseEvent} */ event) =>
                  void remove(settings, root, selected, /** @type {HTMLButtonElement} */ (event.currentTarget), message),
              },
              'Remove',
            ),
        ),
      );
      const save = h(
        'button',
        { type: 'submit', class: 'primary', 'data-key': 'settings-save', disabled: !canChange },
        'Save',
      );
      // The desktop app adds a folder through its own dialog; a browser
      // lists the server's folders.
      const { chooseDownloadFolder, browseFolders, addFolder } = client;
      /** @type {[string, () => Promise<RootSummary | null>] | null} */
      const adding = chooseDownloadFolder
        ? ['Choose folder…', () => chooseDownloadFolder.call(client)]
        : browseFolders && addFolder
          ? [
              'Add folder…',
              () =>
                browseServerFolders(
                  { browseFolders: browseFolders.bind(client), addFolder: addFolder.bind(client) },
                  settings.roots.find((r) => r.id === selected)?.path ?? null,
                ),
            ]
          : null;
      const choose =
        canChange &&
        adding &&
        h(
          'button',
          {
            type: 'button',
            'data-key': 'settings-choose',
            onclick: async (/** @type {MouseEvent} */ event) => {
              const button = /** @type {HTMLButtonElement} */ (event.currentTarget);
              working = focusedKey(destination);
              button.disabled = true;
              busy = true;
              show(message, '');
              try {
                const root = await adding[1]();
                if (root === null) {
                  show(message, 'No folder was chosen; nothing changed.');
                  return;
                }
                render(await client.settings(), root.id, 'Folder added. Save to use it for new downloads.');
              } catch (error) {
                failure(context, error, (text) => show(message, text));
              } finally {
                busy = false;
                button.disabled = false;
                if (button.isConnected) refocus(destination, working);
                working = null;
              }
            },
          },
          adding[0],
        );
      const form = h(
        'form',
        {
          class: 'destination-form',
          onchange: track,
          onsubmit: async (/** @type {SubmitEvent} */ event) => {
            event.preventDefault();
            const hello = context.hello();
            const picked = /** @type {HTMLInputElement | null} */ (
              form.querySelector('input[name="default-root"]:checked')
            );
            if (!hello) {
              show(message, 'Not connected yet. Try again in a moment.');
              return;
            }
            if (!picked) {
              show(message, 'Choose a folder first.');
              return;
            }
            const root = /** @type {RootId} */ (picked.value);
            working = focusedKey(destination);
            save.disabled = true;
            busy = true;
            show(message, 'Saving…');
            try {
              await client.updateSettings({
                idempotencyKey: idempotencyKey(),
                target: hello.instance,
                expectedRevision: settings.revision,
                defaultRoot: root,
              });
              const saved = await client.settings();
              busy = false;
              const path = saved.roots.find((r) => r.id === saved.default_root)?.path;
              render(saved, saved.default_root, `Saved. New downloads go to ${path}.`);
            } catch (error) {
              busy = false;
              if (error instanceof WayfetchError && error.code === 'revision_conflict') {
                await reload(notSaved(error), root);
                return;
              }
              failure(context, error, (text) => show(message, text));
              save.disabled = false;
              refocus(destination, working);
              working = null;
            }
          },
        },
        h(
          'fieldset',
          { class: 'settings-group' },
          h(
            'legend',
            { class: 'group-head' },
            h('span', { class: 'group-title' }, 'Save new downloads to'),
            h('span', { class: 'group-note' }, client.local ? 'Folders on this computer' : 'Server paths'),
          ),
          choices.length > 0
            ? h('div', { class: 'group-rows' }, choices)
            : h('p', { class: 'group-empty' }, 'There is no permitted folder.'),
          h('div', { class: 'group-foot' }, message, choose, save),
        ),
        h(
          'p',
          { class: 'hint' },
          'Downloads already added keep their folder.',
          client.local && ' Choose any folder on this computer to add it.',
        ),
      );
      // Keyboard focus stays on the same control, such as Save, or moves
      // from a disabled one to the next enabled one.
      const focused = focusedKey(destination) ?? working;
      working = null;
      show(destination, form);
      track();
      refocus(destination, focused);
      if (focused !== null && !destination.contains(document.activeElement)) {
        /** @type {HTMLElement | null} */ (form.querySelector('input:checked:enabled, button:enabled'))?.focus();
      }
    };

    /**
     * Stops offering `root`, a folder the user added, and shows the folders
     * left, keeping `selected` checked if it is still one of them.
     * @param {Settings} settings
     * @param {RootSummary} root
     * @param {RootId | null | undefined} selected
     * @param {HTMLButtonElement} button
     * @param {HTMLElement} message
     */
    const remove = async (settings, root, selected, button, message) => {
      const hello = context.hello();
      if (!hello) {
        show(message, 'Not connected yet. Try again in a moment.');
        return;
      }
      working = focusedKey(destination);
      button.disabled = true;
      busy = true;
      show(message, 'Removing…');
      try {
        await client.removeRoot({
          idempotencyKey: idempotencyKey(),
          target: hello.instance,
          expectedRevision: settings.revision,
          root: root.id,
        });
        const left = await client.settings();
        busy = false;
        render(
          left,
          selected === root.id ? undefined : selected,
          `Removed ${root.path}. Its files stay there; add it again to save new downloads there.`,
        );
      } catch (error) {
        if (error instanceof WayfetchError && error.code === 'revision_conflict') {
          await reload('The settings were changed elsewhere and have been reloaded. Try again.', selected ?? undefined);
          return;
        }
        busy = false;
        failure(context, error, (text) => show(message, text), notRemoved);
        button.disabled = false;
        refocus(destination, working);
        working = null;
      }
    };

    /**
     * Reads the settings again, keeping `selected` checked if it is still
     * a choice.
     * @param {string} said
     * @param {RootId | undefined} [selected]
     */
    const reload = async (said = '', selected = undefined) => {
      busy = false;
      try {
        const settings = await client.settings();
        render(settings, settings.roots.some((r) => r.id === selected) ? selected : undefined, said);
      } catch (error) {
        failure(context, error, (text) =>
          show(
            destination,
            h('p', { class: 'message error', role: 'alert' }, `Settings could not be loaded. ${text}`),
            h('button', { type: 'button', onclick: () => void reload() }, 'Try again'),
          ),
        );
      }
    };
    void reload();
    // A change elsewhere keeps the folder checked here, saved or not.
    this.refreshers.push((revision) => {
      if (busy || shown === null || (revision !== null && shown.revision >= revision)) return;
      void reload('', checked());
    });
    return destination;
  }

  /** Theme and row density, applied at once and kept on this device. */
  displaySection() {
    const { context } = this;
    return h(
      'div',
      { class: 'settings-section' },
      h(
        'div',
        { class: 'settings-group' },
        h('div', { class: 'group-head' }, h('span', { class: 'group-title' }, 'Appearance')),
        h(
          'div',
          { class: 'group-rows' },
          settingRow(
            'theme',
            'Theme',
            'System follows your device’s appearance.',
            /** @type {[import('../theme.js').Theme, string][]} */ ([
              ['system', 'System'],
              ['light', 'Light'],
              ['dark', 'Dark'],
            ]),
            theme?.get() ?? 'system',
            (value) => theme?.set(value),
          ),
          settingRow(
            'density',
            'Row density',
            'Compact fits more downloads on screen.',
            /** @type {[Density, string][]} */ ([
              ['compact', 'Compact'],
              ['comfortable', 'Comfortable'],
            ]),
            context.density(),
            (value) => context.setDensity(value),
          ),
        ),
      ),
      h('p', { class: 'hint' }, 'Applies at once, on this device only.'),
    );
  }
}

/**
 * One setting in a group: its name and what it does, and a segmented
 * choice of `options` with `current` picked.
 * @template {string} T
 * @param {string} name
 * @param {string} label
 * @param {string} about
 * @param {[T, string][]} options
 * @param {T} current
 * @param {(value: T) => void} change
 */
function settingRow(name, label, about, options, current, change) {
  return h(
    'div',
    { class: 'setting-row' },
    h(
      'div',
      { class: 'setting-text' },
      h('span', { class: 'setting-name', id: `setting-${name}` }, label),
      h('span', { class: 'hint' }, about),
    ),
    h(
      'div',
      { class: 'segmented', role: 'radiogroup', 'aria-labelledby': `setting-${name}` },
      options.map(([value, text]) =>
        h(
          'label',
          null,
          h('input', { type: 'radio', name, value, checked: current === value, onchange: () => change(value) }),
          h('span', null, text),
        ),
      ),
    ),
  );
}
