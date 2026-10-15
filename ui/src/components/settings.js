// The Settings page: where new downloads are saved, how dense the list
// is, and accounts with download services. Only what Wayfetch supports
// today can be changed here. A menu of sections sits beside the
// selected section, as in a desktop app's preferences.
//
// The default destination is the engine's setting, saved at the revision
// shown. In a browser the folders are on the server, among those its
// operator allowed; the desktop app can also add a folder through its
// native dialog. A folder the user added can be removed again, which only
// stops offering it: its files stay. Row density is this device's own
// preference.
//
// Routes: `/settings` shows the first section; `/settings/<section>` shows
// that one. The page is part of the Downloads workspace, which keeps
// following the engine while it is shown.

import { theme } from '../theme.js';
import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { focusedKey, h, refocus, show } from './dom.js';
import { browseServerFolders } from './folders.js';
import { icon } from './icons.js';
import { pickService } from './services.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').Settings} Settings
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {import('../client/contract.js').RootSummary} RootSummary
 * @typedef {'compact' | 'comfortable'} Density
 * @typedef {'folder' | 'display' | 'accounts'} Section
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
 * @type {{ id: Section, label: string, about: string, icon: 'folder' | 'display' | 'accounts' }[]}
 */
export const SECTIONS = [
  { id: 'folder', label: 'Download folder', about: 'Where new downloads are saved.', icon: 'folder' },
  { id: 'display', label: 'Display', about: 'Appearance and downloads list layout.', icon: 'display' },
  {
    id: 'accounts',
    label: 'Accounts & services',
    about: 'Download services you can use with an account, such as multi-host services.',
    icon: 'accounts',
  },
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
    this.backLabel = h('span', null, 'Back');
    this.back = h(
      'button',
      { type: 'button', class: 'tool back', 'data-key': 'settings-back', onclick: () => context.back() },
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
            context.back();
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
      folder: this.destinationSection(),
      display: this.displaySection(),
      accounts: accountsSection(),
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

  /** The default destination, read from the engine. */
  destinationSection() {
    const { context } = this;
    const { client } = context;
    const destination = h('div', { class: 'settings-section' });
    show(destination, h('p', { class: 'message', role: 'status' }, 'Loading…'));
    /** What had focus before a control was disabled while it works. */
    /** @type {string | null} */
    let working = null;

    /**
     * The destination form for `settings`, keeping `selected` checked if
     * it is still a choice, and saying `said`.
     * @param {Settings} settings
     * @param {RootId | null | undefined} selected
     * @param {string} said
     */
    const render = (settings, selected = settings.default_root, said = '') => {
      context.onSettings(settings);
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
              show(message, '');
              try {
                const root = await adding[1]();
                if (root === null) {
                  show(message, 'No folder was chosen; nothing changed.');
                  return;
                }
                render(await client.settings(), root.id, 'Folder added. Save to use it for new downloads.');
              } catch (error) {
                failed(error, (text) => show(message, text));
              } finally {
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
            show(message, 'Saving…');
            try {
              await client.updateSettings({
                idempotencyKey: idempotencyKey(),
                target: hello.instance,
                expectedRevision: settings.revision,
                defaultRoot: root,
              });
              const saved = await client.settings();
              const path = saved.roots.find((r) => r.id === saved.default_root)?.path;
              render(saved, saved.default_root, `Saved. New downloads go to ${path}.`);
            } catch (error) {
              if (error instanceof WayfetchError && error.code === 'revision_conflict') {
                await reload(notSaved(error));
                return;
              }
              failed(error, (text) => show(message, text));
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
      show(message, 'Removing…');
      try {
        await client.removeRoot({
          idempotencyKey: idempotencyKey(),
          target: hello.instance,
          expectedRevision: settings.revision,
          root: root.id,
        });
        render(
          await client.settings(),
          selected === root.id ? undefined : selected,
          `Removed ${root.path}. Its files stay there; add it again to save new downloads there.`,
        );
      } catch (error) {
        if (error instanceof WayfetchError && error.code === 'revision_conflict') {
          await reload('The settings were changed elsewhere and have been reloaded. Try again.');
          return;
        }
        failed(error, (text) => show(message, text), notRemoved);
        button.disabled = false;
        refocus(destination, working);
        working = null;
      }
    };

    /**
     * Shows `error` with `say`; an ended session goes to the shell.
     * @param {unknown} error
     * @param {(text: string) => void} say
     * @param {(error: WayfetchError) => string} explain
     */
    const failed = (error, say, explain = notSaved) => {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        context.onSignedOut();
        return;
      }
      say(error instanceof WayfetchError ? explain(error) : String(error));
    };

    /** @param {string} said */
    const reload = async (said = '') => {
      try {
        render(await client.settings(), undefined, said);
      } catch (error) {
        failed(error, (text) =>
          show(
            destination,
            h('p', { class: 'message error', role: 'alert' }, `Settings could not be loaded. ${text}`),
            h('button', { type: 'button', onclick: () => void reload() }, 'Try again'),
          ),
        );
      }
    };
    void reload();
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

/** The columns of the accounts table. */
const ACCOUNT_COLUMNS = ['Service', 'Account', 'Status', 'Expires', 'Traffic left'];

/**
 * Accounts with download services: a table of the accounts added, none
 * yet, and a way to add one by picking its service from the catalogue.
 * Nothing here asks for credentials until an integration exists to use
 * them.
 */
function accountsSection() {
  // Integrations the engine reports; it has none yet, so no account can
  // be added.
  /** @type {ReadonlySet<string>} */
  const integrations = new Set();
  return h(
    'div',
    { class: 'settings-section' },
    h(
      'div',
      { class: 'settings-group accounts' },
      h(
        'div',
        { class: 'group-head' },
        h('span', { class: 'group-title' }, 'Accounts'),
        h(
          'button',
          { type: 'button', 'data-key': 'add-account', onclick: () => pickService(integrations) },
          icon('add'),
          'Add account…',
        ),
      ),
      h(
        'div',
        { class: 'table-scroll' },
        h(
          'table',
          { class: 'accounts-table', 'aria-label': 'Accounts' },
          h('thead', null, h('tr', null, ACCOUNT_COLUMNS.map((name) => h('th', { scope: 'col' }, name)))),
          h(
            'tbody',
            null,
            h(
              'tr',
              null,
              h(
                'td',
                { class: 'empty', colspan: ACCOUNT_COLUMNS.length, 'data-key': 'no-accounts' },
                'No accounts added yet.',
              ),
            ),
          ),
        ),
      ),
    ),
    h(
      'p',
      { class: 'hint' },
      'Links you add directly download without an account. To add one, pick its service from the list.',
    ),
  );
}
