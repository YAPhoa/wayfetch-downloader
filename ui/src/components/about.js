// Settings > About: the running version, its author, and updates. While
// Check automatically is on, the engine checks GitHub daily; the reload
// button checks now. Nothing is downloaded until asked: the desktop app
// then downloads the newer release in the background and offers to
// restart into it.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { addedWhen } from '../client/links.js';
import { h, show } from './dom.js';
import { icon } from './icons.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').Settings} Settings
 * @typedef {import('../client/contract.js').UpdateCheck} UpdateCheck
 * @typedef {{
 *   client: WayfetchClient,
 *   hello: () => Hello | null,
 *   onSignedOut: () => void,
 *   onSettings: (settings: Settings) => void,
 *   onChanged: (refresh: () => void) => void,
 *   onUpdates: (check: UpdateCheck) => void,
 * }} AboutContext
 * @typedef {{
 *   badge: string,
 *   icon: import('./icons.js').IconName,
 *   tone: 'ok' | 'run' | 'bad',
 *   detail: string,
 *   link: { href: string, label: string } | null,
 *   command: 'download' | 'restart' | null,
 *   busy: boolean,
 * }} UpdateStatus
 */

/** Who makes Wayfetch, shown under About. */
export const AUTHOR = Object.freeze({ name: 'YAPhoa', url: 'https://github.com/YAPhoa' });

/** How often a download or installation in progress is read again, in ms. */
const PROGRESS_EVERY = 1000;

/**
 * `text` as a sentence: capitalized, with a full stop.
 * @param {string} text
 */
const sentence = (text) => `${text.charAt(0).toUpperCase()}${text.slice(1)}.`;

/**
 * How a check's result is shown: a status badge with its icon and tone,
 * a detail line if there is more to say, the page to go to, if any, and
 * what the person can do about it in this app: download the newer
 * release, or restart into it. `null` before
 * the first check.
 * @param {UpdateCheck} check
 * @returns {UpdateStatus | null}
 */
export function updateStatus(check) {
  const { outcome, install } = check;
  /** @type {UpdateStatus} */
  const plain = { badge: '', icon: 'check', tone: 'ok', detail: '', link: null, command: null, busy: false };
  switch (outcome.kind) {
    case 'not_checked':
      return null;
    case 'up_to_date':
      return { ...plain, badge: 'Up to date' };
    case 'unavailable':
      return {
        ...plain,
        badge: 'Failed',
        icon: 'error',
        tone: 'bad',
        detail: sentence(outcome.problem),
        link: { href: check.releases, label: 'All releases' },
      };
  }
  const get = { href: outcome.url, label: `Get ${outcome.version}` };
  const available = { ...plain, badge: `${outcome.version} available`, icon: /** @type {const} */ ('arrow-up'), tone: /** @type {const} */ ('run') };
  switch (install?.state) {
    case undefined:
      return { ...available, link: get };
    case 'idle':
      return { ...available, command: 'download' };
    case 'downloading': {
      const share = install.total ? ` ${Math.floor((install.received / install.total) * 100)}%` : '…';
      return { ...plain, badge: `Downloading${share}`, icon: 'arrow-down', tone: 'run', busy: true };
    }
    case 'ready':
      return { ...plain, badge: `${outcome.version} ready`, icon: 'arrow-up', command: 'restart' };
    case 'installing':
      return { ...plain, badge: 'Installing…', icon: 'more', tone: 'run', busy: true };
    case 'failed':
      return {
        ...plain,
        badge: 'Failed',
        icon: 'error',
        tone: 'bad',
        detail: sentence(install.problem),
        command: 'download',
      };
  }
}

/**
 * When the last check ran, for the status line.
 * @param {UpdateCheck} check
 */
export function lastChecked(check) {
  return check.checked_at === undefined ? 'Not checked yet' : `Last checked ${addedWhen(check.checked_at)}`;
}

/**
 * Why a request was refused, in plain words.
 * @param {WayfetchError} error
 */
