// Owner setup and sign-in forms for the browser shell.

import { fields, h, show } from './dom.js';

/**
 * @typedef {import('../transports/http.js').HttpClient} HttpClient
 * @typedef {import('../transports/http.js').SessionStatus} SessionStatus
 * @typedef {import('../client/contract.js').WayfetchError} WayfetchError
 */

/** Shortest owner password the server accepts. */
const MIN_PASSWORD = 12;

/**
 * Why a sign-in or setup attempt failed, in words.
 * @param {WayfetchError} error
 * @param {string} refused what to say when the server refused the attempt
 */
function attemptFailed(error, refused) {
  if (error.code === 'overloaded' && error.retryAfter) {
    return `Too many attempts. Try again in ${error.retryAfter} seconds.`;
  }
  if (error.code === 'permission_denied') return refused;
  if (error.code === 'not_applicable') return 'Setup is already complete. Sign in instead.';
  return error.message;
}

/**
 * A form that submits once at a time and shows its outcome.
 * @param {string} title
 * @param {(string | HTMLElement)[]} intro
 * @param {HTMLElement[]} inputs
 * @param {string} action the submit button's label
 * @param {(values: Record<string, string>) => Promise<string | null>} submit resolves to an error message, or null when done
 */
function form(title, intro, inputs, action, submit) {
  const message = h('p', { class: 'message error', role: 'alert' });
  const button = h('button', { type: 'submit' }, action);
  const element = h(
    'form',
    {
      class: 'card narrow',
      onsubmit: async (/** @type {SubmitEvent} */ event) => {
        event.preventDefault();
        button.disabled = true;
        show(message);
        const error = await submit(fields(element));
        button.disabled = false;
        if (error) show(message, error);
      },
    },
    h('h1', null, title),
    intro.map((line) => (typeof line === 'string' ? h('p', null, line) : line)),
    inputs,
    message,
    button,
  );
  return element;
}

/**
 * @param {string} label
 * @param {Record<string, unknown>} props
 */
function field(label, props) {
  return h('label', null, h('span', null, label), h('input', { required: true, ...props }));
}

/**
 * The first-run form: the setup secret from the server's auth directory,
 * and the owner's name and password.
 * @param {HttpClient} auth
 * @param {(status: SessionStatus) => void} done
 */
export function setupView(auth, done) {
  return form(
    'Set up Wayfetch',
    [
      'Create the owner account for this server.',
      h(
        'p',
        null,
        'The setup secret is in the file ',
        h('code', null, 'setup-secret'),
        ' in the server’s auth directory. It works once, for 15 minutes after the server starts.',
      ),
    ],
    [
      field('Setup secret', { name: 'secret', autocomplete: 'off', spellcheck: 'false' }),
      field('Owner name', { name: 'username', autocomplete: 'username', value: 'owner' }),
      field('Password', {
        name: 'password',
        type: 'password',
        autocomplete: 'new-password',
        minlength: MIN_PASSWORD,
      }),
      field('Repeat password', {
        name: 'repeat',
        type: 'password',
        autocomplete: 'new-password',
        minlength: MIN_PASSWORD,
      }),
    ],
    'Create owner',
    async (values) => {
      if (values.password !== values.repeat) return 'The passwords do not match.';
      try {
        done(
          await auth.setup({
            setupSecret: values.secret.trim(),
            username: values.username,
            password: values.password,
          }),
        );
        return null;
      } catch (error) {
        return attemptFailed(
          /** @type {WayfetchError} */ (error),
          'The setup secret is wrong or has expired. Restart the server for a new one.',
        );
      }
    },
  );
}

/**
 * The sign-in form, with an optional notice such as why the last session
 * ended.
 * @param {HttpClient} auth
 * @param {string | null} notice
 * @param {(status: SessionStatus) => void} done
 */
export function loginView(auth, notice, done) {
  return form(
    'Sign in',
    notice ? [h('p', { class: 'message notice', role: 'status' }, notice)] : [],
    [
      field('Owner name', { name: 'username', autocomplete: 'username' }),
      field('Password', { name: 'password', type: 'password', autocomplete: 'current-password' }),
    ],
    'Sign in',
    async (values) => {
      try {
        done(await auth.login({ username: values.username, password: values.password }));
        return null;
      } catch (error) {
        return attemptFailed(
          /** @type {WayfetchError} */ (error),
          'The name or password is wrong.',
        );
      }
    },
  );
}
