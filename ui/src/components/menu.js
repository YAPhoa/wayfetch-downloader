// A pop-up menu of commands, following the WAI-ARIA menu pattern: arrow
// keys, Home and End move between items, Enter or Space runs one, Escape
// or Tab closes it, and focus returns to where it came from. It is placed
// inside the window, and closes on a press outside it, a scroll, a
// resize, or the window losing focus.
//
// Disabled items stay in the menu, and focusable, so a keyboard or screen
// reader user learns they exist; a hint can say why they do not apply.

import { h, show } from './dom.js';

/**
 * @typedef {{
 *   key: string,
 *   label: string,
 *   run: () => void,
 *   disabled?: boolean,
 *   hint?: string,
 *   danger?: boolean,
 * }} MenuItem
 * @typedef {MenuItem | 'separator'} MenuEntry
 * @typedef {{ left: number, top: number, right: number, bottom: number }} Anchor
 */

/** Space kept between a menu and the window's edge, in CSS pixels. */
const MARGIN = 4;

/**
 * Where a menu of `size` goes: below and right of `anchor`, flipped above
 * or to the left when it would leave the window, and kept inside it.
 * @param {Anchor} anchor a pointer's position, or the element it opens from
 * @param {{ width: number, height: number }} size
 * @param {{ width: number, height: number }} viewport
 */
export function place(anchor, size, viewport) {
  let left = anchor.left;
  if (left + size.width > viewport.width - MARGIN) left = anchor.right - size.width;
  let top = anchor.bottom;
  if (top + size.height > viewport.height - MARGIN) top = anchor.top - size.height;
  const clamp = (/** @type {number} */ value, /** @type {number} */ extent, /** @type {number} */ room) =>
    Math.max(MARGIN, Math.min(value, room - MARGIN - extent));
  return {
    left: clamp(left, size.width, viewport.width),
    top: clamp(top, size.height, viewport.height),
  };
}

/** Inputs whose text can be edited, and so copied and pasted. */
const TEXT_FIELDS = ['text', 'search', 'url', 'email', 'password', 'tel', 'number']
  .map((type) => `input[type="${type}"]`)
  .concat('input:not([type])', 'textarea')
  .join(', ');

/**
 * Whether a right-click on `target` should get the webview's own menu:
 * in a text field, for cut, copy and paste, and over selected text, for
 * copy. Everywhere else an app shows only its own menus.
 * @param {Element} target
 * @param {Selection | null} selection
 */
export function wantsNativeMenu(target, selection) {
  if (target.closest(TEXT_FIELDS) || /** @type {HTMLElement} */ (target).isContentEditable === true) return true;
  return Boolean(selection && !selection.isCollapsed && selection.containsNode(target, true));
}

/**
 * Keeps the webview's own menu (Reload, Inspect Element, …) out of an
 * app's window, except where `wantsNativeMenu` says it helps. Menus the
 * page opens itself, such as a row's, are unaffected.
 * @param {Document} doc
 */
export function suppressNativeMenu(doc) {
  doc.addEventListener('contextmenu', (event) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target || !wantsNativeMenu(target, doc.getSelection())) event.preventDefault();
  });
}

export class Menu {
  /**
   * @param {string} label the menu's accessible name
   * @param {() => void} onchange called when it opens or closes
   */
  constructor(label, onchange) {
    this.onchange = onchange;
    this.element = h('div', {
      class: 'menu',
      role: 'menu',
      'aria-label': label,
      hidden: true,
      onkeydown: (/** @type {KeyboardEvent} */ event) => this.key(event),
      // The browser's own menu never opens over this one.
      oncontextmenu: (/** @type {MouseEvent} */ event) => event.preventDefault(),
    });
    /** @type {(() => void) | null} */
    this.restore = null;
    /** What the open menu is for, as its opener named it. */
    /** @type {string | null} */
    this.owner = null;
    /**
     * The owner of a menu that a press outside it just closed, and what was
     * pressed, so pressing its opener again closes it rather than
     * reopening it.
     * @type {{ owner: string, target: Node } | null}
     */
    this.dismissed = null;
    this.onPointerDown = (/** @type {PointerEvent} */ event) => {
      const target = /** @type {Node} */ (event.target);
      if (this.element.contains(target)) return;
      this.dismissed = this.owner === null ? null : { owner: this.owner, target };
      this.close(false);
    };
    this.onDismiss = () => this.close(false);
    this.onScroll = (/** @type {Event} */ event) => {
      if (!this.element.contains(/** @type {Node} */ (event.target))) this.close(false);
    };
  }

