// The bundled UI in a real browser against a real daemon: owner setup,
// downloads of local fixtures, pause, resume, cancel, reloading and closing
// the tab, a daemon restart, and sign-out, from a fresh profile with no
// WAN access.

import { expect, test } from '@playwright/test';

import { Daemon, Fixture, MIB, content, freePort } from './harness.js';

const PASSWORD = 'correct horse battery staple';
/** A file name that is markup if mistaken for HTML, and fine on disk. */
const TRICKY = `report &lt;img src=x onerror=window.__xss=1&gt; & 'q'.bin`;
const SIZE = 20 * MIB + 11;
/** Past two 8 MiB checkpoints, so at least 16 MiB is durable while it stalls. */
const STALL = 17 * MIB + 5;

/** @type {Fixture} */
let fixture;
/** @type {Daemon} */
let daemon;

test.beforeAll(async () => {
  fixture = await new Fixture().start();
  daemon = await new Daemon(await freePort()).init();
  await daemon.start();
});

test.afterAll(async () => {
  await daemon?.remove();
  await fixture?.stop();
});

/**
 * @param {import('@playwright/test').Page} page
 * @param {string} job
 */
function row(page, job) {
  return page.locator(`tr[data-job="${job}"]`);
}

/**
 * @param {import('@playwright/test').Page} page
 * @param {string} job
 * @param {string} state
 */
async function expectState(page, job, state) {
  await expect(row(page, job).locator('td[data-state]')).toHaveText(state);
}

/**
 * Selects a job, so the toolbar acts on it.
 * @param {import('@playwright/test').Page} page
 * @param {string} job
 */
async function select(page, job) {
  await row(page, job).locator('td.name a').click();
  await expect(page).toHaveURL(`${daemon.origin}/jobs/${job}`);
}

/**
 * @param {import('@playwright/test').Page} page
 * @param {string} url
 */
async function add(page, url) {
  const links = page.getByLabel('Links to download, one per line');
  if (!(await links.isVisible())) await page.getByRole('button', { name: 'Add links' }).click();
  await links.fill(url);
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(page.getByText('Added 1 download.')).toBeVisible();
}

