// What the views say about collected links: how entered text becomes
// entries, how a link is named and described, and what a command's
// refusals mean. No DOM, so it is tested
// under Node.

import { durationText } from './media.js';

/**
 * @typedef {import('./contract.js').CollectedLink} CollectedLink
 * @typedef {import('./contract.js').LinkId} LinkId
 * @typedef {import('./contract.js').Hello} Hello
 * @typedef {import('./jobs.js').Tone} Tone
 */

/**
 * The entries in pasted or typed text: one per whitespace-separated word,
 * since a link has no spaces. Blank lines are ignored.
 * @param {string} text
 */
export function entries(text) {
  return text.split(/\s+/).filter(Boolean);
}

/**
 * What stops `items` from being sent, within the engine's published
 * limits, or `null`. The engine checks again and has the final say.
 * @param {string[]} items
 * @param {Hello['limits']} limits
 */
export function entriesProblem(items, limits) {
  if (items.length === 0) return 'Paste or type at least one link.';
  if (items.length > limits.max_submit_items) {
    return `At most ${limits.max_submit_items} links at a time; this has ${items.length}.`;
  }
  const encoder = new TextEncoder();
  const long = items.findIndex((item) => encoder.encode(item).length > limits.max_url_bytes);
  if (long >= 0) return `Link ${long + 1} is longer than ${limits.max_url_bytes} bytes.`;
  return null;
}

/**
 * A link's name: a video's title once checked, the file name its path
 * suggests, the text of an invalid entry, or `null` when none is known. A
 * video page's path names no file, so it is not used.
 * @param {CollectedLink} link
 */
export function linkName(link) {
  if (link.media) return link.media.title ?? null;
  return link.name_hint ?? link.text ?? null;
}

/**
 * What a link's status column says. `Not checked` is all that is known
 * about a valid link: collecting never contacts its server. A video link,
 * such as YouTube's, is the exception: its service reads its title and
 * formats in the background. A started
 * link stays listed, added to Downloads, until removed.
 * @param {CollectedLink} link
 * @returns {{ label: string, tone: Tone, detail: string | null }}
 */
export function linkStatus(link) {
  if (link.job) return { label: 'Added to Downloads', tone: 'ok', detail: null };
  if (link.check === 'invalid') {
    return { label: 'Invalid link', tone: 'bad', detail: link.problem ?? null };
  }
  const media = link.media;
  if (media?.state === 'checking') return { label: 'Checking video', tone: 'calm', detail: null };
  if (media?.state === 'unavailable') {
    return { label: 'Video not read', tone: 'warn', detail: media.problem ?? null };
  }
  if (media) {
    const length = media.duration_seconds === undefined ? null : durationText(media.duration_seconds);
    return {
      label: 'Video found',
      tone: link.problem ? 'warn' : 'ok',
      detail: link.problem ? `Not started: ${link.problem}` : length,
    };
  }
  return {
    label: 'Not checked',
    tone: link.problem ? 'warn' : 'calm',
    detail: link.problem ? `Not started: ${link.problem}` : null,
  };
}

/**
 * Whether `link` can be started: valid, and not started already.
 * @param {CollectedLink} link
 */
export function startable(link) {
  return link.check !== 'invalid' && !link.job;
}

/**
 * Whether `link` matches filter text `query`, already lower case: by
 * name (a video's title), host, or problem.
 * @param {CollectedLink} link
 * @param {string} query
 */
export function matches(link, query) {
  if (query === '') return true;
  return [linkName(link), link.host, link.problem].some((text) => text?.toLowerCase().includes(query));
}

/**
 * `n` and a noun, singular or plural.
 * @param {number} n
 * @param {string} one
 * @param {string} [many]
 */
export function count(n, one, many = `${one}s`) {
  return `${n} ${n === 1 ? one : many}`;
}

/**
 * Why links, or other records, were refused, in one sentence per kind of
 * reason, so many refusals for one reason read as one line.
 * @param {readonly { message: string }[]} refused
 * @param {string} [many] what the records are called
 */
export function refusals(refused, many = 'links') {
  /** @type {Map<string, number>} */
  const reasons = new Map();
  for (const { message } of refused) reasons.set(message, (reasons.get(message) ?? 0) + 1);
  return [...reasons].map(([message, n]) => (n === 1 ? message : `${n} ${many}: ${message}`)).join(' ');
}

/**
 * When a link was added, for its Added at column: the time alone for
 * today, with the date otherwise.
 * @param {number} millis
 * @param {Date} [now]
 */
export function addedWhen(millis, now = new Date()) {
  const at = new Date(millis);
  const time = at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (at.toDateString() === now.toDateString()) return `Today, ${time}`;
  return `${at.toLocaleDateString([], { day: 'numeric', month: 'short', year: 'numeric' })}, ${time}`;
}
