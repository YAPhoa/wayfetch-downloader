import assert from 'node:assert/strict';
import { test } from 'node:test';

import { linkName, linkStatus, matches } from '../../src/client/links.js';
import {
  choiceKey,
  choiceLabel,
  durationText,
  mediaServiceName,
  pluginStatus,
  pluginUse,
} from '../../src/client/media.js';

/**
 * @param {Partial<import('../../src/client/contract.js').CollectedLink>} fields
 * @returns {import('../../src/client/contract.js').CollectedLink}
 */
function link(fields) {
  return {
    id: 'src-1',
    batch: 'batch-1',
    collected_at: 0,
    revision: 1,
    check: 'not_checked',
    host: 'www.youtube.com',
    name_hint: 'watch',
    ...fields,
  };
}

/** @type {import('../../src/client/generated/LinkMedia.js').LinkMedia} */
const READY = {
  service: 'youtube',
  state: 'ready',
  title: 'A synthetic video',
  duration_seconds: 3725,
  choice: { kind: 'video' },
  options: [
    { choice: { kind: 'video' }, available: true },
    { choice: { kind: 'video', max_height: 720 }, available: true },
    { choice: { kind: 'audio', format: 'mp3' }, available: false },
  ],
  needs: 'Some formats need the ffmpeg plugin, which is not available. See Settings > Plugins.',
};

test('choices have names and keys that tell them apart', () => {
  assert.equal(choiceLabel({ kind: 'video' }), 'Best video');
  assert.equal(choiceLabel({ kind: 'video', max_height: 1080 }), '1080p video');
  assert.equal(choiceLabel({ kind: 'audio', format: 'mp3' }), 'MP3 audio');
  assert.equal(choiceLabel({ kind: 'audio', format: 'm4a' }), 'M4A audio');
  const keys = READY.options.map((o) => choiceKey(o.choice));
  assert.deepEqual(keys, ['video-best', 'video-720', 'audio-mp3']);
});

test('lengths read as minutes or hours', () => {
  assert.equal(durationText(5), '0:05');
  assert.equal(durationText(125.4), '2:05');
  assert.equal(durationText(3725), '1:02:05');
});

test('a video link is named by its title, never by its page path', () => {
  assert.equal(linkName(link({ media: { ...READY, state: 'checking', title: undefined, options: [] } })), null);
  assert.equal(linkName(link({ media: READY })), 'A synthetic video');
  assert.equal(linkName(link({})), 'watch');
  assert.ok(matches(link({ media: READY }), 'synthetic'));
});

test('a video link says whether it was read', () => {
  assert.deepEqual(linkStatus(link({ media: { ...READY, state: 'checking', options: [] } })), {
    label: 'Checking video',
    tone: 'calm',
    detail: null,
  });
  assert.deepEqual(linkStatus(link({ media: READY })), { label: 'Video found', tone: 'ok', detail: '1:02:05' });
  assert.deepEqual(linkStatus(link({ media: { ...READY, state: 'unavailable', problem: 'Private video' } })), {
    label: 'Video not read',
    tone: 'warn',
    detail: 'Private video',
  });
  assert.equal(linkStatus(link({ media: READY, problem: 'busy' })).detail, 'Not started: busy');
  assert.equal(linkStatus(link({ media: READY, job: 'job-1' })).label, 'Added to Downloads');
});

test('plugins say whether they work and what for', () => {
  /** @type {import('../../src/client/contract.js').PluginSummary} */
  const ytdlp = {
    id: 'yt-dlp',
    name: 'yt-dlp',
    role: 'media_resolver',
    services: ['youtube'],
    state: 'ready',
    version: '2026.09.30',
    enabled: true,
    updatable: true,
    installed: false,
  };
  assert.deepEqual(pluginStatus(ytdlp), ['Ready', 'ok']);
  assert.deepEqual(pluginStatus({ ...ytdlp, enabled: false, state: 'missing' }), ['Turned off', 'calm']);
  assert.deepEqual(pluginStatus({ ...ytdlp, state: 'missing' }), ['Not installed', 'warn']);
  assert.deepEqual(pluginStatus({ ...ytdlp, state: 'failed' }), ['Not working', 'bad']);
  assert.equal(pluginUse(ytdlp), 'Reads video titles and formats, for YouTube');
  assert.equal(
    pluginUse({ ...ytdlp, id: 'ffmpeg', role: 'media_converter', services: ['youtube', 'other'] }),
    'Joins video with its sound and makes MP3 files, for YouTube, other',
  );
  assert.equal(mediaServiceName('toString'), 'toString');
});
