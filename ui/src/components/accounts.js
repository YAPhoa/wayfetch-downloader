// Settings > Accounts & services: the accounts the user added, and
// signing one in (architecture brief section 8.3, ADR-24).
//
// Signing in happens on the service's own site: the engine asks the
// service for a short code, the person enters it there and approves
// Wayfetch, and the engine keeps what the service then issues. No page
// here asks for a password, token, or key, and nothing the service issues
// ever reaches the page, so nothing secret can end up in the page's
// storage, its address, or a form field.
//
// Listing reads what the engine last stored and contacts no service;
// "Check" asks the service now. Each change is sent at the revision shown,
// and a change made elsewhere meanwhile reloads the list.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import { openDialog } from './dialog.js';
import { focusedKey, h, refocus, show } from './dom.js';
import { icon } from './icons.js';
import { loadServices, pickService } from './services.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').Accounts} Accounts
 * @typedef {import('../client/contract.js').AccountSummary} AccountSummary
 * @typedef {import('../client/contract.js').AccountAction} AccountAction
 * @typedef {import('../client/contract.js').Authorization} Authorization
 * @typedef {import('../client/contract.js').ServiceSupport} ServiceSupport
 * @typedef {import('./services.js').Service} Service
 * @typedef {{
 *   client: WayfetchClient,
 *   hello: () => Hello | null,
 *   onSignedOut: () => void,
 * }} AccountsContext
 */

/** The columns of the accounts table; the last holds each row's actions. */
export const ACCOUNT_COLUMNS = ['Service', 'Account', 'Status', 'Expires'];

/** How often a sign-in waiting for approval is looked at. */
const POLL_MS = 2000;

/**
 * What an account's status says, in plain words, and its tone.
 * @param {AccountSummary} account
 * @param {Accounts['vault']} vault
 * @returns {[string, 'ok' | 'warn' | 'bad' | null]}
 */
export function accountStatus(account, vault) {
  if (vault === 'locked') return ['Locked', 'warn'];
  if (!account.enabled) return ['Disabled', null];
  switch (account.status) {
    case 'authenticated':
      return ['Signed in', 'ok'];
    case 'invalid_credentials':
      return ['Sign in again', 'bad'];
    case 'unavailable':
      return ['Could not check', 'warn'];
    default:
      return ['Not checked', null];
  }
}

/**
 * Why accounts cannot be added or used, if they cannot.
 * @param {Accounts} accounts
 * @param {boolean} local whether the engine runs on this computer
 * @returns {string | null}
 */
export function vaultProblem(accounts, local) {
  const where = local ? 'this app' : 'the server';
  switch (accounts.vault) {
    case 'not_configured':
      return `Accounts cannot be added: ${where} has no key to protect account sign-ins with.`;
    case 'locked':
      return (
        `Stored sign-ins are locked: ${accounts.vault_problem ?? 'their key cannot be used'}. ` +
        'Accounts cannot be checked or signed in until the key is restored; you can still disable or remove them.'
      );
    default:
      return null;
  }
}

/**
 * A date the way the list shows it.
 * @param {number} at Unix milliseconds
 */
function date(at) {
  return new Date(at).toLocaleDateString([], { day: 'numeric', month: 'short', year: 'numeric' });
}

/**
 * Why an account action failed, in plain words.
 * @param {WayfetchError} error
 */
function explain(error) {
  switch (error.code) {
    case 'permission_denied':
      return 'You may not manage accounts.';
    case 'not_found':
      return 'That account was removed elsewhere.';
    case 'busy':
    case 'unavailable':
      return `${error.message}`;
    case 'overloaded':
      return 'Too many account requests are waiting. Try again shortly.';
    default:
      return error.message;
  }
}

/**
 * The Accounts & services section.
 * @param {AccountsContext} context
 */
