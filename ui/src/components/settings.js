// The Settings dialog: where new downloads are saved, how dense the list
// is, and, for later, accounts with download services. Only what Wayfetch
// supports today can be changed here.
//
// The default destination is the engine's setting, saved at the revision
// shown. In a browser the folders are on the server, among those its
// operator allowed; the desktop app can also add a folder through its
// native dialog. Row density is this device's own preference.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { openDialog } from './dialog.js';
import { h, show } from './dom.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').Settings} Settings
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {'compact' | 'comfortable'} Density
 */

/**
 * What the Settings dialog works with.
 * @typedef {{
 *   client: WayfetchClient,
 *   hello: () => Hello | null,
 *   density: () => Density,
 *   setDensity: (density: Density) => void,
 *   onSettings: (settings: Settings) => void,
 *   onSignedOut: () => void,
 * }} SettingsContext
 */

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
 * Opens the Settings dialog.
 * @param {SettingsContext} context
 */
export function openSettings(context) {
  const { client } = context;
  const destination = h('section', { class: 'settings-section', 'aria-labelledby': 'settings-destination' });
  const density = densitySection(context);
  const accounts = accountsSection();
  const dialog = openDialog(
    { title: 'Settings', className: 'settings', closeLabel: 'Close settings' },
    destination,
    density,
    accounts,
  );
  show(
    destination,
    h('h3', { id: 'settings-destination' }, 'Download folder'),
    h('p', { class: 'message', role: 'status' }, 'Loading…'),
  );

  /**
   * The destination form for `settings`, keeping `selected` checked if it
   * is still a choice, and saying `said`.
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
        'label',
        { class: 'choice' },
        h('input', {
          type: 'radio',
          name: 'default-root',
          value: root.id,
          checked: root.id === selected,
          disabled: !canChange || (!root.available && root.id !== selected),
        }),
        h(
          'span',
          { class: 'choice-text' },
          h('span', { class: 'path' }, root.path),
          !client.local && h('span', { class: 'tag' }, 'Server path'),
          root.id === settings.default_root && h('span', { class: 'tag current' }, 'Current'),
          !root.available &&
            h('span', { class: 'problem' }, `Unavailable: ${root.problem ?? 'the folder cannot be opened'}`),
        ),
      ),
    );
    const save = h('button', { type: 'submit', class: 'primary', disabled: !canChange }, 'Save');
    const choose =
      canChange &&
      client.chooseDownloadFolder &&
      h(
        'button',
        {
          type: 'button',
          onclick: async (/** @type {MouseEvent} */ event) => {
            const button = /** @type {HTMLButtonElement} */ (event.currentTarget);
            const picker = /** @type {() => Promise<import('../client/contract.js').RootSummary | null>} */ (
              client.chooseDownloadFolder
            );
            button.disabled = true;
            show(message, '');
            try {
              const root = await picker.call(client);
              if (root === null) {
                show(message, 'No folder was chosen; nothing changed.');
                return;
              }
              render(await client.settings(), root.id, 'Folder added. Save to use it for new downloads.');
            } catch (error) {
              failed(error, (text) => show(message, text));
            } finally {
              button.disabled = false;
            }
          },
        },
        'Choose folder…',
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
          }
        },
      },
      h(
        'fieldset',
        null,
        h('legend', null, 'Save new downloads to'),
        choices.length > 0 ? choices : h('p', { class: 'hint' }, 'There is no permitted folder.'),
      ),
      h('p', { class: 'hint' }, 'Downloads already added keep their folder.'),
      client.local
        ? h('p', { class: 'hint' }, 'Choose any folder on this computer to add it.')
        : h(
            'p',
            { class: 'hint' },
            'These folders are on the server. Only folders its operator allowed are listed; the operator adds one with ',
            h('code', null, 'wayfetchd init-root'),
            ' and ',
            h('code', null, '--root'),
            '.',
          ),
      h('div', { class: 'row' }, save, choose, message),
    );
    show(destination, h('h3', { id: 'settings-destination' }, 'Download folder'), form);
  };

  /**
   * Shows `error` with `say`; an ended session goes to the shell.
   * @param {unknown} error
   * @param {(text: string) => void} say
   */
  const failed = (error, say) => {
    if (error instanceof WayfetchError && error.code === 'unauthenticated') {
      dialog.close();
      context.onSignedOut();
      return;
    }
    say(error instanceof WayfetchError ? notSaved(error) : String(error));
  };

  /** @param {string} said */
  const reload = async (said = '') => {
    try {
      render(await client.settings(), undefined, said);
    } catch (error) {
      failed(error, (text) =>
        show(
          destination,
          h('h3', { id: 'settings-destination' }, 'Download folder'),
          h('p', { class: 'message error', role: 'alert' }, `Settings could not be loaded. ${text}`),
          h('button', { type: 'button', onclick: () => void reload() }, 'Try again'),
        ),
      );
    }
  };
  void reload();
  return dialog;
}

/**
 * Row density, applied at once and kept on this device.
 * @param {SettingsContext} context
 */
function densitySection(context) {
  /** @type {[Density, string][]} */
  const options = [
    ['compact', 'Compact'],
    ['comfortable', 'Comfortable'],
  ];
  return h(
    'section',
    { class: 'settings-section', 'aria-labelledby': 'settings-display' },
    h('h3', { id: 'settings-display' }, 'Display'),
    h(
      'fieldset',
      null,
      h('legend', null, 'Row density'),
      options.map(([value, label]) =>
        h(
          'label',
          { class: 'choice' },
          h('input', {
            type: 'radio',
            name: 'density',
            value,
            checked: context.density() === value,
            onchange: () => context.setDensity(value),
          }),
          h('span', { class: 'choice-text' }, label),
        ),
      ),
    ),
    h('p', { class: 'hint' }, 'Applies at once, on this device only.'),
  );
}

/**
 * Accounts with download services: none can be connected yet, and the
 * section says so rather than offering anything to fill in.
 */
function accountsSection() {
  return h(
    'section',
    { class: 'settings-section', 'aria-labelledby': 'settings-accounts' },
    h(
      'h3',
      { id: 'settings-accounts' },
      'Accounts & services',
      h('span', { class: 'tag' }, 'Not available yet'),
    ),
    h(
      'div',
      { class: 'empty-state' },
      h('p', null, 'No accounts are connected.'),
      h(
        'p',
        null,
        'This is where you will connect supported download services, such as Real-Debrid, once Wayfetch supports them. ' +
          'Connecting accounts is not available yet, so Wayfetch downloads only the links you add directly.',
      ),
    ),
  );
}