test('a browser signs in, downloads, controls jobs, and outlives a daemon restart', async ({
  browser,
}) => {
  // A fresh profile: no cookies, no cache.
  const context = await browser.newContext();
  /** @type {string[]} */
  const offOrigin = [];
  context.on('request', (request) => {
    if (!request.url().startsWith(`${daemon.origin}/`)) offOrigin.push(request.url());
  });
  /** @type {string[]} */
  const dialogs = [];
  context.on('dialog', (dialog) => {
    dialogs.push(dialog.message());
    void dialog.dismiss();
  });
  let page = await context.newPage();

  // First run: the owner is created with the setup secret.
  await page.goto(daemon.origin);
  await expect(page.getByRole('heading', { name: 'Set up Wayfetch' })).toBeVisible();
  await page.getByLabel('Setup secret').fill((await daemon.setupSecret()).trim());
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
  await page.getByLabel('Repeat password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Create owner' }).click();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
  await expect(page.getByText('No downloads yet.')).toBeVisible();
  // The bundled typeface loads from the daemon.
  const loaded = await page.evaluate(async () => {
    await document.fonts.ready;
    return [...document.fonts]
      .filter((font) => font.family.includes('IBM Plex Sans') && font.status === 'loaded')
      .map((font) => font.weight);
  });
  expect(loaded).toContain('400');

  // A download that stalls after two checkpoints.
  await add(page, fixture.put(TRICKY, SIZE, STALL));
  await expectState(page, 'job-1', 'Downloading');
  await expect(row(page, 'job-1').locator('.bytes')).toHaveText(/^1[67]\.\d MiB of 20\.0 MiB$/);

  // Pause and resume the selected job through the API's commands.
  await select(page, 'job-1');
  await page.getByRole('button', { name: 'Pause job-1' }).click();
  await expectState(page, 'job-1', 'Paused');
  await page.getByRole('button', { name: 'Resume job-1' }).click();
  await expectState(page, 'job-1', 'Downloading');

  // Reloading or closing the tab changes nothing about the download.
  await page.reload();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
  await expectState(page, 'job-1', 'Downloading');
  await page.close();
  page = await context.newPage();
  await page.goto(daemon.origin);
  await expectState(page, 'job-1', 'Downloading');

  // The daemon restarts: the page notices, and since sessions live in
  // memory, asks to sign in again; the download carries on.
  await daemon.stop();
  await expect(page.locator('.statusbar .status')).toHaveText('Reconnecting…');
  await expect(page.getByRole('alert').filter({ hasText: 'Downloads continue on the server' })).toBeVisible();
  await daemon.start();
  await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
  await expect(page.getByText('Your session ended')).toBeVisible();
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
  await expectState(page, 'job-1', 'Downloading');

  // It completes once the server sends the rest, under its literal name.
  fixture.release(TRICKY);
  await expectState(page, 'job-1', 'Done');
  await expect(row(page, 'job-1').locator('td.name')).toHaveText(TRICKY);
  // The host of the link, not the link.
  await expect(row(page, 'job-1').locator('td.host')).toHaveText('127.0.0.1');
  expect((await daemon.published(TRICKY)).equals(content(0, SIZE))).toBe(true);

  // Its details, also after a reload of its address.
  await select(page, 'job-1');
  await expect(page.locator('dd[data-fact="file"]')).toHaveText(TRICKY);
  await page.reload();
  await expect(page.locator('dd[data-fact="state"]')).toHaveText('Done');
  await expect(page.locator('dd[data-fact="size"]')).toHaveText('20.0 MiB');
  await page.getByRole('tab', { name: 'Engine details' }).click();
  await expect(page.locator('dd[data-fact="job"]')).toHaveText('job-1');
  await page.getByRole('button', { name: 'Close details' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/`);

  // The state filters list only matching jobs, with how many there are,
  // and keep keyboard focus while the list reloads.
  await expect(page.getByRole('button', { name: 'All (1)' })).toBeVisible();
  const done = page.getByRole('button', { name: 'Done (1)' });
  await done.focus();
  await page.keyboard.press('Enter');
  await expect(done).toHaveAttribute('aria-pressed', 'true');
  await expectState(page, 'job-1', 'Done');
  await expect(done).toBeFocused();
  await page.getByRole('button', { name: 'Active (0)' }).click();
  await expect(page.getByText('No downloads in this state.')).toBeVisible();
  await page.getByRole('button', { name: /^All \(/ }).click();

  // Cancel a second download mid-transfer.
  await add(page, fixture.put('second.bin', 10 * MIB, 9 * MIB));
  await expect(row(page, 'job-2').locator('.bytes')).toHaveText(/^[89]\.\d MiB of 10\.0 MiB$/);
  await select(page, 'job-2');
  await page.getByRole('button', { name: 'Cancel job-2' }).click();
  await expectState(page, 'job-2', 'Cancelled');
  await expect(page.getByRole('button', { name: 'Cancel job-2' })).toBeDisabled();
  // The counts follow the change.
  await expect(page.getByRole('button', { name: 'Cancelled (1)' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'All (2)' })).toBeVisible();

  // Text from downloads stayed text.
  expect(await page.locator('img').count()).toBe(0);
  expect(await page.evaluate(() => /** @type {any} */ (window).__xss)).toBeUndefined();
  expect(dialogs).toEqual([]);

  // Signing out in another tab ends this tab's session as well.
  const other = await context.newPage();
  await other.goto(daemon.origin);
  await other.getByRole('button', { name: 'Sign out' }).click();
  await expect(other.getByText('You are signed out.')).toBeVisible();
  await expect(page.getByText('Your session ended')).toBeVisible();
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expectState(page, 'job-2', 'Cancelled');

  // A sign-out that fails says so, keeps the session, and can be retried.
  await page.route('**/v1/auth/logout', (route) => route.abort());
  await page.getByRole('button', { name: 'Sign out' }).click();
  await expect(page.getByRole('alert').filter({ hasText: 'Signing out failed' })).toBeVisible();
  await page.reload();
  await expectState(page, 'job-2', 'Cancelled');
  await page.unroute('**/v1/auth/logout');

  // Signing out ends the session; the jobs stay.
  await page.getByRole('button', { name: 'Sign out' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
  const jobs = await fetch(`${daemon.origin}/v1/jobs`);
  expect(jobs.status).toBe(401);

  // Everything came from the daemon itself.
  expect(offOrigin).toEqual([]);
  await context.close();
});
