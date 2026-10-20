// What the views say about collected links: what stops text from being
// collected, how a link is named and described, and what a command's
// refusals mean. No DOM, so it is tested
// under Node.

import { durationText } from './media.js';

/**
 * @typedef {import('./contract.js').CollectedLink} CollectedLink
 * @typedef {import('./contract.js').LinkId} LinkId
 * @typedef {import('./contract.js').Hello} Hello
 * @typedef {import('./contract.js').CheckReason} CheckReason
 * @typedef {import('./jobs.js').Tone} Tone
 */

/**
 * What stops `text` from being sent to collect, within the engine's
 * published limits, or `null`. The engine finds the links in it and has
 * the final say.
 * @param {string} text
 * @param {Hello['limits']} limits
 */
export function textProblem(text, limits) {
  if (text.trim() === '') return 'Paste or type at least one link.';
  const bytes = new TextEncoder().encode(text).length;
  if (bytes > limits.max_collect_text_bytes) {
    return `The text is longer than ${Math.floor(limits.max_collect_text_bytes / 1024)} KiB; collect it in parts.`;
  }
  return null;
}

/** What each reason a check gives means. @type {Record<CheckReason, string>} */
const REASONS = {
  not_found: 'The server says it does not exist.',
  access_denied: 'The server refused access.',
  rate_limited: 'The server asked to wait before asking again.',
  timed_out: 'The server did not answer in time.',
  unreachable: 'The server could not be reached.',
  blocked: 'Wayfetch is not allowed to connect to this address.',
  server_error: 'The server had an error.',
  unreadable: 'The server’s answer could not be read.',
  too_large: 'The page is too large to read.',
};

/**
 * Why a check found `link` offline or could not tell, with what the
 * server said, or `null`. A provider's check says it in its own words.
 * @param {CollectedLink} link
 */
export function checkReason(link) {
  const parts = [!link.checked_via && link.reason && REASONS[link.reason], link.check_detail].filter(Boolean);
  return parts.length > 0 ? parts.join(' ') : null;
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
 * What a link's status column says. Collected links are checked in the
 * background: a file's server is asked for its name and size, never its
 * content, and a page typed or analyzed is read once for the files on
 * it, which are listed beside it. A video link's service reads its title
 * and formats instead. A link an account would download is checked by
 * that account's provider; when the provider cannot tell, it is not a
 * failure, since starting goes through the account anyway. A started
 * link stays listed, added to Downloads, until removed.
 * @param {CollectedLink} link
 * @param {(service: string) => string} [serviceName] A service's display name.
 * @returns {{ label: string, tone: Tone, detail: string | null }}
 */
export function linkStatus(link, serviceName = (id) => id) {
  if (link.job) return { label: 'Added to Downloads', tone: 'ok', detail: null };
  if (link.check === 'invalid') {
    return { label: 'Invalid link', tone: 'bad', detail: link.problem ?? null };
  }
  const refused = link.problem ? `Not started: ${link.problem}` : null;
  const media = link.media;
  if (media?.state === 'checking') return { label: 'Checking video', tone: 'calm', detail: null };
  if (media?.state === 'unavailable') {
    return { label: 'Video not read', tone: 'warn', detail: media.problem ?? null };
  }
  if (media) {
    const length = media.duration_seconds === undefined ? null : durationText(media.duration_seconds);
    return { label: 'Video found', tone: refused ? 'warn' : 'ok', detail: refused ?? length };
  }
  const via = link.checked_via ? serviceName(link.checked_via) : null;
  switch (link.check) {
    case 'checking':
      return { label: 'Checking', tone: 'calm', detail: null };
    case 'online':
      return { label: 'Online', tone: refused ? 'warn' : 'ok', detail: refused ?? (via && `Checked through ${via}.`) };
    case 'offline':
      return { label: 'Offline', tone: 'bad', detail: checkReason(link) };
    case 'unknown':
      if (via) {
        const why = [checkReason(link), `Starting downloads it through ${via}.`].filter(Boolean).join(' ');
        return { label: 'Not confirmed', tone: refused ? 'warn' : 'calm', detail: refused ?? why };
      }
      return { label: 'Could not check', tone: 'warn', detail: checkReason(link) };
    case 'page':
      return { label: 'Web page', tone: 'calm', detail: pageDetail(link) };
    default:
      return { label: 'Not checked', tone: refused ? 'warn' : 'calm', detail: refused };
  }
}

/**
 * What reading a page found, or that it was not read.
 * @param {CollectedLink} link
 */
function pageDetail(link) {
  if (link.found === undefined) return 'Not read. Analyze it to find the files on it.';
  const found = link.found === 0 ? 'No files found on it.' : `${count(link.found, 'file')} found on it, listed below it.`;
  const skipped = link.skipped ? ` ${count(link.skipped, 'link')} to other pages left alone.` : '';
  const more = link.check_detail ? ` ${link.check_detail}` : '';
  return `${found}${skipped}${more}`;
}

/**
 * Whether `link` can be started: valid and not started already. A web
 * page starts only through a provider account, which the engine decides
 * when starting, and refuses otherwise with the reason.
 * @param {CollectedLink} link
 */
export function startable(link) {
  return link.check !== 'invalid' && !link.job;
}

/**
 * Whether `link` can be checked again, or analyzed: valid and not started.
 * @param {CollectedLink} link
 */
export function checkable(link) {
  return link.check !== 'invalid' && !link.job;
}

/**
 * Whether `link` matches filter text `query`, already lower case: by
 * name (a video's title), host, problem, or the page it was found on.
 * @param {CollectedLink} link
 * @param {string} query
 */
export function matches(link, query) {
  if (query === '') return true;
  return [linkName(link), link.host, link.problem, link.found_on].some((text) => text?.toLowerCase().includes(query));
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
