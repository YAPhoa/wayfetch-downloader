// Settings > Link filters: link filters, which leave matching links out
// when links are added or found on an analyzed page. Every change saves the
// whole list at once, at the settings revision shown; the engine matches
// links, and says what a filter would do with a link to try.

import { WayfetchError, idempotencyKey } from '../client/contract.js';
import {
  CHECKED_CONDITIONS,
  CHOICES,
  CONDITIONS,
  FILE_KINDS,
  MEMBERSHIP,
  SIZE_UNITS,
  TEXT_OPS,
  describeRule,
  parseSize,
  sizeFields,
  splitExtensions,
  waitsForCheck,
} from '../client/filters.js';
import { confirmDialog, openDialog } from './dialog.js';
import { focusedKey, h, refocus, show } from './dom.js';
import { icon } from './icons.js';
import { Menu, rowMenuHandlers } from './menu.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').Hello} Hello
 * @typedef {import('../client/contract.js').Settings} Settings
 * @typedef {import('../client/contract.js').LinkFilters} LinkFilters
 * @typedef {import('../client/contract.js').LinkRule} LinkRule
 * @typedef {import('../client/contract.js').FilterTrial} FilterTrial
 * @typedef {import('./menu.js').Anchor} Anchor
 * @typedef {import('./menu.js').MenuEntry} MenuEntry
 * @typedef {{
 *   client: WayfetchClient,
 *   hello: () => Hello | null,
 *   onSignedOut: () => void,
 *   onSettings: (settings: Settings) => void,
 *   onChanged: (refresh: () => void) => void,
 * }} FiltersContext
 * @typedef {{
 *   key: string,
 *   on: HTMLInputElement,
 *   element: HTMLElement,
 *   read: () => { value: unknown } | { error: string },
 * }} ConditionRow
 */

/** The columns of the filters table; the last holds each row's menu. */
const FILTER_COLUMNS = ['On', 'Name', 'Leaves out links when'];
/** Filters shown per page. */
const PAGE_SIZE = 10;

/** What a tried link would meet, in words. */
/** @type {Record<FilterTrial, string>} */
const TRIALS = {
  left_out: 'This filter would leave this link out.',
  kept: 'This filter would keep this link.',
  check_decides: 'Its check decides: this link would wait off the list until its size, check result, or account is known.',
};

/**
 * Why filters were not saved, in plain words.
 * @param {WayfetchError} error
 */
function notSaved(error) {
  switch (error.code) {
    case 'revision_conflict':
      return 'The settings were changed elsewhere and have been reloaded. Try again.';
    case 'permission_denied':
      return 'You may not change settings.';
    case 'unavailable':
      return 'The engine cannot be reached. Try again shortly.';
    default:
      return `Not saved: ${error.message}`;
  }
}

/**
 * A select of `pairs`, showing `value`.
 * @param {readonly [string, string][]} pairs
 * @param {string} value
 * @param {Record<string, unknown>} props
 */
function select(pairs, value, props) {
  const element = h(
    'select',
    props,
    pairs.map(([v, text]) => h('option', { value: v }, text)),
  );
  element.value = value;
  return element;
}

/**
 * A filter's condition rows, filled from `rule`, each switched on by its
 * checkbox; controls of a row switched off are disabled.
 * @param {LinkRule | null} rule
 * @returns {ConditionRow[]}
 */
