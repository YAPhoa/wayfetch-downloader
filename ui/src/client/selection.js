// Which rows of a table are ticked, for commands that act on several at
// once. Rows are the records a page shows, each with an `id`; a filter
// hides some, and commands act only on the ticked rows that are shown.
// No DOM, so it is tested under Node.

/**
 * @template K
 * @extends {Set<K>}
 */
export class Selection extends Set {
  /**
   * Ticks or unticks `items`.
   * @param {readonly { id: K }[]} items
   * @param {boolean} on
   */
  pick(items, on) {
    for (const { id } of items) {
      if (on) this.add(id);
      else this.delete(id);
    }
  }

  /**
   * Unticks what is no longer listed, such as after reading a page again.
   * @param {Iterable<K>} listed
   */
  keepOnly(listed) {
    const present = new Set(listed);
    for (const id of this) if (!present.has(id)) this.delete(id);
  }

  /**
   * The ticked ones of `shown`, in their order: what commands act on.
   * @template {{ id: K }} T
   * @param {readonly T[]} shown
   * @returns {T[]}
   */
  chosen(shown) {
    return shown.filter((item) => this.has(item.id));
  }

  /**
   * How a "select all shown" box looks: ticked when all of `shown` are,
   * mixed when some are, and off when nothing is shown.
   * @param {readonly { id: K }[]} shown
   */
  allState(shown) {
    const ticked = this.chosen(shown).length;
    return {
      checked: shown.length > 0 && ticked === shown.length,
      indeterminate: ticked > 0 && ticked < shown.length,
      disabled: shown.length === 0,
    };
  }
}
