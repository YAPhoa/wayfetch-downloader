// Modal dialogs: the browser's own `<dialog>`, which keeps focus inside
// it, closes on Escape, and shows above everything else. Closing it puts
// focus back where it was before it opened, or calls `restore` to, since
// what was focused may have been redrawn meanwhile.

import { h, show } from './dom.js';

/** Dialogs opened so far, for unique title IDs. */
let opened = 0;

/**
 * A modal dialog with a title bar and a close button. It is shown at once
 * and removed from the page when it closes; `onclose` runs then.
 * @param {{
 *   title: string,
 *   className?: string,
 *   closeLabel?: string,
 *   onclose?: () => void,
 *   restore?: () => void,
 * }} options
 * @param {...import('./dom.js').Children} content
 */
export function openDialog({ title, className = '', closeLabel = 'Close', onclose, restore }, ...content) {
  opened += 1;
  const id = `dialog-${opened}`;
  const returnTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const body = h('div', { class: 'dialog-body' });
  const dialog = h(
    'dialog',
    { class: `dialog ${className}`.trim(), 'aria-labelledby': id },
    h(
      'header',
      { class: 'dialog-head' },
      h('h2', { id }, title),
      h(
        'button',
        {
          type: 'button',
          class: 'close',
          'aria-label': closeLabel,
          onclick: () => dialog.close(),
        },
        '×',
      ),
    ),
    body,
  );
  show(body, ...content);
  dialog.addEventListener('close', () => {
    dialog.remove();
    onclose?.();
    if (restore) restore();
    else if (returnTo?.isConnected) returnTo.focus({ preventScroll: true });
  });
  document.body.append(dialog);
  dialog.showModal();
  return dialog;
}

/**
 * A confirmation with Cancel and a primary `confirm` button, which closes
 * the dialog and runs `onConfirm`.
 * @param {{ title: string, confirm: string, onConfirm: () => void }} options
 * @param {...import('./dom.js').Children} message
 */
export function confirmDialog({ title, confirm, onConfirm }, ...message) {
  const dialog = openDialog(
    { title, className: 'confirm' },
    h('p', null, ...message),
    h(
      'div',
      { class: 'dialog-actions' },
      h('button', { type: 'button', 'data-key': 'remove-cancel', onclick: () => dialog.close() }, 'Cancel'),
      h(
        'button',
        {
          type: 'button',
          class: 'primary',
          'data-key': 'remove-confirm',
          onclick: () => {
            dialog.close();
            onConfirm();
          },
        },
        confirm,
      ),
    ),
  );
  return dialog;
}
