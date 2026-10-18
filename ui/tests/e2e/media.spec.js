// A YouTube link read by a stand-in yt-dlp: no request leaves loopback. The
// stand-in answers in yt-dlp's shape with formats the fixture server
// serves, and ffmpeg is missing, so only formats with sound included can
// be made.

import { chmod, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { expect, test } from '@playwright/test';

import { Daemon, Fixture, MIB, content, freePort } from './harness.js';

const PASSWORD = 'correct horse battery staple';
const VIDEO = 'https://www.youtube.com/watch?v=abcdefghijk&t=42s';

/**
 * A stand-in for yt-dlp in `dir` that describes one video whose formats
 * are `formats`.
 * @param {string} dir
 * @param {object[]} formats
 */
async function fakeYtDlp(dir, formats) {
  const video = {
    id: 'abcdefghijk',
    title: 'Wayfetch test video',
    duration: 61,
    live_status: 'not_live',
    _type: 'video',
    formats,
  };
  const path = join(dir, 'yt-dlp');
  await writeFile(
    path,
    `#!${process.execPath}\n` +
      `const args = process.argv.slice(2);\n` +
      `if (args.includes('--version')) console.log('2099.01.01');\n` +
      `else if (args.at(-1) !== ${JSON.stringify('https://www.youtube.com/watch?v=abcdefghijk')}) process.exit(2);\n` +
      `else console.log(${JSON.stringify(JSON.stringify(video))});\n`,
  );
  await chmod(path, 0o755);
  return path;
}

test('a YouTube link shows its title and formats, and downloads the one chosen', async ({ page }) => {
  const fixture = await new Fixture().start();
  const plugins = await mkdtemp(join(tmpdir(), 'wayfetch-plugins-'));
  const size = 3 * MIB;
  // The HTTP fixture stands in for YouTube's HTTPS servers.
  /** @param {string} id @param {string} vcodec @param {string} acodec @param {number | null} height */
  const format = (id, vcodec, acodec, height) => ({
    format_id: id,
    url: `${fixture.put(`yt-${id}.mp4`, size)}?sig=secret`,
    protocol: 'http',
    ext: id === '140' ? 'm4a' : 'mp4',
    vcodec,
    acodec,
    height,
    filesize: size,
    tbr: height ?? 128,
  });
  const formats = [format('18', 'avc1', 'mp4a', 360), format('137', 'avc1', 'none', 1080), format('140', 'none', 'mp4a', null)];
  const daemon = await new Daemon(await freePort(), {
    plugins: { ytDlp: await fakeYtDlp(plugins, formats), ffmpeg: join(plugins, 'no-ffmpeg') },
  }).init();
  try {
    await daemon.start();
    await page.goto(daemon.origin);
    await page.getByLabel('Setup secret').fill((await daemon.setupSecret()).trim());
    await page.getByLabel('Owner name').fill('owner');
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
    await page.getByLabel('Repeat password').fill(PASSWORD);
    await page.getByRole('button', { name: 'Create owner' }).click();
    await expect(page.locator('.statusbar .status')).toHaveText('Live');

    // Settings > Plugins says what each plugin is and whether it runs.
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByRole('link', { name: 'Plugins' }).click();
    await expect(page).toHaveURL(`${daemon.origin}/settings/plugins`);
    const ytDlp = page.locator('tr[data-plugin="yt-dlp"]');
    await expect(ytDlp.locator('td.version')).toHaveText('2099.01.01');
    await expect(ytDlp.locator('td.status')).toHaveText('Ready');
    await expect(ytDlp.locator('td.use')).toHaveText('Reads video titles and formats, for YouTube');
    await expect(page.locator('tr[data-plugin="ffmpeg"] td.status')).toContainText('Not installed');
    await page.goBack();
    await page.goBack();

    // Collected, the link is read for its title and formats; nothing is
    // downloaded yet.
    await page.getByRole('button', { name: 'Add links' }).click();
    await page.getByLabel('Links, one per line').fill(VIDEO);
    await page.getByRole('button', { name: 'Collect links' }).click();
    await expect(page).toHaveURL(`${daemon.origin}/links`);
    const row = page.locator('tr[data-link]');
    await expect(row.locator('td.name')).toHaveText('Wayfetch test video');
    await expect(row.locator('td.link-status')).toContainText('Video found');
    expect(fixture.requested).toEqual([]);

    // Without ffmpeg, MP3 and the 1080p picture need a plugin; Best video
    // is the format with sound included.
    const select = row.locator('select[data-key^="link-format-"]');
    await expect(select).toHaveAccessibleName('Format of Wayfetch test video');
    await expect(select.locator('option:checked')).toHaveText('Best video');
    await expect(select.locator('option', { hasText: 'MP3 audio' })).toBeDisabled();
    await expect(select.locator('option', { hasText: 'MP3 audio' })).toHaveText('MP3 audio (needs a plugin)');
    await select.selectOption({ label: '360p video' });
    await expect(page.getByText('1 video will download as 360p video.')).toBeVisible();
    await page.reload();
    await expect(row.locator('select option:checked')).toHaveText('360p video');

    await row.getByRole('checkbox').check();
    await page.getByRole('button', { name: 'Start selected' }).click();
    await expect(page.getByText('Started 1 download.')).toBeVisible();
    await page.getByRole('button', { name: 'View downloads' }).click();
    const job = page.locator('tr[data-job]');
    await expect(job.locator('td.name')).toHaveText('Wayfetch test video.mp4');
    await expect(job.locator('td[data-state]')).toHaveText('Done');
    expect(await daemon.published('Wayfetch test video.mp4')).toEqual(content(0, size));
    // Only the chosen stream was fetched, by the engine, in ranges.
    expect(new Set(fixture.requested)).toEqual(new Set(['yt-18.mp4']));
  } finally {
    await daemon.remove();
    await rm(plugins, { recursive: true, force: true });
    fixture.server.close();
  }
});
