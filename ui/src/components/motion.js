// Shared motion: surfaces that open and close (menus, dialogs, the
// banner), and the moving pill behind a choice of tabs. The timing lives
// in style.css as custom properties; this only flips the classes the
// snippets there key on and waits out a closing transition before the
// surface is hidden. Under the reduced motion setting nothing waits.

/** Whether the person asked for less motion. */
export function still() {
  return typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;
}

/**
 * A duration custom property's value on `element`, in milliseconds.
 * @param {Element} element
 * @param {string} property such as `--dropdown-close-dur`
 */
export function duration(element, property) {
  const value = getComputedStyle(element).getPropertyValue(property).trim();
  const amount = parseFloat(value);
  if (!Number.isFinite(amount)) return 0;
  return value.endsWith('ms') ? amount : amount * 1000;
}

/** Closing surfaces' pending clean-ups. */
const closing = new WeakMap();

/**
 * Drops a pending close on `element` without running what it would do
 * afterwards, so a surface reopened mid-close starts from rest.
 * @param {Element} element
 */
export function settle(element) {
  clearTimeout(closing.get(element));
  closing.delete(element);
  element.classList.remove('is-closing');
}

/**
 * Plays `element`'s opening: from its resting style to `.is-open`. Call it
 * once the element is shown, so the resting style has been drawn.
 * @param {HTMLElement} element
 */
export function enter(element) {
  settle(element);
  if (element.classList.contains('is-open')) return;
  void element.offsetWidth;
  element.classList.add('is-open');
}

/**
 * Plays `element`'s closing, then calls `done`, which hides it. `done`
 * runs at once under reduced motion.
 * @param {HTMLElement} element
 * @param {string} property the custom property holding the closing time
 * @param {() => void} done
 */
export function leave(element, property, done) {
  settle(element);
  element.classList.remove('is-open');
  const wait = still() ? 0 : duration(element, property);
  if (wait <= 0) {
    done();
    return;
  }
  element.classList.add('is-closing');
  closing.set(
    element,
    setTimeout(() => {
      closing.delete(element);
      element.classList.remove('is-closing');
      done();
    }, wait),
  );
}

/**
 * The highlight behind a choice of tabs or links, which slides from the
 * old choice to the new one (Tabs sliding, from transitions.dev). It sits
 * in `container`, which must be the choices' positioned parent, and
 * follows them when the container's size changes, as when the page is
 * shown again or the fonts load.
 */
export class Pill {
  /**
   * @param {HTMLElement} container
   * @param {{ edge?: boolean }} [options] `edge` draws it as a 2 px bar
   *   along the choice's bottom edge rather than a box behind it
   */
  constructor(container, { edge = false } = {}) {
    this.edge = edge;
    this.element = document.createElement('span');
    this.element.className = edge ? 't-tabs-pill edge' : 't-tabs-pill';
    this.element.setAttribute('aria-hidden', 'true');
    this.element.hidden = true;
    /** @type {HTMLElement | null} */
    this.target = null;
    if (typeof ResizeObserver === 'function') {
      new ResizeObserver(() => this.move(this.target, false)).observe(container);
    }
  }

  /**
   * Moves behind `target`: sliding there, or with `animate` false jumping,
   * as on first draw. A target not on screen hides it until it is.
   * @param {HTMLElement | null} target
   * @param {boolean} [animate]
   */
  move(target, animate = true) {
    this.target = target;
    const pill = this.element;
    if (!target || target.offsetParent === null) {
      pill.hidden = true;
      return;
    }
    const jump = !animate || pill.hidden;
    pill.hidden = false;
    if (jump) pill.style.transition = 'none';
    const top = this.edge ? target.offsetTop + target.offsetHeight - 2 : target.offsetTop;
    pill.style.transform = `translate(${target.offsetLeft}px, ${top}px)`;
    pill.style.width = `${target.offsetWidth}px`;
    pill.style.height = this.edge ? '2px' : `${target.offsetHeight}px`;
    if (jump) {
      void pill.offsetWidth;
      pill.style.transition = '';
    }
  }
}

/**
 * Shakes a refused field once (Error state shake, from transitions.dev).
 * Its message and colours are left to the caller.
 * @param {Element | null | undefined} field
 */
export function shake(field) {
  if (!field || still()) return;
  field.classList.add('t-input');
  field.classList.remove('is-shaking');
  void (/** @type {HTMLElement} */ (field).offsetWidth);
  field.classList.add('is-shaking');
  field.addEventListener('animationend', () => field.classList.remove('is-shaking'), { once: true });
}

/**
 * Plays the arrival half of a text swap on `element`, just shown with new
 * text: it rises in through a blur.
 * @param {HTMLElement} element
 */
export function riseIn(element) {
  if (still()) return;
  element.classList.add('t-text-swap', 'is-enter-start');
  void element.offsetHeight;
  element.classList.remove('is-enter-start');
}

/** Labels part-way through a swap, and the text they are going to. */
const swapping = new WeakMap();

/**
 * Changes `element`'s text to `text` with the old text leaving upwards
 * and the new rising in through a blur (Text states swap, from
 * transitions.dev). The same text, or reduced motion, changes nothing or
 * changes it at once.
 * @param {HTMLElement} element
 * @param {string} text
 */
export function swapText(element, text) {
  const pending = swapping.get(element);
  if (pending) {
    pending.text = text;
    return;
  }
  if (element.textContent === text) return;
  if (still() || !element.isConnected || element.textContent === '') {
    // Nothing to see leave: the new text only arrives.
    element.textContent = text;
    if (text !== '') riseIn(element);
    return;
  }
  element.classList.add('t-text-swap', 'is-exit');
  const swap = { text };
  swapping.set(element, swap);
  setTimeout(() => {
    swapping.delete(element);
    element.textContent = swap.text;
    element.classList.remove('is-exit');
    element.classList.add('is-enter-start');
    void element.offsetHeight;
    element.classList.remove('is-enter-start');
  }, duration(element, '--text-swap-dur'));
}