function conditionRows(rule) {
  return CONDITIONS.map(([key, name]) => {
    const existing = rule?.[key];
    const id = `filter-${key}`;
    /** @type {HTMLElement[]} */
    let controls;
    /** @type {ConditionRow['read']} */
    let read;
    if (key === 'file_name' || key === 'link' || key === 'found_on') {
      const condition = rule?.[key];
      const op = select(TEXT_OPS, condition?.op ?? 'contains', { 'aria-label': `${name} comparison`, 'data-key': `${id}-op` });
      const pattern = h('input', {
        type: 'text',
        class: 'pattern',
        value: condition?.pattern ?? '',
        placeholder: key === 'file_name' ? 'e.g. *sample*' : 'e.g. example.com',
        'aria-label': `${name} pattern`,
        'data-key': `${id}-pattern`,
        spellcheck: 'false',
      });
      const regex = h('input', { type: 'checkbox', 'data-key': `${id}-regex`, checked: Boolean(condition?.regex) });
      controls = [
        op,
        pattern,
        h('label', { class: 'regex', title: 'Read the pattern as a regular expression' }, regex, 'Regex'),
      ];
      read = () => {
        if (!pattern.value.trim()) return { error: `Enter what the ${name.toLowerCase()} should match.` };
        return { value: { op: op.value, pattern: pattern.value.trim(), ...(regex.checked && { regex: true }) } };
      };
    } else if (key === 'size') {
      const size = rule?.size;
      const op = select(
        [
          ['is', 'is between'],
          ['is_not', 'is not between'],
        ],
        size?.op ?? 'is',
        { 'aria-label': 'Size comparison', 'data-key': `${id}-op` },
      );
      const bound = (/** @type {'min' | 'max'} */ end, /** @type {number | undefined} */ bytes) => {
        const shown = bytes === undefined || (end === 'min' && bytes === 0) ? null : sizeFields(bytes);
        const amount = h('input', {
          type: 'text',
          class: 'size',
          inputmode: 'decimal',
          value: shown?.amount ?? '',
          placeholder: end === 'min' ? '0' : 'any',
          'aria-label': end === 'min' ? 'Smallest size' : 'Largest size',
          'data-key': `${id}-${end}`,
        });
        const unit = select(
          SIZE_UNITS.map((u) => [u, u]),
          shown?.unit ?? 'MiB',
          { 'aria-label': end === 'min' ? 'Smallest size unit' : 'Largest size unit', 'data-key': `${id}-${end}-unit` },
        );
        return { amount, unit };
      };
      const min = bound('min', size?.min);
      const max = bound('max', size?.max);
      controls = [op, min.amount, min.unit, h('span', { class: 'hint' }, 'to'), max.amount, max.unit];
      read = () => {
        const low = parseSize(min.amount.value, /** @type {import('../client/filters.js').SizeUnit} */ (min.unit.value));
        const high = parseSize(max.amount.value, /** @type {import('../client/filters.js').SizeUnit} */ (max.unit.value));
        if (Number.isNaN(low) || Number.isNaN(high)) return { error: 'Enter sizes as numbers.' };
        if (low === null && high === null) return { error: 'Enter a smallest or largest size.' };
        if (low !== null && high !== null && high < low) return { error: 'The largest size is smaller than the smallest.' };
        return { value: { op: op.value, min: low ?? 0, ...(high !== null && { max: high }) } };
      };
    } else if (key === 'file_type') {
      const types = rule?.file_type;
      const op = select(MEMBERSHIP, types?.op ?? 'is', { 'aria-label': 'File type comparison', 'data-key': `${id}-op` });
      const kinds = FILE_KINDS.map(([kind, text]) =>
        h(
          'label',
          { class: 'option' },
          h('input', { type: 'checkbox', value: kind, 'data-key': `${id}-${kind}`, checked: types?.kinds?.includes(kind) ?? false }),
          text,
        ),
      );
      const extensions = h('input', {
        type: 'text',
        class: 'pattern',
        value: (types?.extensions ?? []).join(', '),
        placeholder: 'Other extensions, e.g. nfo, r*',
        'aria-label': 'Other extensions, separated by commas',
        'data-key': `${id}-extensions`,
        spellcheck: 'false',
      });
      controls = [op, h('span', { class: 'options' }, kinds), extensions];
      read = () => {
        const chosen = kinds
          .map((label) => /** @type {HTMLInputElement} */ (label.querySelector('input')))
          .filter((input) => input.checked)
          .map((input) => input.value);
        const named = splitExtensions(extensions.value);
        if (chosen.length === 0 && named.length === 0) return { error: 'Choose a file type or name an extension.' };
        return {
          value: { op: op.value, ...(chosen.length > 0 && { kinds: chosen }), ...(named.length > 0 && { extensions: named }) },
        };
      };
    } else {
      const condition = rule?.[key];
      const op = select(MEMBERSHIP, condition?.op ?? 'is', { 'aria-label': `${name} comparison`, 'data-key': `${id}-op` });
      /** @type {readonly [string, string][]} */
      const pairs = CHOICES[key];
      const values = pairs.map(([value, text]) =>
        h(
          'label',
          { class: 'option' },
          h('input', {
            type: 'checkbox',
            value,
            'data-key': `${id}-${value}`,
            checked: /** @type {string[] | undefined} */ (condition?.values)?.includes(value) ?? false,
          }),
          text,
        ),
      );
      controls = [op, h('span', { class: 'options' }, values)];
      read = () => {
        const chosen = values
          .map((label) => /** @type {HTMLInputElement} */ (label.querySelector('input')))
          .filter((input) => input.checked)
          .map((input) => input.value);
        if (chosen.length === 0) return { error: `Choose at least one ${name.toLowerCase()}.` };
        return { value: { op: op.value, values: chosen } };
      };
    }
    const on = h('input', { type: 'checkbox', id, 'data-key': `${id}-on`, checked: existing !== undefined });
    const fields = h('div', { class: 'condition-fields' }, controls);
    const enable = () => {
      for (const control of fields.querySelectorAll('input, select')) {
        /** @type {HTMLInputElement} */ (control).disabled = !on.checked;
      }
    };
    on.addEventListener('change', () => {
      enable();
      if (!on.checked) return;
      // The field typed into first, else the first choice.
      const first = fields.querySelector('input[type="text"]') ?? fields.querySelector('input, select');
      /** @type {HTMLElement | null} */ (first)?.focus();
    });
    enable();
    const element = h(
      'div',
      { class: 'condition', 'data-condition': key },
      h('label', { class: 'condition-name', for: id }, on, name),
      fields,
    );
    return { key, on, element, read };
  });
}