  get open() {
    return !this.element.hidden;
  }

  /**
   * Opens the menu at `anchor` with focus on its first item.
   * @param {MenuEntry[]} entries
   * @param {Anchor} anchor
   * @param {{ owner: string, restore: () => void }} options `restore` puts
   *   focus back when the menu closes from the keyboard or runs an item
   */
  show(entries, anchor, { owner, restore }) {
    this.close(false);
    this.dismissed = null;
    show(
      this.element,
      entries.map((entry) =>
        entry === 'separator'
          ? h('div', { role: 'separator', class: 'menu-separator' })
          : h(
              'button',
              {
                type: 'button',
                role: 'menuitem',
                tabindex: -1,
                class: entry.danger ? 'menu-item danger' : 'menu-item',
                'data-key': `menu-item-${entry.key}`,
                'aria-disabled': entry.disabled ? 'true' : null,
                onpointermove: (/** @type {PointerEvent} */ event) => {
                  // Keep the visual highlight and keyboard target together.
                  // Touch scrolling must not move focus or scroll an item.
                  if (event.pointerType === 'touch') return;
                  const item = /** @type {HTMLElement} */ (event.currentTarget);
                  if (document.activeElement !== item) item.focus({ preventScroll: true });
                },
                onclick: () => {
                  if (entry.disabled) return;
                  this.close(true);
                  entry.run();
                },
              },
              h('span', { class: 'menu-label' }, entry.label),
              entry.hint && h('span', { class: 'menu-hint' }, entry.hint),
            ),
      ),
    );
    this.owner = owner;
    this.restore = restore;
    this.element.hidden = false;
    const { width, height } = this.element.getBoundingClientRect();
    const at = place(anchor, { width, height }, {
      width: document.documentElement.clientWidth,
      height: document.documentElement.clientHeight,
    });
    this.element.style.left = `${at.left}px`;
    this.element.style.top = `${at.top}px`;
    this.items()[0]?.focus();
    document.addEventListener('pointerdown', this.onPointerDown, true);
    document.addEventListener('scroll', this.onScroll, true);
    window.addEventListener('resize', this.onDismiss);
    window.addEventListener('blur', this.onDismiss);
    this.onchange();
  }

  /**
   * Whether a press on `opener` just closed `owner`'s menu; asking clears
   * it.
   * @param {string} owner
   * @param {Element} opener
   */
  justDismissed(owner, opener) {
    const dismissed = this.dismissed;
    this.dismissed = null;
    return dismissed !== null && dismissed.owner === owner && opener.contains(dismissed.target);
  }

  /**
   * Closes the menu. With `refocus`, focus goes back to the opener;
   * otherwise it stays where the person moved it.
   * @param {boolean} refocus
   */
  close(refocus) {
    if (!this.open) return;
    document.removeEventListener('pointerdown', this.onPointerDown, true);
    document.removeEventListener('scroll', this.onScroll, true);
    window.removeEventListener('resize', this.onDismiss);
    window.removeEventListener('blur', this.onDismiss);
    const restore = this.restore;
    const inside = this.element.contains(document.activeElement);
    this.element.hidden = true;
    this.element.replaceChildren();
    this.owner = null;
    this.restore = null;
    // Focus left inside a hidden menu would be lost.
    if (refocus || inside) restore?.();
    this.onchange();
  }

  /** @returns {HTMLElement[]} */
  items() {
    return [...this.element.querySelectorAll('[role="menuitem"]')].map(
      (item) => /** @type {HTMLElement} */ (item),
    );
  }

  /** @param {KeyboardEvent} event */
  key(event) {
    const items = this.items();
    const at = items.indexOf(/** @type {HTMLElement} */ (document.activeElement));
    /** @param {number} index */
    const focus = (index) => items[(index + items.length) % items.length]?.focus();
    switch (event.key) {
      case 'ArrowDown':
        focus(at + 1);
        break;
      case 'ArrowUp':
        focus(at - 1);
        break;
      case 'Home':
        focus(0);
        break;
      case 'End':
        focus(items.length - 1);
        break;
      case 'Escape':
      case 'Tab':
        this.close(true);
        break;
      case 'Enter':
      case ' ':
        if (at >= 0) items[at].click();
        break;
      default:
        return;
    }
    event.preventDefault();
    event.stopPropagation();
  }
}
