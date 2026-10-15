// Device-local appearance, shared by the browser and desktop shells.
/** @typedef {'system' | 'light' | 'dark'} Theme */
const KEY = 'wayfetch.theme';

/** @param {unknown} value @returns {Theme} */
function validTheme(value) {
  return value === 'light' || value === 'dark' ? value : 'system';
}

/**
 * Read and apply before either shell renders, including setup and sign-in.
 * Storage can be blocked; choices still work for this window.
 * @param {Window} host
 * @param {HTMLElement} root
 */
export function createTheme(host, root) {
  /** @type {Theme} */
  let choice = 'system';
  try { choice = validTheme(host.localStorage.getItem(KEY)); } catch { /* Private/blocked storage. */ }
  const system = host.matchMedia('(prefers-color-scheme: dark)');
  const apply = () => {
    root.dataset.theme = choice === 'system' ? (system.matches ? 'dark' : 'light') : choice;
  };
  system.addEventListener('change', apply);
  apply();
  return {
    get: () => choice,
    /** @param {Theme} value */
    set(value) {
      choice = validTheme(value);
      apply();
      try { host.localStorage.setItem(KEY, choice); } catch { /* Keep the in-memory choice. */ }
    },
  };
}

// Unit tests can import the factory without a browser.
export const theme = typeof window === 'undefined' ? null : createTheme(window, document.documentElement);