function explain(error) {
  switch (error.code) {
    case 'overloaded':
      return 'Already checking. Try again shortly.';
    case 'unavailable':
      return 'The engine cannot be reached. Try again shortly.';
    case 'permission_denied':
      return 'You may not change settings.';
    case 'revision_conflict':
      return 'The settings were changed elsewhere. Try again.';
    default:
      return sentence(error.message);
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
  return h('a', { ...props, href, target: '_blank', rel: 'noopener noreferrer' }, label, icon('external-link'));
}

/** @param {AboutContext} context */
export function aboutSection(context) {
  const { client } = context;
  const badge = h('span', { class: 'update-status', role: 'status', 'data-key': 'update-status' });
  // One status line under the name, so a result never moves the row.
  const detail = h('span', { class: 'hint update-detail', 'data-key': 'update-detail' });
  const action = h('span', { class: 'update-link' });
  const button = h(
    'button',
    {
      type: 'button',
      class: 'icon-button',
      'aria-label': 'Check for updates',
      title: 'Check for updates',
      'data-key': 'check-for-updates',
      onclick: () => void check(),
    },
    icon('refresh'),
  );
  const download = h(
    'button',
    { type: 'button', class: 'primary', 'data-key': 'download-update', onclick: () => void downloadUpdate() },
    'Download update',
  );
  const restart = h(
    'button',
    { type: 'button', class: 'primary', 'data-key': 'restart-to-update', onclick: () => void restartToUpdate() },
    'Restart to update',
  );
  /** @type {UpdateCheck | null} */
  let last = null;
  /** @type {ReturnType<typeof setTimeout> | undefined} */
  let polling;

  /**
   * Shows the badge, its detail line, and the link and command, if any.
   * @param {UpdateStatus | null} status
   * @param {string} [problem] why the last request failed, which wins
   */
  const showStatus = (status, problem = '') => {
    if (status) {
      badge.className = `update-status tone-${status.tone}`;
      show(badge, icon(status.icon), h('span', null, status.badge));
    } else {
      show(badge);
    }
    const more = problem || status?.detail || '';
    show(detail, more || (last ? lastChecked(last) : ''));
    detail.classList.toggle('problem', Boolean(more));
    detail.title = more;
    const link = status?.link;
    const command =
      (status?.command === 'download' && client.downloadUpdate && download) ||
      (status?.command === 'restart' && client.restartToUpdate && restart);
    show(
      action,
      ...(link ? [outsideLink(client, link.href, link.label, { 'data-key': 'update-link' })] : []),
      ...(command ? [command] : []),
    );
  };

  /**
   * Shows `check`, and reads it again while a download or installation
   * runs.
   * @param {UpdateCheck} check
   */
  const showCheck = (check) => {
    last = check;
    context.onUpdates(check);
    const status = updateStatus(check);
    showStatus(status);
    clearTimeout(polling);
    if (status?.busy && section.isConnected) polling = setTimeout(() => void read(), PROGRESS_EVERY);
  };

  /**
   * Handles a refused request: an ended session goes to the shell.
   * @param {unknown} error
   */
  const failed = (error) => {
    if (error instanceof WayfetchError && error.code === 'unauthenticated') {
      context.onSignedOut();
      return;
    }
    showStatus({ ...FAILED }, error instanceof WayfetchError ? explain(error) : String(error));
  };

  const read = async () => {
    try {
      showCheck(await client.updates());
    } catch (error) {
      // Not worth a badge: the reload button says more.
      if (error instanceof WayfetchError && error.code === 'unauthenticated') context.onSignedOut();
    }
  };

  const check = async () => {
    button.disabled = true;
    button.classList.add('busy');
    showStatus({ ...CHECKING });
    try {
      showCheck(await client.checkForUpdates());
    } catch (error) {
      failed(error);
    } finally {
      button.disabled = false;
      button.classList.remove('busy');
    }
  };

  const downloadUpdate = async () => {
    download.disabled = true;
    try {
      await client.downloadUpdate?.();
      await read();
    } catch (error) {
      failed(error);
    } finally {
      download.disabled = false;
    }
  };

  const restartToUpdate = async () => {
    restart.disabled = true;
    try {
      // The app stops and starts again once it is installed.
      await client.restartToUpdate?.();
      showStatus({ ...INSTALLING });
    } catch (error) {
      failed(error);
    } finally {
      restart.disabled = false;
    }
  };

  // Automatic checks, saved at once at the revision shown.
  /** @type {Settings | null} */
  let settings = null;
  const automatic = /** @type {[boolean, string][]} */ ([
    [false, 'Off'],
    [true, 'On'],
  ]).map(([value, text]) =>
    h(
      'label',
      null,
      h('input', {
        type: 'radio',
        name: 'automatic-update-checks',
        value: String(value),
        'data-key': `automatic-update-checks-${value ? 'on' : 'off'}`,
        disabled: true,
        onchange: () => void setAutomatic(value),
      }),
      h('span', null, text),
    ),
  );
  const inputs = automatic.map((label) => /** @type {HTMLInputElement} */ (label.querySelector('input')));
  const automaticNote = h('span', { class: 'hint', 'data-key': 'automatic-update-checks-note' });
  /** @param {Settings} saved */
  const showSettings = (saved) => {
    settings = saved;
    inputs[0].checked = !saved.automatic_update_checks;
    inputs[1].checked = saved.automatic_update_checks;
    for (const input of inputs) input.disabled = false;
    show(automaticNote, 'Daily');
  };
  const readSettings = async () => {
    try {
      showSettings(await client.settings());
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') context.onSignedOut();
    }
  };
  /** @param {boolean} on */
  const setAutomatic = async (on) => {
    const hello = context.hello() ?? (await client.hello().catch(() => null));
    if (!hello || !settings) return;
    for (const input of inputs) input.disabled = true;
    try {
      await client.updateSettings({
        idempotencyKey: idempotencyKey(),
        target: hello.instance,
        expectedRevision: settings.revision,
        automaticUpdateChecks: on,
      });
      const saved = await client.settings();
      context.onSettings(saved);
      showSettings(saved);
      // Turning it on checks at once.
      if (on) setTimeout(() => void read(), PROGRESS_EVERY);
    } catch (error) {
      failed(error);
      await readSettings();
    }
  };
  context.onChanged(() => void readSettings());

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
  const section = h(
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
          h('div', { class: 'setting-text' }, h('span', { class: 'setting-name' }, 'Updates'), detail),
          h('div', { class: 'update-actions' }, action, badge, button),
        ),
        h(
          'div',
          { class: 'setting-row' },
          h(
            'div',
            { class: 'setting-text' },
            h('span', { class: 'setting-name', id: 'automatic-update-checks-name' }, 'Check automatically'),
            automaticNote,
          ),
          h('div', { class: 'segmented', role: 'radiogroup', 'aria-labelledby': 'automatic-update-checks-name' }, automatic),
        ),
      ),
    ),
  );
  void read();
  void readSettings();
  return section;
}

/** @type {UpdateStatus} */
const FAILED = { badge: 'Failed', icon: 'error', tone: 'bad', detail: '', link: null, command: null, busy: false };
/** @type {UpdateStatus} */
const CHECKING = { ...FAILED, badge: 'Checking…', icon: 'more', tone: 'run' };
/** @type {UpdateStatus} */
const INSTALLING = { ...CHECKING, badge: 'Installing…' };
