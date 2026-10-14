// The desktop shell: hosts the shared Downloads views over the desktop
// host's IPC, in the window of `wayfetch-desktop`. It has no sign-in; the
// browser's sign-in shell is `main.js`.
//
// Reloading this page, or losing its event stream, only stops watching:
// the engine runs in the host, and jobs continue.

import { h, show } from '../components/dom.js';
import { Downloads } from '../components/downloads.js';
import { DesktopIpcClient } from '../transports/ipc.js';

const root = /** @type {HTMLElement} */ (document.getElementById('app'));
let client;
try {
  client = new DesktopIpcClient();
} catch (error) {
  show(
    root,
    h(
      'section',
      { class: 'card narrow' },
      h('h1', null, 'Wayfetch'),
      h('p', { class: 'message error', role: 'alert' }, String(/** @type {Error} */ (error).message)),
    ),
  );
}
if (client) {
  // The desktop engine runs in this app on this computer.
  const account = h('span', { class: 'account' }, h('span', { class: 'user' }, 'This computer'));
  // Only a browser session can end; the desktop client never reports it.
  const downloads = new Downloads(client, { onSignedOut: () => {}, account });
  show(root, downloads.element);
  downloads.start();
}