/**
 * The filter the dialog's fields describe, or why it cannot be saved.
 * @param {HTMLInputElement} name
 * @param {ConditionRow[]} rows
 * @param {boolean} enabled
 * @returns {{ rule: LinkRule } | { error: string }}
 */
function readRule(name, rows, enabled) {
  if (!name.value.trim()) return { error: 'Give the filter a name.' };
  /** @type {Record<string, unknown>} */
  const rule = { name: name.value.trim(), enabled };
  for (const row of rows) {
    if (!row.on.checked) continue;
    const read = row.read();
    if ('error' in read) return { error: read.error };
    rule[row.key] = read.value;
  }
  if (Object.keys(rule).length === 2) return { error: 'Tick at least one condition.' };
  return { rule: /** @type {LinkRule} */ (/** @type {unknown} */ (rule)) };
}

/**
 * Opens the dialog to add a filter, or edit `rule`. `onSave` saves the
 * filter and says why not, if it was not saved; the dialog stays open then.
 * @param {FiltersContext} context
 * @param {LinkRule | null} rule
 * @param {(rule: LinkRule) => Promise<string | null>} onSave
 */
function editFilter(context, rule, onSave) {
  const name = h('input', {
    type: 'text',
    id: 'filter-name',
    'data-key': 'filter-name',
    value: rule?.name ?? '',
    placeholder: 'e.g. Skip samples',
    maxlength: 80,
  });
  const rows = conditionRows(rule);
  const message = h('p', { class: 'message', role: 'status', 'data-key': 'filter-message' });
  const link = h('input', {
    type: 'url',
    'data-key': 'filter-try-link',
    placeholder: 'https://example.com/file.zip',
    'aria-label': 'A link to try the filter on',
    spellcheck: 'false',
  });
  const trial = h('p', { class: 'hint', role: 'status', 'data-key': 'filter-trial' });
  const tryIt = async () => {
    const read = readRule(name, rows, true);
    if ('error' in read) return show(trial, read.error);
    if (!link.value.trim()) return show(trial, 'Enter a link to try.');
    show(trial, 'Trying…');
    try {
      show(trial, TRIALS[await context.client.tryLinkFilter(read.rule, link.value.trim())]);
    } catch (error) {
      show(trial, error instanceof WayfetchError ? error.message : String(error));
    }
  };
  const save = h('button', { type: 'submit', class: 'primary', 'data-key': 'filter-save' }, 'Save');
  const form = h(
    'form',
    {
      class: 'filter-form',
      novalidate: true,
      onsubmit: async (/** @type {SubmitEvent} */ event) => {
        event.preventDefault();
        const read = readRule(name, rows, rule?.enabled ?? true);
        if ('error' in read) return show(message, read.error);
        save.disabled = true;
        show(message, 'Saving…');
        const problem = await onSave(read.rule);
        save.disabled = false;
        if (problem === null) dialog.close();
        else show(message, problem);
      },
    },
    h('label', { class: 'field', for: 'filter-name' }, h('span', null, 'Name'), name),
    h(
      'fieldset',
      { class: 'conditions' },
      h('legend', null, 'Leave out links when all the ticked conditions hold'),
      rows.map((row) => row.element),
    ),
    h(
      'p',
      { class: 'hint' },
      `${CONDITIONS.filter(([key]) => CHECKED_CONDITIONS.includes(key))
        .map(([, label]) => label)
        .join(', ')} are known once a link is checked; until then, a link they could match waits off the list.`,
    ),
    h(
      'div',
      { class: 'filter-try' },
      link,
      h('button', { type: 'button', 'data-key': 'filter-try', onclick: () => void tryIt() }, 'Try'),
    ),
    trial,
    message,
    h(
      'div',
      { class: 'dialog-actions' },
      h('button', { type: 'button', 'data-key': 'filter-cancel', onclick: () => dialog.close() }, 'Cancel'),
      save,
    ),
  );
  const dialog = openDialog({ title: rule ? `Edit filter ${rule.name}` : 'Add filter', className: 'filter-dialog' }, form);
  name.focus();
}

