// What the views say about media links, such as YouTube videos, and the
// plugins that read and convert them: a choice's name, a length, and a
// plugin's state. No DOM, so it is tested under Node.

/**
 * @typedef {import('./generated/MediaChoice.js').MediaChoice} MediaChoice
 * @typedef {import('./generated/MediaOption.js').MediaOption} MediaOption
 * @typedef {import('./generated/PluginSummary.js').PluginSummary} PluginSummary
 * @typedef {import('./jobs.js').Tone} Tone
 */

/**
 * The names of the media services the engine may report, by ID. A service
 * not listed shows by its ID.
 * @type {Readonly<Record<string, string>>}
 */
export const MEDIA_SERVICES = Object.freeze({ youtube: 'YouTube' });

/**
 * A media service's name.
 * @param {string} id
 */
export function mediaServiceName(id) {
  return Object.hasOwn(MEDIA_SERVICES, id) ? MEDIA_SERVICES[id] : id;
}

/**
 * A choice's name, as the format list shows it.
 * @param {MediaChoice} choice
 */
export function choiceLabel(choice) {
  if (choice.kind === 'audio') return `${choice.format.toUpperCase()} audio`;
  return choice.max_height === undefined ? 'Best video' : `${choice.max_height}p video`;
}

/**
 * A key naming `choice`, equal for equal choices.
 * @param {MediaChoice} choice
 */
export function choiceKey(choice) {
  return choice.kind === 'audio' ? `audio-${choice.format}` : `video-${choice.max_height ?? 'best'}`;
}

/**
 * A length in seconds as `m:ss` or `h:mm:ss`.
 * @param {number} seconds
 */
export function durationText(seconds) {
  const s = Math.max(0, Math.round(seconds));
  const hours = Math.floor(s / 3600);
  const minutes = Math.floor((s % 3600) / 60);
  const rest = String(s % 60).padStart(2, '0');
  return hours > 0 ? `${hours}:${String(minutes).padStart(2, '0')}:${rest}` : `${minutes}:${rest}`;
}

/**
 * What a plugin's state says, and its tone.
 * @param {PluginSummary} plugin
 * @returns {[string, Tone]}
 */
export function pluginStatus(plugin) {
  if (!plugin.enabled) return ['Turned off', 'calm'];
  switch (plugin.state) {
    case 'ready':
      return ['Ready', 'ok'];
    case 'missing':
      return ['Not installed', 'warn'];
    default:
      return ['Not working', 'bad'];
  }
}

/**
 * What a plugin is used for.
 * @param {PluginSummary} plugin
 */
export function pluginUse(plugin) {
  const what =
    plugin.role === 'media_converter'
      ? 'Joins video with its sound and makes MP3 files'
      : 'Reads video titles and formats';
  const services = plugin.services.map(mediaServiceName);
  return services.length > 0 ? `${what}, for ${services.join(', ')}` : what;
}
