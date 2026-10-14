// The browser shell: signs the owner in through the daemon's browser
// session, then hosts the shared Downloads views over the HTTP transport.
//
// Signing out, an expired session, or a daemon restart (which ends every
// session) brings back the sign-in form; downloads are never affected by
// what happens to this page.

import { loginView, setupView } from './components/auth.js';
import { h, show } from './components/dom.js';
import { Downloads } from './components/downloads.js';
import { HttpClient } from './transports/http.js';

/**
 * @typedef {import('./transports/http.js').SessionStatus} SessionStatus
 */

const ENDED =
  'Your session ended: it expired, was signed out, or the server restarted. ' +
  'Downloads kept running. Sign in again to continue.';

class BrowserShell {
  /** @param {HTMLElement} root */
  constructor(root) {
    this.root = root;
    this.client = new HttpClient({ onUnauthenticated: () => this.signedOut() });
    /** @type {Downloads | null} */
    this.downloads = null;
  }

  async boot() {
    show(this.root, h('p', { class: 'message', role: 'status' }, 'Loading…'));
    let status;
    try {
      status = await this.client.session();
    } catch (error) {
      show(
        this.root,
        h(
          'section',
          { class: 'card narrow' },
          h('h1', null, 'Wayfetch'),
          h('p', { class: 'message error', role: 'alert' }, String(/** @type {Error} */ (error).message)),
          h('button', { type: 'button', onclick: () => this.boot() }, 'Try again'),
        ),
      );
      return;
    }
    if (status === null) {
      show(
        this.root,
        h(
          'section',
          { class: 'card narrow' },
          h('h1', null, 'Browser sign-in is off'),
          h(
            'p',
            null,
            'This server does not accept browser sign-in. Start wayfetchd with ',
            h('code', null, '--auth-dir'),
            ' and ',
            h('code', null, '--browser-origin'),
            ' to use this page.',
          ),
        ),
      );
    } else if (status.authenticated) {
      this.open(status);
    } else if (status.setup_required) {
      show(this.root, setupView(this.client, (signedIn) => this.open(signedIn)));
    } else {
      this.login(null);
    }
  }

  /** @param {string | null} notice */
  login(notice) {
    show(this.root, loginView(this.client, notice, (status) => this.open(status)));
    this.root.querySelector('input')?.focus();
  }

  /** Called when the session turns out to be gone. */
  signedOut() {
    if (!this.downloads) return;
    this.downloads.stop();
    this.downloads = null;
    this.login(ENDED);
  }

  /** @param {SessionStatus} status */
  open(status) {
    this.downloads?.stop();
    const downloads = new Downloads(this.client, { onSignedOut: () => this.signedOut() });
    this.downloads = downloads;
    const signOutError = h('span', { class: 'message error', role: 'alert' });
    const signOut = h(
      'button',
      {
        type: 'button',
        class: 'secondary',
        onclick: async (/** @type {MouseEvent} */ event) => {
          const button = /** @type {HTMLButtonElement} */ (event.currentTarget);
          button.disabled = true;
          signOutError.textContent = '';
          try {
            await this.client.logout();
          } catch (error) {
            // The session may still be live, so do not claim otherwise.
            button.disabled = false;
            signOutError.textContent =
              `Signing out failed: ${/** @type {Error} */ (error).message} ` +
              'You may still be signed in. Try again.';
            return;
          }
          if (this.downloads === downloads) {
            downloads.stop();
            this.downloads = null;
          }
          this.login('You are signed out. Downloads keep running on the server.');
        },
      },
      'Sign out',
    );
    show(
      this.root,
      h(
        'header',
        null,
        h('strong', { class: 'brand' }, 'Wayfetch'),
        downloads.status,
        h('span', { class: 'user' }, status.username ?? ''),
        signOutError,
        signOut,
      ),
      downloads.element,
    );
    downloads.start();
  }
}

void new BrowserShell(/** @type {HTMLElement} */ (document.getElementById('app'))).boot();