/**
 * The Link filters section.
 * @param {FiltersContext} context
 */
export function filtersSection(context) {
  const { client } = context;
  const section = h('div', { class: 'settings-section' });
  // What the last change did, as a banner that fades; refusals stay
  // longer, so they can be read.
  const banner = h('div', { class: 'flash', 'data-key': 'filters-message', hidden: true });
  /** @type {ReturnType<typeof setTimeout> | undefined} */
  let fading;
  /**
   * @param {string} text
   * @param {'ok' | 'bad'} [tone]
   */
  const say = (text, tone = 'ok') => {
    clearTimeout(fading);
    banner.hidden = !text;
    if (!text) return;
    banner.className = `flash ${tone}`;
    banner.setAttribute('role', tone === 'bad' ? 'alert' : 'status');
    show(banner, icon(tone === 'bad' ? 'cancel' : 'check'), h('span', null, text));
    fading = setTimeout(
      () => {
        banner.classList.add('leaving');
        fading = setTimeout(() => (banner.hidden = true), 400);
      },
      tone === 'bad' ? 8000 : 2500,
    );
  };
  const canChange = client.permissions.includes('control');
  /** @type {Settings | null} */
  let current = null;
  /** The page shown, from 0. */
  let page = 0;
  /**
   * Shows the page holding the filter at `index` once saved.
   * @param {number} index
   */
  const follow = (index) => (page = Math.floor(index / PAGE_SIZE));
  let saving = false;
  const markMenu = () => {
    for (const row of section.querySelectorAll('tr[data-filter]')) {
      const open = row.getAttribute('data-filter') === menu.owner;
      row.classList.toggle('menu-open', open);
      row.querySelector('button.more')?.setAttribute('aria-expanded', String(open));
    }
  };
  const menu = new Menu('Filter actions', markMenu);

  /**
   * Saves `filters`, replacing every filter, and says `said`. Resolves to
   * why it was not saved, or `null`.
   * @param {LinkFilters} filters
   * @param {string} said
   * @returns {Promise<string | null>}
   */
  const save = async (filters, said) => {
    const hello = context.hello() ?? (await client.hello().catch(() => null));
    if (!hello || !current) return 'Not connected yet. Try again in a moment.';
    const working = focusedKey(section);
    saving = true;
    try {
      await client.updateSettings({
        idempotencyKey: idempotencyKey(),
        target: hello.instance,
        expectedRevision: current.revision,
        linkFilters: filters,
      });
      saving = false;
      await reload(said);
      refocus(section, working);
      return null;
    } catch (error) {
      saving = false;
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        context.onSignedOut();
        return error.message;
      }
      const text = error instanceof WayfetchError ? notSaved(error) : String(error);
      if (error instanceof WayfetchError && error.code === 'revision_conflict') await reload(text, 'bad');
      else say(text, 'bad');
      return text;
    }
  };

  /**
   * The filters with `change` made to a copy of their list.
   * @param {(rules: LinkRule[]) => void} change
   * @returns {LinkFilters}
   */
  const changed = (change) => {
    const filters = /** @type {Settings} */ (current).link_filters;
    const rules = filters.filters.map((rule) => ({ ...rule }));
    change(rules);
    return { enabled: filters.enabled, filters: rules };
  };

  /**
   * What can be done with the filter at `index`.
   * @param {LinkRule} rule
   * @param {number} index
   * @param {number} count
   * @returns {MenuEntry[]}
   */
  const entries = (rule, index, count) => [
    { key: 'edit-filter', label: 'Edit…', run: () => edit(index) },
    {
      key: 'duplicate-filter',
      label: 'Duplicate',
      run: () => {
        follow(index + 1);
        void save(
          changed((rules) => rules.splice(index + 1, 0, { ...rule, name: `${rule.name} (copy)`.slice(0, 80) })),
          'Duplicated.',
        );
      },
    },
    'separator',
    {
      key: 'filter-up',
      label: 'Move up',
      disabled: index === 0,
      run: () => {
        follow(index - 1);
        void save(changed((rules) => rules.splice(index - 1, 0, ...rules.splice(index, 1))), 'Moved.');
      },
    },
    {
      key: 'filter-down',
      label: 'Move down',
      disabled: index === count - 1,
      run: () => {
        follow(index + 1);
        void save(changed((rules) => rules.splice(index + 1, 0, ...rules.splice(index, 1))), 'Moved.');
      },
    },
    'separator',
    {
      key: 'remove-filter',
      label: 'Remove…',
      danger: true,
      run: () =>
        confirmDialog(
          {
            title: 'Remove filter',
            confirm: 'Remove',
            onConfirm: () => void save(changed((rules) => rules.splice(index, 1)), 'Removed.'),
          },
          `Remove ${rule.name}? Links already in the list stay.`,
        ),
    },
  ];

  /** @param {number | null} index the filter to edit, or `null` to add one */
  const edit = (index) => {
    const rule = index === null ? null : /** @type {Settings} */ (current).link_filters.filters[index];
    editFilter(context, rule, (saved) => {
      if (index === null) follow(/** @type {Settings} */ (current).link_filters.filters.length);
      return save(
        changed((rules) => {
          if (index === null) rules.push(saved);
          else rules[index] = saved;
        }),
        index === null ? 'Filter added.' : 'Saved.',
      );
    });
  };

  /**
   * One filter's row.
   * @param {LinkRule} rule
   * @param {number} index
   * @param {number} count
   */
  const filterRow = (rule, index, count) => {
    const id = String(index);
    /** @param {Anchor} anchor */
    const open = (anchor) =>
      menu.show(entries(rule, index, count), anchor, {
        owner: id,
        restore: () => /** @type {HTMLElement | null} */ (section.querySelector(`[data-key="filter-menu-${id}"]`))?.focus(),
      });
    const more = h(
      'button',
      {
        type: 'button',
        class: 'more',
        'data-key': `filter-menu-${id}`,
        'aria-label': `Actions for ${rule.name}`,
        'aria-haspopup': 'menu',
        'aria-expanded': 'false',
        disabled: !canChange,
        onclick: (/** @type {MouseEvent} */ event) => menu.toggle(id, /** @type {HTMLElement} */ (event.currentTarget), open),
      },
      icon('more'),
    );
    return h(
      'tr',
      { 'data-filter': id, ...(canChange && rowMenuHandlers(more, open)), ondblclick: () => canChange && edit(index) },
      h(
        'td',
        { class: 'toggle' },
        h('input', {
          type: 'checkbox',
          'data-key': `filter-on-${id}`,
          'aria-label': `Use ${rule.name}`,
          checked: rule.enabled,
          disabled: !canChange,
          onchange: (/** @type {Event} */ event) => {
            const on = /** @type {HTMLInputElement} */ (event.currentTarget).checked;
            void save(
              changed((rules) => (rules[index].enabled = on)),
              on ? `${rule.name} is on.` : `${rule.name} is off.`,
            );
          },
        }),
      ),
      h('td', { class: 'filter-name' }, rule.name),
      h(
        'td',
        { class: 'filter-summary' },
        describeRule(rule),
        waitsForCheck(rule) && h('span', { class: 'tag', title: 'Matching links wait off the list for their check' }, 'After check'),
      ),
      h('td', { class: 'row-actions' }, more),
    );
  };

  /**
   * Shows `settings`' filters, saying `said`.
   * @param {Settings} settings
   * @param {string} said
   * @param {'ok' | 'bad'} [tone]
   */
  const render = (settings, said, tone = 'ok') => {
    const focused = focusedKey(section);
    menu.close(false);
    current = settings;
    const { enabled, filters } = settings.link_filters;
    const toggle = h('input', {
      type: 'checkbox',
      role: 'switch',
      class: 'switch',
      'aria-label': 'Filters enabled',
      title: enabled ? 'Filters enabled' : 'Filters disabled',
      'data-key': 'filters-enabled',
      checked: enabled,
      disabled: !canChange,
      onchange: () =>
        void save(
          { enabled: toggle.checked, filters: settings.link_filters.filters },
          toggle.checked ? 'Filters enabled.' : 'Filters disabled.',
        ),
    });
    const add = h(
      'button',
      { type: 'button', class: 'ghost', 'data-key': 'add-filter', disabled: !canChange, onclick: () => edit(null) },
      icon('add'),
      'Add filter',
    );
    const pages = Math.max(1, Math.ceil(filters.length / PAGE_SIZE));
    page = Math.min(Math.max(page, 0), pages - 1);
    const from = page * PAGE_SIZE;
    const shown = filters.slice(from, from + PAGE_SIZE);
    /** @param {number} to */
    const turn = (to) => {
      page = to;
      render(settings, '');
    };
    const pager =
      pages > 1 &&
      h(
        'nav',
        { class: 'filters-pager', 'aria-label': 'Pages of filters' },
        h('span', { class: 'range' }, `${from + 1}\u2013${from + shown.length} of ${filters.length}`),
        h(
          'button',
          {
            type: 'button',
            class: 'icon-only',
            'aria-label': 'Previous page',
            'data-key': 'filters-back',
            disabled: page === 0,
            onclick: () => turn(page - 1),
          },
          icon('left'),
        ),
        h('span', { class: 'page' }, `${page + 1} / ${pages}`),
        h(
          'button',
          {
            type: 'button',
            class: 'icon-only',
            'aria-label': 'Next page',
            'data-key': 'filters-next',
            disabled: page === pages - 1,
            onclick: () => turn(page + 1),
          },
          icon('right'),
        ),
      );
    const rows =
      filters.length === 0
        ? [
            h(
              'tr',
              null,
              h(
                'td',
                { class: 'empty', colspan: FILTER_COLUMNS.length + 1, 'data-key': 'no-filters' },
                'No filters yet. Every link found is collected.',
              ),
            ),
          ]
        : shown.map((rule, at) => filterRow(rule, from + at, filters.length));
    if (said) say(said, tone);
    show(
      section,
      h(
        'div',
        { class: `settings-group filters${enabled ? '' : ' off'}` },
        h(
          'div',
          { class: 'group-head' },
          h('span', { class: 'group-title' }, 'Filters'),
          h('span', { class: 'filters-state', 'aria-hidden': 'true' }, enabled ? 'Enabled' : 'Disabled'),
          toggle,
        ),
        h(
          'div',
          { class: 'table-scroll' },
          h(
            'table',
            { class: 'accounts-table filters-table', 'aria-label': 'Link filters' },
            h(
              'thead',
              null,
              h(
                'tr',
                null,
                FILTER_COLUMNS.map((name) => h('th', { scope: 'col' }, name)),
                h('th', { scope: 'col' }, h('span', { class: 'visually-hidden' }, 'Actions')),
              ),
            ),
            h('tbody', null, rows),
          ),
        ),
        h('div', { class: 'filters-foot' }, add, pager),
      ),
      h(
        'p',
        { class: 'hint' },
        enabled
          ? 'Filters apply to links you add and links found on analyzed pages.'
          : 'Filtering is off. Saved filters won’t run.',
        canChange ? '' : ' You may not change settings.',
      ),
      menu.element,
      banner,
    );
    refocus(section, focused);
  };

  /**
   * @param {string} [said]
   * @param {'ok' | 'bad'} [tone]
   */
  const reload = async (said = '', tone = /** @type {'ok' | 'bad'} */ ('ok')) => {
    try {
      const settings = await client.settings();
      context.onSettings(settings);
      render(settings, said, tone);
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'unauthenticated') {
        context.onSignedOut();
        return;
      }
      const text = error instanceof WayfetchError ? error.message : String(error);
      show(
        section,
        h('p', { class: 'message error', role: 'alert' }, `Filters could not be loaded. ${text}`),
        h('button', { type: 'button', onclick: () => void reload() }, 'Try again'),
      );
    }
  };

  context.onChanged(() => {
    if (!saving) void reload();
  });
  show(section, h('p', { class: 'message', role: 'status' }, 'Loading…'));
  void reload();
  return section;
}
