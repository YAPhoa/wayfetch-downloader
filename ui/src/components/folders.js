// Choosing a folder on the server to add as a download folder: a dialog
// that lists the folders inside one, moves into a folder or up, or goes
// to a path typed in, and adds the folder shown. The engine lists and
// checks the folders; this only shows what it answers.

import { WayfetchError } from '../client/contract.js';
import { openDialog } from './dialog.js';
import { h, show } from './dom.js';
import { icon } from './icons.js';

/**
 * @typedef {import('../client/contract.js').FolderListing} FolderListing
 * @typedef {import('../client/contract.js').RootSummary} RootSummary
 */

/**
 * Lets the owner pick a server folder, starting at `start` (the top of
 * the filesystem if `null`), and adds it. Resolves with the folder added,
 * or `null` if the dialog was closed without adding one; rejects if the
 * session ended.
 * @param {{
 *   browseFolders: (path: string | null) => Promise<FolderListing>,
 *   addFolder: (path: string) => Promise<RootSummary>,
 * }} server the client's own methods
 * @param {string | null} start
 * @returns {Promise<RootSummary | null>}
 */
export function browseServerFolders(server, start) {
  return new Promise((resolve, reject) => {
    /** @type {FolderListing | null} */
    let shown = null;
    /** Counts listings asked for; only the latest one's answer is shown. */
    let asked = 0;
    /** @type {RootSummary | Error | null} */
    let outcome = null;
    const field = h('input', {
      type: 'text',
      name: 'path',
      autocomplete: 'off',
      spellcheck: 'false',
      'data-key': 'folder-path',
    });
    const up = h(
      'button',
      { type: 'button', 'data-key': 'folder-up', onclick: () => shown?.parent && void go(shown.parent, 'folder-up') },
      'Up',
    );
    const list = h('ul', { class: 'folder-list', 'aria-label': 'Folders inside' });
    const message = h('p', { class: 'message', role: 'status' });
    const add = h('button', { type: 'submit', class: 'primary', 'data-key': 'folder-add', disabled: true }, 'Add this folder');
    const form = h(
      'form',
      {
        class: 'folder-browser',
        onsubmit: async (/** @type {SubmitEvent} */ event) => {
          event.preventDefault();
          // Enter in the path field goes there; the button adds the folder shown.
          if (event.submitter !== add) {
            await go(field.value.trim(), 'folder-path');
            return;
          }
          // Adding is offered only while the folder shown is the one
          // listed last, not while another is loading or after it failed.
          if (!shown || add.disabled) return;
          const at = asked;
          add.disabled = true;
          show(message, 'Adding…');
          try {
            outcome = await server.addFolder(shown.path);
            dialog.close();
          } catch (error) {
            if (!failed(error) && at === asked) add.disabled = false;
          }
        },
      },
      h(
        'div',
        { class: 'row' },
        h('label', { class: 'folder-field' }, 'Folder', field),
        h('button', { type: 'submit', 'data-key': 'folder-go' }, 'Go'),
        up,
      ),
      list,
      message,
      h('div', { class: 'row' }, add),
    );
    const dialog = openDialog(
      {
        title: 'Add a server folder',
        className: 'folders',
        closeLabel: 'Close without adding',
        onclose: () => (outcome instanceof Error ? reject(outcome) : resolve(outcome)),
      },
      form,
    );

    /**
     * Shows `error`; an ended session closes the dialog and rejects.
     * Returns whether it did.
     * @param {unknown} error
     */
    const failed = (error) => {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        outcome = error;
        dialog.close();
        return true;
      }
      const text = error instanceof WayfetchError ? error.message : String(error);
      message.classList.add('error');
      show(message, `${text.charAt(0).toUpperCase()}${text.slice(1)}.`);
      return false;
    };

    /**
     * Lists the folders at `path`, keeping focus near `from`. A slower
     * answer to an earlier request is dropped.
     * @param {string | null} path
     * @param {string} from the `data-key` of the control used
     */
    const go = async (path, from) => {
      const at = ++asked;
      add.disabled = true;
      message.classList.remove('error');
      show(message, 'Loading…');
      let listing;
      try {
        listing = await server.browseFolders(path === '' ? null : path);
      } catch (error) {
        // Adding stays off: the path typed is not the folder listed.
        if (at === asked || (error instanceof WayfetchError && error.code === 'unauthenticated')) failed(error);
        return;
      }
      if (at !== asked) return;
      shown = listing;
      field.value = listing.path;
      up.disabled = listing.parent === undefined;
      add.disabled = false;
      show(
        list,
        listing.folders.map((name) =>
          h(
            'li',
            null,
            h(
              'button',
              {
                type: 'button',
                class: 'folder-item',
                'data-key': `folder-${name}`,
                onclick: () => void go(join(listing.path, name), 'folder-entry'),
              },
              icon('folder'),
              h('span', null, name),
            ),
          ),
        ),
      );
      show(
        message,
        listing.folders.length === 0 && 'There are no folders inside.',
        listing.truncated && `Only the first ${listing.folders.length} folders are shown. Type a path to go further.`,
      );
      // Moving into a folder leaves focus on its first folder, as in a file
      // dialog, as does reaching the top with Up; other controls keep it.
      if (from === 'folder-entry' || (from === 'folder-up' && up.disabled)) {
        /** @type {HTMLElement | null} */ (list.querySelector('button') ?? field)?.focus();
      }
    };

    field.focus();
    void go(start, 'folder-path');
  });
}

/**
 * The path of `name` inside the folder at `path`.
 * @param {string} path
 * @param {string} name
 */
function join(path, name) {
  return path.endsWith('/') ? `${path}${name}` : `${path}/${name}`;
}