export function accountsSection(context) {
  const { client } = context;
  const section = h('div', { class: 'settings-section' });
  const message = h('p', { class: 'message', role: 'status', 'data-key': 'accounts-message' });
  /** @type {Map<string, Service>} */
  const catalogue = new Map();

  if (!client.permissions.includes('accounts')) {
    show(
      section,
      h('p', { class: 'message', 'data-key': 'accounts-denied' }, 'You may not manage accounts.'),
      h('p', { class: 'hint' }, 'Links you add directly download without an account.'),
    );
    return section;
  }

  /**
   * Shows `error` with `say`; an ended session goes to the shell.
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
   * The list for `accounts`, saying `said`.
   * @param {Accounts} accounts
   * @param {string} said
   */
  const render = (accounts, said = '') => {
    const focused = focusedKey(section);
    const blocked = vaultProblem(accounts, client.local);
    /** @type {Map<string, ServiceSupport>} */
    const integrations = new Map(accounts.services.map((s) => [s.service, s]));
    show(message, said);
    const add = h(
      'button',
      {
        type: 'button',
        'data-key': 'add-account',
        onclick: () =>
          pickService({
            integrations,
            blocked,
            onPick: (service, support) => signIn(context, service, support, null, done),
          }),
      },
      icon('add'),
      'Add account…',
    );
    const rows =
      accounts.accounts.length === 0
        ? [
            h(
              'tr',
              null,
              h(
                'td',
                { class: 'empty', colspan: ACCOUNT_COLUMNS.length + 1, 'data-key': 'no-accounts' },
                'No accounts added yet.',
              ),
            ),
          ]
        : accounts.accounts.map((account) => accountRow(account, accounts, integrations));
    show(
      section,
      blocked && h('p', { class: 'message notice', 'data-key': 'vault-problem' }, blocked),
      h(
        'div',
        { class: 'settings-group accounts' },
        h('div', { class: 'group-head' }, h('span', { class: 'group-title' }, 'Accounts'), add),
        h(
          'div',
          { class: 'table-scroll' },
          h(
            'table',
            { class: 'accounts-table', 'aria-label': 'Accounts' },
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                ACCOUNT_COLUMNS.map((name) => h('th', { scope: 'col' }, name)),
                h('th', { scope: 'col' }, h('span', { class: 'visually-hidden' }, 'Actions')),
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
        'Links you add directly download without an account. To add one, pick its service from the list. ' +
          'Signing in to a service and downloading through it are separate: a service marked Sign-in only can be checked, but downloads do not use it yet.',
      ),
    );
    refocus(section, focused);
  };

  /**
   * One account's row.
   * @param {AccountSummary} account
   * @param {Accounts} accounts
   * @param {Map<string, ServiceSupport>} integrations
   */
  const accountRow = (account, accounts, integrations) => {
    const service = catalogue.get(account.service);
    const name = service?.name ?? account.service;
    const label = account.alias ?? account.username ?? name;
    const [status, tone] = accountStatus(account, accounts.vault);
    const usable = accounts.vault === 'ready' && account.enabled;
    const support = integrations.get(account.service);
    /**
     * @param {string} key
     * @param {string} text
     * @param {string} aria
     * @param {(button: HTMLButtonElement) => Promise<void>} run
     * @param {string} [className]
     */
    const action = (key, text, aria, run, className) =>
      h(
        'button',
        {
          type: 'button',
          class: className,
          'data-key': `${key}-${account.id}`,
          'aria-label': `${aria} ${label}`,
          onclick: (/** @type {MouseEvent} */ event) =>
            void run(/** @type {HTMLButtonElement} */ (event.currentTarget)),
        },
        text,
      );
    return h(
      'tr',
      { 'data-account': account.id },
      h('td', null, name),
      h(
        'td',
        null,
        h('span', { class: 'account-name' }, label),
        account.alias && account.username && h('span', { class: 'hint' }, ` ${account.username}`),
        account.plan && h('span', { class: 'tag' }, account.plan === 'premium' ? 'Premium' : 'Free'),
      ),
      h(
        'td',
        null,
        h('span', { class: tone ? `tone-${tone}` : null }, status),
        account.problem && h('span', { class: 'hint account-problem' }, account.problem),
      ),
      h('td', null, account.expires_at ? date(account.expires_at) : '—'),
      h(
        'td',
        { class: 'row-actions' },
        usable && action('check-account', 'Check', 'Check', (button) => check(account, button)),
        usable &&
          support &&
          service &&
          action('replace-account', 'Sign in again…', 'Sign in again to', async () =>
            signIn(context, service, support, account, done),
          ),
        action(
          'toggle-account',
          account.enabled ? 'Disable' : 'Enable',
          account.enabled ? 'Disable' : 'Enable',
          (button) => command(account, account.enabled ? 'disable' : 'enable', button),
        ),
        action('remove-account', 'Remove…', 'Remove', async () => confirmRemove(account, label), 'remove-root'),
      ),
    );
  };

  /**
   * Checks `account` with its service now.
   * @param {AccountSummary} account
   * @param {HTMLButtonElement} button
   */
  const check = async (account, button) => {
    button.disabled = true;
    show(message, 'Checking…');
    try {
      const checked = await client.refreshAccount(account.id);
      const [status] = accountStatus(checked, 'ready');
      await reload(`Checked: ${status}${checked.problem ? ` (${checked.problem})` : ''}.`);
    } catch (error) {
      button.disabled = false;
      failed(error);
    }
  };

  /**
   * Enables, disables, or removes `account` at the revision shown.
   * @param {AccountSummary} account
   * @param {AccountAction} action
   * @param {HTMLButtonElement | null} button
   */
  const command = async (account, action, button) => {
    const hello = context.hello();
    if (!hello) {
      show(message, 'Not connected yet. Try again in a moment.');
      return;
    }
    if (button) button.disabled = true;
    try {
      await client.accountCommand(account.id, {
        idempotencyKey: idempotencyKey(),
        target: hello.instance,
        action,
        expectedRevision: account.revision,
      });
      const done = { enable: 'Enabled.', disable: 'Disabled: kept, but not used.', remove: 'Removed.' }[action];
      await reload(done);
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'revision_conflict') {
        await reload('The account was changed elsewhere and has been reloaded. Try again.');
        return;
      }
      if (button) button.disabled = false;
      failed(error);
    }
  };

  /**
   * Asks before removing `account`.
   * @param {AccountSummary} account
   * @param {string} label
   */
  const confirmRemove = async (account, label) => {
    const dialog = openDialog(
      { title: 'Remove account', className: 'confirm' },
      h(
        'p',
        null,
        `Remove ${label}? Wayfetch forgets this account and deletes its stored sign-in. `,
        'The account itself stays at the service.',
      ),
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
              void command(account, 'remove', null);
            },
          },
          'Remove',
        ),
      ),
    );
  };

  /** @param {string} said */
  const done = (said) => void reload(said);

  /** @param {string} said */
  const reload = async (said = '') => {
    try {
      if (catalogue.size === 0) {
        for (const service of await loadServices().catch(() => [])) catalogue.set(service.id, service);
      }
      render(await client.accounts(), said);
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        context.onSignedOut();
        return;
      }
      const text = error instanceof WayfetchError ? explain(error) : String(error);
      show(
        section,
        h('p', { class: 'message error', role: 'alert' }, `Accounts could not be loaded. ${text}`),
        h('button', { type: 'button', onclick: () => void reload() }, 'Try again'),
      );
    }
  };

  show(section, h('p', { class: 'message', role: 'status' }, 'Loading…'));
  void reload();
  return section;
}

