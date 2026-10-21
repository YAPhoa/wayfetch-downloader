// The Plugins section of Settings: the programs Wayfetch runs for video
// links, such as yt-dlp, which reads YouTube's titles and formats, and
// ffmpeg, which joins a video with its sound and makes MP3 files. Each
// shows the version it reports and whether it can be used. Each row's
// menu turns its plugin off or on, checks for a newer official release of
// one Wayfetch can update and installs it, and removes a copy Wayfetch
// installed, going back to the copy included with Wayfetch, if any.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { pluginStatus, pluginUse } from '../client/media.js';
import { confirmDialog } from './dialog.js';
import { focusedKey, h, refocus, show } from './dom.js';
import { icon } from './icons.js';
import { Menu, rowMenuHandlers } from './menu.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').PluginSummary} PluginSummary
 * @typedef {import('../client/contract.js').PluginChange} PluginChange
 * @typedef {import('./menu.js').MenuEntry} MenuEntry
 * @typedef {{
 *   client: WayfetchClient,
 *   hello: () => Hello | null,
 *   onSignedOut: () => void,
 *   onChanged: (refresh: (revision: number | null) => void) => void,
 * }} PluginsContext
 */

/** The table's columns. */
export const PLUGIN_COLUMNS = ['Plugin', 'Version', 'Status', 'Used for'];

/**
 * What a refused or failed plugin change means.
 * @param {WayfetchError} error
 */
function explain(error) {
  switch (error.code) {
    case 'permission_denied':
      return 'You may not change plugins.';
    case 'unavailable':
      return 'The engine cannot be reached. Try again shortly.';
    default:
      return `${error.message.charAt(0).toUpperCase()}${error.message.slice(1)}.`;
  }
}

/**
 * What stops while a plugin with each role is turned off.
 * @type {Readonly<Record<PluginSummary['role'], string>>}
 */
const UNUSED = Object.freeze({
  media_resolver: 'Video links that need it are not checked or downloaded',
  media_converter: 'Formats that need it are not offered',
  archive_extractor: 'Downloaded archives are kept but not unpacked',
});

/**
 * What a change did, in plain words.
 * @param {PluginSummary} plugin
 * @param {PluginChange} change
 * @param {{ changed: boolean, version?: string }} result
 */
export function changeMessage(plugin, change, result) {
  const name = plugin.name;
  const version = result.version ? ` ${result.version}` : '';
  switch (change) {
    case 'update':
      return result.changed
        ? `Updated ${name} to${version}.`
        : `${name}${version} is the newest version.`;
    case 'enable':
      return `${name} is turned on.`;
    case 'disable':
      return `${name} is turned off. ${UNUSED[plugin.role]} until it is turned on.`;
    case 'remove':
      return result.version
        ? `Removed the copy Wayfetch installed. ${name}${version} is used again.`
        : `Removed ${name}.`;
  }
}

/**
 * What a plugin's status says while each change is on its way.
 * @type {Readonly<Record<PluginChange, string>>}
 */
const DOING = Object.freeze({
  enable: 'Turning on…',
  disable: 'Turning off…',
  update: 'Checking…',
  remove: 'Removing…',
});

/**
 * @param {PluginsContext} context
 */
