// Settings > About: the running version, and a check for a newer release
// on GitHub that runs only when asked.

import { WayfetchError } from '../client/contract.js';
import { h, show } from './dom.js';

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

/**
 * What a check found, in plain words, and the page to go to, if any.
 * @param {UpdateCheck} check
 * @returns {{ text: string, tone: '' | 'error', link: { href: string, label: string } | null }}
 */
export function updateMessage(check) {
  const { outcome } = check;
  switch (outcome.kind) {
    case 'available':
      return {
        text: `Version ${outcome.version} is available.`,
        tone: '',
        link: { href: outcome.url, label: 'View release' },
      };
    case 'up_to_date':
      return { text: 'Wayfetch is up to date.', tone: '', link: null };
    case 'unavailable':
      return {
        text: `Could not check for updates: ${outcome.problem}.`,
        tone: 'error',
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
      return `Could not check for updates: ${error.message}.`;
  }
}

/** @param {AboutContext} context */
export function aboutSection(context) {
  const { client } = context;
  const message = h('p', { class: 'message', role: 'status', 'data-key': 'update-message' });
  const button = h(
    'button',
    { type: 'button', 'data-key': 'check-for-updates', onclick: () => void check() },
    'Check for updates',
  );

  /**
   * Shows `text`, then the link to `href`. The desktop app shows the
   * address to open in a browser, so its window never leaves Wayfetch.
   * @param {string} text
   * @param {'' | 'error'} tone
   * @param {{ href: string, label: string } | null} [link]
   */
  const say = (text, tone, link = null) => {
    message.className = tone ? `message ${tone}` : 'message';
    if (!link) {
      show(message, text);
    } else if (client.local) {
      show(message, `${text} Open `, h('span', { class: 'address', 'data-key': 'update-link' }, link.href), ' in your browser.');
    } else {
      const anchor = h('a', { href: link.href, target: '_blank', rel: 'noopener noreferrer', 'data-key': 'update-link' }, link.label);
      show(message, `${text} `, anchor);
    }
  };

  const check = async () => {
    button.disabled = true;
    say('Checking…', '');
    try {
      const { text, tone, link } = updateMessage(await client.checkForUpdates());
      say(text, tone, link);
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        context.onSignedOut();
        return;
      }
      say(error instanceof WayfetchError ? explain(error) : String(error), 'error');
    } finally {
      button.disabled = false;
    }
  };

  const version = h('span', { 'data-key': 'app-version' }, context.hello()?.version ?? '');
  // Settings opened straight from its address can come before the handshake.
  if (!version.textContent) {
    client.hello().then(
      (hello) => (version.textContent = hello.version),
      () => (version.textContent = 'Unknown'),
    );
  }
  return h(
    'div',
    { class: 'settings-section' },
    h(
      'div',
      { class: 'settings-group' },
      h('div', { class: 'group-head' }, h('span', { class: 'group-title' }, 'Wayfetch Downloader')),
      h(
        'div',
        { class: 'group-rows' },
        h(
          'div',
          { class: 'setting-row' },
          h('div', { class: 'setting-text' }, h('span', { class: 'setting-name' }, 'Version')),
          version,
        ),
        h(
          'div',
          { class: 'setting-row' },
          h(
            'div',
            { class: 'setting-text' },
            h('span', { class: 'setting-name' }, 'Updates'),
            h('span', { class: 'hint' }, 'Asks GitHub for the newest release. Nothing is sent until you check.'),
          ),
          button,
        ),
      ),
      h('div', { class: 'group-foot' }, message),
    ),
  );
}
