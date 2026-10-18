// The Plugins section of Settings: the programs Wayfetch runs for video
// links, such as yt-dlp, which reads YouTube's titles and formats, and
// ffmpeg, which joins a video with its sound and makes MP3 files. Each
// shows the version it reports and whether it can be used. The engine
// checks them when it starts, so a plugin installed or fixed since shows
// after a restart.

import { WayfetchError } from '../client/contract.js';
import { pluginStatus, pluginUse } from '../client/media.js';
import { h, show } from './dom.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').PluginSummary} PluginSummary
 */

/** The table's columns. */
export const PLUGIN_COLUMNS = ['Plugin', 'Version', 'Status', 'Used for'];

/**
 * @param {{ client: WayfetchClient, onSignedOut: () => void }} context
 */
export function pluginsSection(context) {
  const section = h('div', { class: 'settings-section' });
  show(section, h('p', { class: 'message', role: 'status' }, 'Loading…'));
  void (async () => {
    let plugins;
    try {
      plugins = (await context.client.settings()).plugins;
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        context.onSignedOut();
        return;
      }
      show(section, h('p', { class: 'message', role: 'alert' }, error instanceof WayfetchError ? error.message : String(error)));
      return;
    }
    show(section, pluginsTable(plugins));
  })();
  return section;
}

/**
 * The plugins' table, and what a missing one means.
 * @param {readonly PluginSummary[]} plugins
 */
export function pluginsTable(plugins) {
  const rows =
    plugins.length === 0
      ? [h('tr', null, h('td', { class: 'empty', colspan: PLUGIN_COLUMNS.length, 'data-key': 'no-plugins' }, 'This Wayfetch runs no plugins.'))]
      : plugins.map((plugin) => {
          const [status, tone] = pluginStatus(plugin);
          return h(
            'tr',
            { 'data-plugin': plugin.id },
            h('td', { class: 'name' }, plugin.name),
            h('td', { class: plugin.version ? 'version' : 'version unknown' }, plugin.version ?? 'Unknown'),
            h(
              'td',
              { class: `status tone-${tone}` },
              h('span', null, status),
              plugin.problem && h('span', { class: 'status-detail' }, plugin.problem),
            ),
            h('td', { class: 'use' }, pluginUse(plugin)),
          );
        });
  const missing = plugins.some((p) => p.state !== 'ready');
  return [
    h(
      'div',
      { class: 'settings-group plugins' },
      h('div', { class: 'group-head' }, h('span', { class: 'group-title' }, 'Plugins')),
      h(
        'div',
        { class: 'table-scroll' },
        h(
          'table',
          { class: 'accounts-table plugins-table', 'aria-label': 'Plugins' },
          h('thead', null, h('tr', null, PLUGIN_COLUMNS.map((name) => h('th', { scope: 'col' }, name)))),
          h('tbody', null, rows),
        ),
      ),
    ),
    h(
      'p',
      { class: 'hint' },
      'Video links, such as YouTube’s, are read by yt-dlp; Wayfetch then downloads them itself, with the same speed limit and resume as any download. ' +
        (missing
          ? 'Without yt-dlp, video links cannot be checked or downloaded; without ffmpeg, only formats that need no joining or converting are offered. '
          : '') +
        'Plugins are checked when Wayfetch starts.',
    ),
  ];
}
