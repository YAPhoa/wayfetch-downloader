// The Add links page: paste links and choose where they will be saved.
// Collecting puts them on the Links page for review; nothing downloads,
// and no server is contacted, until they are started there.
//
// The text stays when collecting fails, and when the page is left
// without collecting, which sends nothing; it goes once the links are
// collected. A retry of the same text to the same folder reuses its
// idempotency key, so a request that timed out after the engine took it
// is not collected twice. The folders are read afresh each time the page
// opens; left at the default, the engine saves the links where new
// downloads go when it collects them.
//
// Route: `/add`.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { count, entries, entriesProblem } from '../client/links.js';
import { h, show } from './dom.js';
import { icon } from './icons.js';
import { toolbar } from './toolbar.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').Settings} Settings
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {import('../client/contract.js').CommandResult} CommandResult
 * @typedef {Extract<CommandResult, { type: 'links_collected' }>} Collected
 */

/**
 * What the page works with, from the workspace hosting it.
 * @typedef {{
 *   client: WayfetchClient,
 *   hello: () => Hello | null,
 *   settings: () => Settings | null,
 *   onSettings: (settings: Settings) => void,
 *   onCollected: (result: Collected) => void,
 *   onSignedOut: () => void,
 *   back: () => void,
 *   backLabel: () => string,
 *   settingsButton: Node,
 * }} AddContext
 */

/** The page's path. */
export const ADD_PATH = '/add';

/**
 * What to tell the user about a refused collect.
 * @param {WayfetchError} error
 */
function notCollected(error) {
  switch (error.code) {
    case 'limit_exceeded':
      return `${error.message.replace(/^./, (c) => c.toUpperCase())}.`;
    case 'not_found':
      return 'That folder is no longer a permitted destination. Choose another.';
    case 'not_applicable':
      return `That folder cannot be used: ${error.message}`;
    case 'permission_denied':
      return 'You may not add links.';
    case 'unavailable':
      return 'The engine cannot be reached. Your links are still here; try again shortly.';
    default:
      return `Not collected: ${error.message}`;
  }
}

/**
 * What to say once links are collected.
 * @param {{ collected: number, invalid: number }} result
 */
export function collectedText({ collected, invalid }) {
  const valid = `Collected ${count(collected, 'link')} for review.`;
  if (invalid === 0) return valid;
  return `${valid} ${count(invalid, 'entry is', 'entries are')} not a valid link; ${invalid === 1 ? 'it is' : 'they are'} marked below.`;
}

export class AddLinksPage {
  /** @param {AddContext} context */
  constructor(context) {
    this.context = context;
    this.shown = false;
    /** The text typed and not yet collected. */
    this.text = '';
    /** What was last sent, and its key, for retrying it. */
    /** @type {{ text: string, destination: string, key: string } | null} */
    this.sent = null;
    /** Whether a folder other than the default was chosen. */
    this.chosen = false;
    /** The settings the folders were listed from. @type {Settings | null} */
    this.listed = null;

    this.message = h('p', { class: 'message error', role: 'alert' });
    this.field = h('textarea', {
      id: 'collect-links',
      name: 'links',
      rows: 10,
      spellcheck: 'false',
      autocomplete: 'off',
      placeholder: 'https://example.com/file.zip',
      oninput: () => {
        this.text = this.field.value;
      },
    });
    this.destination = h('select', {
      id: 'collect-destination',
      name: 'destination',
      onchange: () => {
        this.chosen = this.destination.value !== (this.listed?.default_root ?? '');
      },
    });
    this.serverPath = h('span', { class: 'tag', hidden: true }, 'Server path');
    this.collect = h('button', { type: 'submit', class: 'primary', 'data-key': 'collect' }, 'Collect links');
    this.backLabel = h('span', null, 'Back');
    this.title = h('h1', { id: 'add-title', tabindex: -1 }, 'Add links');
    const form = h(
      'form',
      { class: 'collect-form', onsubmit: (/** @type {SubmitEvent} */ event) => void this.submit(event) },
      h('label', { for: 'collect-links' }, 'Links, one per line'),
      this.field,
      h(
        'div',
        { class: 'collect-destination' },
        h('label', { for: 'collect-destination' }, 'Save to'),
        this.destination,
        this.serverPath,
      ),
      h(
        'p',
        { class: 'hint' },
        'Links are collected on the Links page for review. Nothing is downloaded or checked until you start them there.',
      ),
      this.message,
      h(
        'div',
        { class: 'row' },
        this.collect,
        h('button', { type: 'button', 'data-key': 'collect-cancel', onclick: () => context.back() }, 'Cancel'),
      ),
    );
    this.element = h(
      'div',
      {
        class: 'add-page',
        hidden: true,
        onkeydown: (/** @type {KeyboardEvent} */ event) => {
          if (event.key === 'Escape' && !event.defaultPrevented) {
            event.preventDefault();
            context.back();
          }
        },
      },
      toolbar(
        [
          h(
            'button',
            { type: 'button', class: 'tool back', 'data-key': 'add-back', onclick: () => context.back() },
            icon('back'),
            this.backLabel,
          ),
          this.title,
        ],
        context.settingsButton,
      ),
      h('main', { class: 'add-content' }, h('section', { class: 'add-panel', 'aria-labelledby': 'add-title' }, form)),
    );
  }