export function pluginsSection(context) {
  const { client } = context;
  const section = h('div', { class: 'settings-section' });
  const message = h('p', { class: 'message', role: 'status', 'data-key': 'plugins-message' });
  const canChange = client.permissions.includes('control');
  /** The plugin with a change on its way, if any. @type {string | null} */
  let working = null;
  /** What its status says meanwhile. */
  let doing = '';
  /** @type {PluginSummary[]} */
  let shown = [];
  /** The settings revision last read. */
  let revision = 0;
  const markMenu = () => {
    for (const row of section.querySelectorAll('tr[data-plugin]')) {
      const open = row.getAttribute('data-plugin') === menu.owner;
      row.classList.toggle('menu-open', open);
      row.querySelector('button.more')?.setAttribute('aria-expanded', String(open));
    }
  };
  const menu = new Menu('Plugin actions', markMenu);

  /**
   * Shows `error`; an ended session goes to the shell.
   * @param {unknown} error
   */
  const failed = (error) => {
    if (error instanceof WayfetchError && error.code === 'unauthenticated') {
      context.onSignedOut();
      return;
    }
    show(message, error instanceof WayfetchError ? explain(error) : String(error));
  };

  /**
   * Reads the plugins again, saying `said`.
   * @param {string} said
   */
  const reload = async (said = '') => {
    try {
      const settings = await client.settings();
      revision = settings.revision;
      // A check that finds nothing new leaves an open menu open.
      if (!said && shown.length > 0 && JSON.stringify(settings.plugins) === JSON.stringify(shown)) return;
      render(settings.plugins, said);
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        context.onSignedOut();
        return;
      }
      if (shown.length > 0) {
        failed(error);
        return;
      }
      show(section, h('p', { class: 'message', role: 'alert' }, error instanceof WayfetchError ? error.message : String(error)));
    }
  };

  /**
   * Makes `change` to `plugin`.
   * @param {PluginSummary} plugin
   * @param {PluginChange} change
   */
  const run = async (plugin, change) => {
    const hello = context.hello();
    if (!hello) {
      show(message, 'Not connected yet. Try again in a moment.');
      return;
    }
    working = plugin.id;
    doing = DOING[change];
    render(shown, change === 'update' ? `Checking for a newer ${plugin.name}…` : 'Working…');
    try {
      const result = await client.changePlugin({
        idempotencyKey: idempotencyKey(),
        target: hello.instance,
        plugin: plugin.id,
        change,
      });
      working = null;
      const changed = result.type === 'plugin_changed' ? result : { changed: true };
      await reload(changeMessage(plugin, change, changed));
    } catch (error) {
      working = null;
      render(shown);
      failed(error);
    }
  };

  /**
   * Asks before removing the copy of `plugin` Wayfetch installed.
   * @param {PluginSummary} plugin
   */
  const confirmRemove = (plugin) => {
    const after = plugin.fallback_version
      ? `Wayfetch goes back to ${plugin.name} ${plugin.fallback_version}, the copy it found before.`
      : `No other copy of ${plugin.name} was found, so what needs it stops working until it is updated again.`;
    confirmDialog(
      { title: `Remove ${plugin.name}`, confirm: 'Remove', onConfirm: () => void run(plugin, 'remove') },
      `Delete the copy of ${plugin.name} that Wayfetch installed? ${after}`,
    );
  };

  /**
   * What can be done with `plugin`; what does not apply now stays in the
   * menu, greyed out.
   * @param {PluginSummary} plugin
   * @returns {MenuEntry[]}
   */
  const entries = (plugin) => {
    const busy = working !== null;
    return [
      ...(plugin.updatable
        ? /** @type {MenuEntry[]} */ ([
            {
              key: 'update-plugin',
              label: plugin.state === 'missing' && !plugin.installed ? 'Install' : 'Check for update',
              disabled: busy,
              run: () => void run(plugin, 'update'),
            },
            'separator',
          ])
        : []),
      {
        key: 'toggle-plugin',
        label: plugin.enabled ? 'Turn off' : 'Turn on',
        disabled: busy,
        run: () => void run(plugin, plugin.enabled ? 'disable' : 'enable'),
      },
      {
        key: 'remove-plugin',
        label: plugin.fallback_version ? 'Remove update…' : 'Remove…',
        danger: true,
        disabled: busy || !plugin.installed,
        run: () => confirmRemove(plugin),
      },
    ];
  };

  /**
   * One plugin's row.
   * @param {PluginSummary} plugin
   */
  const row = (plugin) => {
    const id = plugin.id;
    const [status, tone] = working === id ? [doing, 'calm'] : pluginStatus(plugin);
    /** @param {import('./menu.js').Anchor} anchor */
    const open = (anchor) =>
      menu.show(entries(plugin), anchor, {
        owner: id,
        restore: () => /** @type {HTMLElement | null} */ (section.querySelector(`[data-key="plugin-menu-${id}"]`))?.focus(),
      });
    const more =
      canChange &&
      h(
        'button',
        {
          type: 'button',
          class: 'more',
          'data-key': `plugin-menu-${id}`,
          'aria-label': `Actions for ${plugin.name}`,
          'aria-haspopup': 'menu',
          'aria-expanded': 'false',
          onclick: (/** @type {MouseEvent} */ event) =>
            menu.toggle(id, /** @type {HTMLElement} */ (event.currentTarget), open),
        },
        icon('more'),
      );
    return h(
      'tr',
      { 'data-plugin': id, ...(more ? rowMenuHandlers(more, open) : {}) },
      h('td', { class: 'name' }, plugin.name),
      h(
        'td',
        { class: plugin.version ? 'version' : 'version unknown' },
        plugin.version ?? 'Unknown',
        plugin.installed && h('span', { class: 'tag', 'data-key': `plugin-installed-${id}` }, 'Updated'),
      ),
      h(
        'td',
        { class: `plugin-status tone-${tone}` },
        h('span', { class: 'status-label' }, h('span', { class: 'dot', 'aria-hidden': 'true' }), status),
        working !== id && plugin.problem && h('span', { class: 'status-detail' }, plugin.problem),
      ),
      h('td', { class: 'use' }, pluginUse(plugin)),
      canChange && h('td', { class: 'row-actions' }, more),
    );
  };

  /**
   * The table for `plugins`, saying `said`.
   * @param {readonly PluginSummary[]} plugins
   * @param {string} said
   */
  const render = (plugins, said = '') => {
    const focused = focusedKey(section);
    menu.close(false);
    shown = [...plugins];
    show(message, said);
    const columns = PLUGIN_COLUMNS.length + (canChange ? 1 : 0);
    const rows =
      plugins.length === 0
        ? [h('tr', null, h('td', { class: 'empty', colspan: columns, 'data-key': 'no-plugins' }, 'This Wayfetch runs no plugins.'))]
        : plugins.map(row);
    const unusable = plugins.some((p) => p.state !== 'ready' || !p.enabled);
    show(
      section,
      h(
        'div',
        { class: 'settings-group plugins' },
        h(
          'div',
          { class: 'table-scroll' },
          h(
            'table',
            { class: 'accounts-table plugins-table', 'aria-label': 'Plugins' },
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                PLUGIN_COLUMNS.map((name) => h('th', { scope: 'col' }, name)),
                canChange && h('th', { scope: 'col' }, h('span', { class: 'visually-hidden' }, 'Actions')),
              ),
            ),
            h('tbody', null, rows),
          ),
        ),
        h('div', { class: 'group-foot' }, message),
      ),
      h(
        'p',
        { class: 'hint' },
        (unusable ? 'What a plugin is used for is unavailable while it is turned off, missing, or not working. ' : '') +
          'Updates come only from the plugin’s official releases and are installed after their signature is checked. ' +
          'A plugin included with Wayfetch can be turned off; a copy Wayfetch installed can be removed.',
      ),
      menu.element,
    );
    refocus(section, focused);
  };

  show(section, h('p', { class: 'message', role: 'status' }, 'Loading…'));
  void reload();
  context.onChanged((changed) => {
    if (working === null && (changed === null || changed > revision)) void reload();
  });
  return section;
}
