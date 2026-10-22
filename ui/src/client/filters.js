// Link filters as Settings shows them: each condition's wording, a filter's
// one-line summary, and sizes typed with a unit. The engine matches links;
// nothing here does.

import { formatBytes } from './jobs.js';

/**
 * @typedef {import('./contract.js').LinkRule} LinkRule
 * @typedef {import('./contract.js').TextCondition} TextCondition
 * @typedef {import('./contract.js').TextOp} TextOp
 * @typedef {import('./contract.js').Membership} Membership
 * @typedef {import('./contract.js').FileKind} FileKind
 * @typedef {import('./contract.js').LinkOrigin} LinkOrigin
 * @typedef {import('./contract.js').CheckOutcome} CheckOutcome
 * @typedef {import('./contract.js').LinkRoute} LinkRoute
 * @typedef {'file_name' | 'link' | 'found_on'} TextField
 * @typedef {'added_from' | 'check' | 'route'} ChoiceField
 * @typedef {'B' | 'KiB' | 'MiB' | 'GiB'} SizeUnit
 */

/** @type {readonly [TextOp, string][]} */
export const TEXT_OPS = Object.freeze([
  ['contains', 'contains'],
  ['not_contains', 'does not contain'],
  ['is', 'is'],
  ['is_not', 'is not'],
]);

/** @type {readonly [Membership, string][]} */
export const MEMBERSHIP = Object.freeze([
  ['is', 'is'],
  ['is_not', 'is not'],
]);

/** @type {readonly [FileKind, string][]} */
export const FILE_KINDS = Object.freeze([
  ['archive', 'Archive'],
  ['video', 'Video'],
  ['audio', 'Audio'],
  ['image', 'Image'],
  ['document', 'Document'],
]);

/** The values each choice condition offers, with their labels. */
export const CHOICES = Object.freeze({
  added_from: /** @type {readonly [LinkOrigin, string][]} */ ([
    ['text', 'Pasted text'],
    ['web_page', 'A pasted web page'],
    ['analyzed_page', 'An analyzed page'],
  ]),
  check: /** @type {readonly [CheckOutcome, string][]} */ ([
    ['online', 'Online'],
    ['offline', 'Offline'],
    ['unknown', 'Could not tell'],
    ['page', 'Web page'],
  ]),
  route: /** @type {readonly [LinkRoute, string][]} */ ([
    ['direct', 'Directly'],
    ['account', 'Through an account'],
    ['media', 'Through a media service'],
  ]),
});

/** Each condition's name, in the order the filter dialog lists them. */
export const CONDITIONS = Object.freeze(
  /** @type {const} */ ([
    ['file_name', 'File name'],
    ['size', 'Size'],
    ['file_type', 'File type'],
    ['link', 'Link'],
    ['found_on', 'Found on'],
    ['added_from', 'Added from'],
    ['check', 'Check result'],
    ['route', 'Downloaded through'],
  ]),
);

/** Conditions only a link's check tells. */
export const CHECKED_CONDITIONS = Object.freeze(['size', 'check', 'route']);

/** Units a size is typed in. */
/** @type {readonly SizeUnit[]} */
export const SIZE_UNITS = Object.freeze(['B', 'KiB', 'MiB', 'GiB']);
const UNIT_BYTES = { B: 1, KiB: 1024, MiB: 1024 ** 2, GiB: 1024 ** 3 };

/**
 * `bytes` as the largest unit that shows it as a whole number.
 * @param {number} bytes
 * @returns {{ amount: string, unit: SizeUnit }}
 */
export function sizeFields(bytes) {
  for (const unit of [...SIZE_UNITS].reverse()) {
    const size = UNIT_BYTES[unit];
    if (bytes >= size && bytes % size === 0) return { amount: String(bytes / size), unit };
  }
  return { amount: String(bytes), unit: 'B' };
}

/**
 * The size typed in `unit`, in whole bytes; `null` for an empty field, and
 * `NaN` for one that is not a size.
 * @param {string} text
 * @param {SizeUnit} unit
 */
export function parseSize(text, unit) {
  const trimmed = text.trim();
  if (trimmed === '') return null;
  if (!/^\d+(\.\d+)?$/.test(trimmed)) return Number.NaN;
  const bytes = Math.round(Number(trimmed) * UNIT_BYTES[unit]);
  return Number.isSafeInteger(bytes) ? bytes : Number.NaN;
}

/**
 * The extensions typed, separated by commas or spaces, without dots.
 * @param {string} text
 */
export function splitExtensions(text) {
  return text
    .split(/[\s,]+/)
    .map((e) => e.replace(/^\.+/, ''))
    .filter(Boolean);
}

/**
 * @param {readonly [string, string][]} pairs
 * @param {string} value
 */
const label = (pairs, value) => pairs.find(([v]) => v === value)?.[1] ?? value;

/**
 * @param {string} name
 * @param {TextCondition} condition
 */
function textPart(name, condition) {
  const pattern = condition.regex ? `/${condition.pattern}/` : `“${condition.pattern}”`;
  return `${name} ${label(TEXT_OPS, condition.op)} ${pattern}`;
}

/**
 * `name` reads as a verb phrase: “added from”, “not added from”.
 * @param {string} name
 * @param {{ op: Membership, values: string[] }} condition
 * @param {readonly [string, string][]} pairs
 */
function choicePart(name, condition, pairs) {
  const values = condition.values.map((v) => label(pairs, v).toLowerCase());
  const said = `${name} ${values.join(' or ')}`;
  return condition.op === 'is' ? said : `not ${said}`;
}

/**
 * What a filter leaves out, in one line.
 * @param {LinkRule} rule
 */
export function describeRule(rule) {
  /** @type {string[]} */
  const parts = [];
  if (rule.file_name) parts.push(textPart('file name', rule.file_name));
  if (rule.size) {
    const { op, min, max } = rule.size;
    const range =
      max === undefined
        ? `at least ${formatBytes(min)}`
        : min === 0
          ? `at most ${formatBytes(max)}`
          : `${formatBytes(min)}–${formatBytes(max)}`;
    parts.push(op === 'is' ? `size ${range}` : `size not ${range}`);
  }
  if (rule.file_type) {
    const kinds = (rule.file_type.kinds ?? []).map((k) => label(FILE_KINDS, k).toLowerCase());
    const extensions = (rule.file_type.extensions ?? []).map((e) => `.${e}`);
    parts.push(`type ${label(MEMBERSHIP, rule.file_type.op)} ${[...kinds, ...extensions].join(' or ')}`);
  }
  if (rule.link) parts.push(textPart('link', rule.link));
  if (rule.found_on) parts.push(textPart('found on', rule.found_on));
  if (rule.added_from) parts.push(choicePart('added from', rule.added_from, CHOICES.added_from));
  if (rule.check) {
    const values = rule.check.values.map((v) => label(CHOICES.check, v).toLowerCase());
    parts.push(`check ${label(MEMBERSHIP, rule.check.op)} ${values.join(' or ')}`);
  }
  if (rule.route) parts.push(choicePart('downloaded', rule.route, CHOICES.route));
  const text = parts.join(' and ');
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/**
 * Whether `rule` has a condition only a check tells, so matching links
 * wait off the list until checked.
 * @param {LinkRule} rule
 */
export function waitsForCheck(rule) {
  return CHECKED_CONDITIONS.some((key) => rule[/** @type {keyof LinkRule} */ (key)] !== undefined);
}