  /** Shows the page with whatever was typed before, the folders read afresh. */
  open() {
    if (this.shown) return;
    this.shown = true;
    this.element.hidden = false;
    this.backLabel.textContent = this.context.backLabel();
    this.field.value = this.text;
    show(this.message, '');
    this.folders(this.context.settings());
    this.context.client.settings().then(
      (fresh) => {
        this.context.onSettings(fresh);
        this.folders(fresh);
      },
      () => {
        // The engine checks the folder anyway; the list shown stays.
      },
    );
    this.field.focus({ preventScroll: true });
  }

  /** Hides the page; the text stays for next time. */
  close() {
    this.shown = false;
    this.element.hidden = true;
  }

  /**
   * Lists the permitted folders, keeping a folder chosen meanwhile.
   * @param {Settings | null} settings
   */
  folders(settings) {
    this.listed = settings;
    const roots = settings?.roots ?? [];
    const selected = this.chosen ? this.destination.value : (settings?.default_root ?? '');
    show(
      this.destination,
      roots.length === 0 && h('option', { value: '' }, 'The download folder'),
      roots.map((root) =>
        h(
          'option',
          { value: root.id, disabled: !root.available },
          root.available ? root.path : `${root.path} (unavailable)`,
        ),
      ),
    );
    this.destination.value = selected;
    this.serverPath.hidden = this.context.client.local || roots.length === 0;
  }

  /** @param {SubmitEvent} event */
  async submit(event) {
    event.preventDefault();
    const { context, field } = this;
    const text = field.value;
    const items = entries(text);
    const connected = context.hello();
    if (!connected) {
      show(this.message, 'Not connected yet. Your links are still here; try again in a moment.');
      return;
    }
    const problem = entriesProblem(items, connected.limits);
    if (problem) {
      show(this.message, problem);
      field.focus();
      return;
    }
    // The default is left to the engine, which knows the current one.
    const folder = this.chosen ? this.destination.value : '';
    if (this.sent?.text !== text || this.sent.destination !== folder) {
      this.sent = { text, destination: folder, key: idempotencyKey() };
    }
    this.collect.disabled = true;
    show(this.message, '');
    try {
      const result = await context.client.collectLinks({
        idempotencyKey: this.sent.key,
        target: connected.instance,
        items,
        destination: folder === '' ? null : /** @type {RootId} */ (folder),
      });
      this.text = '';
      this.sent = null;
      this.chosen = false;
      field.value = '';
      if (result.type === 'links_collected') context.onCollected(result);
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        context.onSignedOut();
        return;
      }
      show(this.message, error instanceof WayfetchError ? notCollected(error) : String(error));
      field.focus();
    } finally {
      this.collect.disabled = false;
    }
  }
}
