// Settings > About: the running version, and a check for a newer release
// on GitHub that runs only when asked.

import { WayfetchError } from '../client/contract.js';
import { h, show } from './dom.js';
import { icon } from './icons.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').UpdateCheck} UpdateCheck
 * @typedef {{
 *   client: WayfetchClient,
 *   hello: () => Hello | null,
 *   onSignedOut: () => void,
 * }} AboutContext
 */

/** Who makes Wayfetch, shown under About. */
export const AUTHOR = Object.freeze({ name: 'YAPhoa', url: 'https://github.com/YAPhoa' });

/**
 * How a check's result is shown: a status badge with its icon and tone,
 * a detail line if there is more to say, and the page to go to, if any.
 * @param {UpdateCheck} check
 * @returns {{
 *   badge: string,
 *   icon: import('./icons.js').IconName,
 *   tone: 'ok' | 'run' | 'bad',
 *   detail: string,
 *   link: { href: string, label: string } | null,
 * }}
 */
export function updateStatus(check) {
  const { outcome } = check;
  switch (outcome.kind) {
    case 'available':
      return {
        badge: `${outcome.version} available`,
        icon: 'up',
        tone: 'run',
        detail: '',
        link: { href: outcome.url, label: `Get ${outcome.version}` },
      };
    case 'up_to_date':
      return { badge: 'Up to date', icon: 'check', tone: 'ok', detail: '', link: null };
    case 'unavailable':
      return {
        badge: 'Check failed',
        icon: 'cancel',
        tone: 'bad',
        detail: `${outcome.problem}.`,
        link: { href: check.releases, label: 'All releases' },
      };
  }
}

/**
 * Why a check was refused, in plain words.
 * @param {WayfetchError} error
 */
function explain(error) {
  switch (error.code) {
    case 'overloaded':
      return 'Already checking. Try again shortly.';
    case 'unavailable':
      return 'The engine cannot be reached. Try again shortly.';
    default:
      return `${error.message.charAt(0).toUpperCase()}${error.message.slice(1)}.`;
  }
}

/**
 * A link that opens outside Wayfetch. The desktop app shows the address to
 * open in a browser instead, so its window never leaves Wayfetch.
 * @param {WayfetchClient} client
 * @param {string} href
 * @param {string} label
 * @param {Record<string, string>} [props]
 */
function outsideLink(client, href, label, props = {}) {
  if (client.local) return h('span', { ...props, class: 'address' }, href.replace(/^https:\/\//, ''));
  return h('a', { ...props, href, target: '_blank', rel: 'noopener noreferrer' }, label, icon('external'));
}

/** @param {AboutContext} context */
export function aboutSection(context) {
  const { client } = context;
  const badge = h('span', { class: 'update-status', role: 'status', 'data-key': 'update-status' });
  const detail = h('span', { class: 'update-detail', 'data-key': 'update-detail' });
  const action = h('span', { class: 'update-link' });
  const button = h(
    'button',
    { type: 'button', 'data-key': 'check-for-updates', onclick: () => void check() },
    icon('retry'),
    'Check for updates',
  );

  /**
   * Shows the badge, its detail line, and the link, if any.
   * @param {string} text
   * @param {import('./icons.js').IconName} name
   * @param {string} tone
   * @param {string} [more]
   * @param {{ href: string, label: string } | null} [link]
   */
  const showStatus = (text, name, tone, more = '', link = null) => {
    badge.className = `update-status tone-${tone}`;
    show(badge, icon(name), h('span', null, text));
    show(detail, more);
    show(action, ...(link ? [outsideLink(client, link.href, link.label, { 'data-key': 'update-link' })] : []));
  };

  const check = async () => {
    button.disabled = true;
    showStatus('Checking…', 'retry', 'calm');
    try {
      const status = updateStatus(await client.checkForUpdates());
      showStatus(status.badge, status.icon, status.tone, status.detail, status.link);
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        context.onSignedOut();
        return;
      }
      showStatus('Check failed', 'cancel', 'bad', error instanceof WayfetchError ? explain(error) : String(error));
    } finally {
      button.disabled = false;
    }
  };

  const version = h('span', { class: 'tag current', 'data-key': 'app-version' }, context.hello()?.version ?? '');
  // Settings opened straight from its address can come before the handshake.
  if (!version.textContent) {
    client.hello().then(
      (hello) => (version.textContent = hello.version),
      () => (version.textContent = 'Unknown'),
    );
  }
  /**
   * @param {string} name
   * @param {...(Node | string)} value
   */
  const row = (name, ...value) =>
    h('div', { class: 'setting-row' }, h('div', { class: 'setting-text' }, h('span', { class: 'setting-name' }, name)), ...value);
  return h(
    'div',
    { class: 'settings-section' },
    h(
      'div',
      { class: 'settings-group about' },
      h(
        'div',
        { class: 'group-rows' },
        row('Version', version),
        row('Author', outsideLink(client, AUTHOR.url, AUTHOR.name, { 'data-key': 'author' })),
        h(
          'div',
          { class: 'setting-row' },
          h(
            'div',
            { class: 'setting-text' },
            h('span', { class: 'setting-name' }, 'Updates'),
            h('span', { class: 'hint' }, 'Checks GitHub only when you ask.'),
            detail,
          ),
          h('div', { class: 'update-actions' }, badge, action, button),
        ),
      ),
    ),
  );
}