/**
 * Signs in to `service`: a new account, or `account` again. The engine
 * asks the service for a code; the person enters it on the service's site
 * and approves Wayfetch there, while this waits. Closing the dialog
 * cancels a sign-in still waiting. `done` is told how it ended.
 * @param {AccountsContext} context
 * @param {Service} service
 * @param {ServiceSupport} support
 * @param {AccountSummary | null} account
 * @param {(said: string) => void} done
 */
export function signIn(context, service, support, account, done) {
  const { client } = context;
  const status = h('p', { class: 'message', role: 'status', 'data-key': 'sign-in-status' });
  const body = h('div', { class: 'sign-in' });
  /** The sign-in waiting for approval, if any. @type {Authorization | null} */
  let pending = null;
  let open = true;
  /** @type {ReturnType<typeof setTimeout> | null} */
  let timer = null;
  /** One key per attempt, so a retried start never starts twice. */
  let key = idempotencyKey();

  const dialog = openDialog(
    {
      title: account ? `Sign in to ${service.name} again` : `Sign in to ${service.name}`,
      className: 'sign-in-dialog',
      onclose: () => {
        open = false;
        if (timer !== null) clearTimeout(timer);
        if (pending) void client.cancelAuthorization(pending.id).catch(() => {});
      },
    },
    body,
    status,
    h(
      'p',
      { class: 'hint' },
      'Wayfetch never asks for your password: you approve it on the service’s own site, and can revoke it there.',
      support.downloads ? '' : ` Downloading through ${service.name} is not supported yet; signing in lets Wayfetch check the account.`,
    ),
  );

  /** @param {unknown} error */
  const failed = (error) => {
    if (error instanceof WayfetchError && error.code === 'unauthenticated') {
      dialog.close();
      context.onSignedOut();
      return;
    }
    show(status, error instanceof WayfetchError ? explain(error) : String(error));
    show(
      body,
      h(
        'div',
        { class: 'dialog-actions' },
        h(
          'button',
          {
            type: 'button',
            class: 'primary',
            'data-key': 'sign-in-retry',
            onclick: () => {
              // A sign-in that never started may be retried with its key;
              // one the engine refused starts afresh.
              if (!(error instanceof WayfetchError && error.code === 'unavailable')) key = idempotencyKey();
              void start(alias);
            },
          },
          'Try again',
        ),
      ),
    );
  };

  /** The name given to a new account. */
  let alias = '';

  /** @param {string} name */
  const start = async (name) => {
    const hello = context.hello();
    if (!hello) {
      show(status, 'Not connected yet. Try again in a moment.');
      return;
    }
    show(status, 'Asking for a code…');
    show(body);
    try {
      const result = await client.authorizeAccount({
        idempotencyKey: key,
        target: hello.instance,
        service: service.id,
        account: account ? { account: account.id, revision: account.revision } : undefined,
        alias: name || undefined,
      });
      if (!open) return;
      if (result.type === 'account_authorized') {
        finish('Signed in.');
        return;
      }
      if (result.type !== 'authorization_started') throw new Error(`unexpected answer: ${result.type}`);
      waiting(result);
    } catch (error) {
      if (open) failed(error);
    }
  };

  /**
   * Shows the code to enter, and waits for approval.
   * @param {Authorization} authorization
   */
  const waiting = (authorization) => {
    pending = authorization;
    const page = authorization.verification_url ?? '';
    show(
      body,
      h(
        'ol',
        { class: 'sign-in-steps' },
        h(
          'li',
          null,
          'Open ',
          // In the desktop app the address is shown to open in a browser,
          // so the app's own window never leaves Wayfetch.
          client.local
            ? h('span', { class: 'address', 'data-key': 'sign-in-page' }, page)
            : h('a', { href: page, target: '_blank', rel: 'noopener noreferrer', 'data-key': 'sign-in-page' }, page),
          client.local ? ' in your browser.' : '.',
        ),
        h(
          'li',
          null,
          'Enter this code: ',
          h('strong', { class: 'user-code', 'data-key': 'sign-in-code' }, authorization.user_code ?? ''),
        ),
        h('li', null, `Approve Wayfetch on ${service.name}.`),
      ),
      h(
        'div',
        { class: 'dialog-actions' },
        h('button', { type: 'button', 'data-key': 'sign-in-cancel', onclick: () => dialog.close() }, 'Cancel'),
      ),
    );
    show(status, `Waiting for you to approve Wayfetch on ${service.name}…`);
    timer = setTimeout(poll, POLL_MS);
  };

  const poll = async () => {
    timer = null;
    if (!open || !pending) return;
    try {
      const now = await client.authorization(pending.id);
      if (!open) return;
      switch (now.state) {
        case 'pending':
          timer = setTimeout(poll, POLL_MS);
          return;
        case 'completed':
          pending = null;
          finish(account ? 'Signed in again.' : 'Account added and signed in.');
          return;
        case 'expired':
          pending = null;
          key = idempotencyKey();
          failed(new WayfetchError('not_applicable', 'The code expired before it was approved.'));
          return;
        default:
          pending = null;
          key = idempotencyKey();
          failed(new WayfetchError('not_applicable', now.problem ?? 'The sign-in did not complete.'));
      }
    } catch (error) {
      if (!open) return;
      // A lost answer is asked again; the sign-in goes on meanwhile.
      if (error instanceof WayfetchError && error.code === 'unavailable') {
        timer = setTimeout(poll, POLL_MS);
        return;
      }
      pending = null;
      failed(error);
    }
  };

  /** @param {string} said */
  const finish = (said) => {
    open = false;
    dialog.close();
    done(said);
  };

  if (account) {
    void start('');
    return;
  }
  const field = h('input', {
    type: 'text',
    id: 'account-alias',
    name: 'alias',
    maxlength: 64,
    autocomplete: 'off',
    spellcheck: 'false',
    'data-key': 'account-alias',
  });
  show(
    body,
    h(
      'form',
      {
        class: 'sign-in-form',
        onsubmit: (/** @type {SubmitEvent} */ event) => {
          event.preventDefault();
          alias = field.value.trim();
          field.value = '';
          void start(alias);
        },
      },
      h('label', { for: 'account-alias' }, 'Name in Wayfetch (optional)'),
      field,
      h(
        'div',
        { class: 'dialog-actions' },
        h('button', { type: 'submit', class: 'primary', 'data-key': 'sign-in-start' }, 'Continue'),
      ),
    ),
  );
  field.focus();
}
