// The bundled UI in a real browser against a real daemon: owner setup,
// downloads of local fixtures, pause, resume, cancel, reloading and closing
// the tab, a daemon restart, and sign-out, from a fresh profile with no
// WAN access.

import { expect, test } from '@playwright/test';

import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

import {
  Daemon,
  Fixture,
  MIB,
  RD_SECRETS,
  RD_SIGNATURE,
  RD_USER_CODE,
  RealDebridFixture,
  content,
  freePort,
} from './harness.js';

const PASSWORD = 'correct horse battery staple';
/** A file name that is markup if mistaken for HTML, and fine on disk. */
const TRICKY = `report &lt;img src=x onerror=window.__xss=1&gt; & 'q'.bin`;
const SOURCE_NAME = 'early-source.bin';
const SOURCE_SUFFIX = '?private-token=secret#private-fragment';
const SIZE = 20 * MIB + 11;
/** Past two 8 MiB checkpoints, so at least 16 MiB is durable while it stalls. */
const STALL = 17 * MIB + 5;

/** @type {Fixture} */
let fixture;
/** @type {RealDebridFixture} */
let realDebrid;
/** @type {Daemon} */
let daemon;

test.beforeAll(async () => {
  fixture = await new Fixture().start();
  realDebrid = await new RealDebridFixture().start();
  // GitHub's answer for the newest release, from the same stand-in.
  fixture.page(
    'releases/latest',
    JSON.stringify({ tag_name: 'v99.0.0', html_url: `${fixture.base}/pages/releases/tag/v99.0.0` }),
  );
  daemon = await new Daemon(await freePort(), {
    realDebrid: realDebrid.base,
    updates: `${fixture.base}/pages/`,
  }).init();
  await daemon.start();
});

test.afterAll(async () => {
  await daemon?.remove();
  await fixture?.stop();
  await realDebrid?.stop();
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
 * Collects `text` through Add links, then starts its batch from the Links
 * page and goes back to Downloads.
 * @param {import('@playwright/test').Page} page
 * @param {string} text
 * @param {number} [count]
 */
async function add(page, text, count = 1) {
  const links = count === 1 ? '1 link' : `${count} links`;
  await collect(page, text, `Collected ${links} for review.`);
  // The links just collected are selected.
  await expect(page.locator('.links-page .selection-summary strong')).toHaveText(`${links} selected`);
  await page.getByRole('button', { name: 'Start selected' }).click();
  // Starting switches to Downloads, which says what started.
  await expect(page).toHaveURL(`${daemon.origin}/`);
  await expect(page.getByText(`Started ${count === 1 ? '1 download' : `${count} downloads`}.`)).toBeVisible();
}

/**
 * Pastes `text` into Add links and collects it, landing on the Links page.
 * @param {import('@playwright/test').Page} page
 * @param {string} text
 * @param {string} collected what the page then says
 */
async function collect(page, text, collected) {
  await page.getByRole('button', { name: 'Add links' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/add`);
  await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(text);
  await page.getByRole('button', { name: 'Collect links' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/links`);
  await expect(page.getByText(collected)).toBeVisible();
}

/**
 * The page itself never scrolls: the screen fits the window, and long
 * lists scroll inside the table.
 * @param {import('@playwright/test').Page} page
 */
async function expectFitsWindow(page) {
  const [scrolled, window] = await page.evaluate(() => [
    document.documentElement.scrollHeight,
    document.documentElement.clientHeight,
  ]);
  expect(scrolled).toBe(window);
}

/**
 * The service catalogue, opened to add an account from the Accounts &
 * services section: Real-Debrid first, search by name and address,
 * categories from the keyboard, the no-results state, only Real-Debrid
 * offered for signing in, and a narrow window. Focus returns to Add account
 * when it closes.
 * @param {import('@playwright/test').Page} page
 * @param {import('@playwright/test').Locator} accounts
 */
async function expectServiceCatalogue(page, accounts) {
  const adding = accounts.getByRole('button', { name: 'Add account…' });
  await adding.click();
  const catalogue = page.getByRole('dialog', { name: 'Add account' });
  const search = catalogue.getByRole('searchbox', { name: 'Search services' });
  const status = catalogue.getByRole('status');
  const names = catalogue.locator('.service-card h4');
  const chips = catalogue.locator('.chip');
  await expect(search).toBeFocused();
  await expect(status).toHaveText('20 services');
  await expect(chips).toHaveText(['All20', 'Multi-host services6', 'File hosting8', 'Cloud storage6']);
  await expect(catalogue.getByRole('heading', { level: 3 })).toHaveText([
    'Multi-host services',
    'File hosting',
    'Cloud storage',
  ]);
  // Real-Debrid leads, as the first planned integration.
  const featured = catalogue.locator('.service-card.featured');
  await expect(featured.locator('h4')).toHaveText('Real-Debrid');
  // Downloads go through it too, so it is not marked Sign-in only.
  await expect(featured.locator('.tag')).toHaveText(['Multi-host services', 'Available']);
  await expect(names.first()).toHaveText('Real-Debrid');
  // Only Real-Debrid can be signed in to, and nothing asks for a password.
  await expect(catalogue.getByRole('button')).toHaveText(['×', 'Sign in…']);
  await expect(featured.getByRole('button', { name: 'Sign in to Real-Debrid' })).toBeVisible();
  await expect(featured.getByText(/cannot download through it yet/)).toHaveCount(0);
  await expect(catalogue.locator('input:not([type="search"], [type="radio"]), select, textarea')).toHaveCount(0);
  await expect(catalogue.locator('.service-card .tag', { hasText: 'Not available yet' })).toHaveCount(19);
  await expect(catalogue.locator('.service-card .tag.available')).toHaveCount(1);
  // By name, ignoring case and punctuation.
  await search.fill('real debrid');
  await expect(names).toHaveText(['Real-Debrid']);
  await expect(status).toHaveText('1 service');
  // By an alias, or a pasted link: one entry, saying which domain matched.
  await search.fill('https://rg.to/file/abc/name.rar.html');
  await expect(names).toHaveText(['Rapidgator']);
  await expect(catalogue.getByText('Matches rg.to')).toBeVisible();
  await search.fill('mega');
  // Name matches first, ungrouped while searching.
  await expect(names).toHaveText(['MEGA', '1fichier']);
  await expect(catalogue.getByRole('heading', { level: 3 })).toHaveCount(0);
  await expect(catalogue.getByText('Matches megadl.fr')).toBeVisible();
  await expect(chips).toHaveText(['All2', 'Multi-host services0', 'File hosting1', 'Cloud storage1']);
  // A category from the keyboard: Tab reaches the selected one, arrows
  // move between them.
  await search.fill('');
  await page.keyboard.press('Tab');
  await expect(catalogue.getByRole('radio', { name: /^All/ })).toBeFocused();
  expect(await chips.first().evaluate((el) => getComputedStyle(el).outlineStyle)).toBe('solid');
  await page.keyboard.press('ArrowRight');
  await expect(catalogue.getByRole('radio', { name: /^Multi-host services/ })).toBeChecked();
  await expect(names).toHaveText(['Real-Debrid', 'AllDebrid', 'TorBox', 'Debrid-Link', 'LinkSnappy', 'premium.to']);
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('ArrowRight');
  await expect(catalogue.getByRole('radio', { name: /^Cloud storage/ })).toBeChecked();
  await expect(names).toHaveText(['MEGA', 'Google Drive', 'Dropbox', 'MediaFire', 'Pixeldrain', 'Gofile']);
  // The list scrolls from the keyboard too.
  await page.keyboard.press('Tab');
  await expect(catalogue.getByRole('region', { name: 'Services' })).toBeFocused();
  // Nothing matches: said plainly, in this category and in all.
  await search.fill('rapidgator');
  await expect(names).toHaveCount(0);
  await expect(catalogue.getByText('No services match “rapidgator” in Cloud storage.')).toBeVisible();
  await search.fill('no such service');
  await chips.first().click();
  await expect(catalogue.getByRole('radio', { name: /^All/ })).toBeChecked();
  await expect(catalogue.getByText('No services match “no such service”.')).toBeVisible();
  await expect(status).toHaveText('');
  // A narrow window: the dialog fits, its list scrolls inside it.
  await search.fill('');
  await page.setViewportSize({ width: 360, height: 640 });
  const box = await catalogue.boundingBox();
  expect(box && box.x >= 0 && box.y >= 0 && box.x + box.width <= 360 && box.y + box.height <= 640).toBe(true);
  const results = catalogue.locator('.service-results');
  expect(await results.evaluate((el) => el.scrollHeight > el.clientHeight)).toBe(true);
  expect(await catalogue.evaluate((el) => el.scrollWidth <= el.clientWidth)).toBe(true);
  await page.setViewportSize({ width: 1280, height: 720 });
  // The dark theme has its own surfaces.
  const card = catalogue.locator('.service-card').first();
  const surface = () => card.evaluate((el) => getComputedStyle(el).backgroundColor);
  expect(await surface()).toBe('rgb(255, 255, 255)');
  await page.emulateMedia({ colorScheme: 'dark' });
  await expect.poll(surface).toBe('rgb(15, 19, 24)');
  await page.emulateMedia({ colorScheme: 'light' });
  // Escape clears a search, then closes it and stays in Settings, back on
  // Add account.
  await search.fill('rg.to');
  await page.keyboard.press('Escape');
  await expect(search).toHaveValue('');
  await expect(catalogue).toBeVisible();
  await expect(status).toHaveText('20 services');
  await page.keyboard.press('Escape');
  await expect(catalogue).toHaveCount(0);
  await expect(page).toHaveURL(`${daemon.origin}/settings/accounts`);
  await expect(adding).toBeFocused();
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
  await expectFitsWindow(page);
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
  await add(page, `${fixture.put(SOURCE_NAME, SIZE, STALL, TRICKY)}${SOURCE_SUFFIX}`);
  await expectState(page, 'job-1', 'Downloading');
  await expect(row(page, 'job-1').locator('.bytes')).toHaveText(/^1[67]\.\d MiB of 20\.0 MiB$/);
  // Response metadata updates the name while bytes are still in flight.
  await expect(row(page, 'job-1').locator('td.name')).toHaveText(TRICKY);
  await select(page, 'job-1');
  await expect(page.locator('dd[data-fact="file"]')).toHaveText(TRICKY);
  const search = page.locator('#filter-names');
  await search.fill('report');
  await expect(row(page, 'job-1')).toBeVisible();
  await search.fill('early-source');
  await expect(row(page, 'job-1')).toHaveCount(0);
  await search.fill('');

  // Pause and resume the selected job through the API's commands.
  await select(page, 'job-1');
  await page.getByRole('button', { name: `Pause ${TRICKY}` }).click();
  await expectState(page, 'job-1', 'Paused');
  await page.getByRole('button', { name: `Resume ${TRICKY}` }).click();
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
  await expectFitsWindow(page);
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
  await expectState(page, 'job-1', 'Downloading');

  // It completes once the server sends the rest, under its literal name.
  fixture.release(SOURCE_NAME);
  await expectState(page, 'job-1', 'Done');
  await expect(row(page, 'job-1').locator('td.name')).toHaveText(TRICKY);
  // The host of the link, not the link.
  await expect(row(page, 'job-1').locator('td.host')).toHaveText('127.0.0.1');
  expect((await daemon.published(TRICKY)).equals(content(0, SIZE))).toBe(true);

  // Its details, also after a reload of its address.
  await select(page, 'job-1');
  // What the file is, where it came from, and where it is: a server
  // folder, which a browser cannot open.
  await expect(page.locator('dd[data-fact="file"] .path')).toHaveText(TRICKY);
  await expect(page.locator('dd[data-fact="file"] .path')).toHaveAttribute('title', `${daemon.root}/${TRICKY}`);
  await expect(page.locator('dd[data-fact="type"]')).toHaveText('BIN file');
  await expect(page.locator('dd[data-fact="saved-in"] .path')).toHaveText(/** @type {string} */ (daemon.root));
  await expect(page.locator('dd[data-fact="saved-in"]').getByText('Server path')).toBeVisible();
  await expect(page.locator('dd[data-fact="saved-in"] button')).toHaveCount(0);
  await expect(page.locator('dd[data-fact="source"] .path')).toHaveText(
    `${fixture.base}/files/${SOURCE_NAME}?…`,
  );
  await page.reload();
  await expect(page.locator('dd[data-fact="state"]')).toHaveText('Done');
  await expect(page.locator('dd[data-fact="size"]')).toHaveText('20.0 MiB');
  await page.getByRole('tab', { name: 'Engine details' }).click();
  await expect(page.locator('dd[data-fact="job"]')).toHaveText('job-1');
  await page.getByRole('button', { name: 'Close details' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/`);
  // With nothing chosen the details pane is gone, not a placeholder.
  await expect(page.locator('.inspector')).toBeHidden();

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
  // While only cancelled jobs are listed, the job joins the list once cancelled.
  await page.getByRole('button', { name: /^Cancelled \(/ }).click();
  await expect(page.getByText('No downloads in this state.')).toBeVisible();
  await page.getByRole('button', { name: 'Cancel second.bin' }).click();
  await expectState(page, 'job-2', 'Cancelled');
  await expect(page.getByRole('button', { name: 'Cancel second.bin' })).toBeDisabled();
  await page.getByRole('button', { name: /^All \(/ }).click();
  // The counts follow the change.
  await expect(page.getByRole('button', { name: 'Cancelled (1)' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'All (2)' })).toBeVisible();

  // A long list scrolls inside the table, not the page.
  const many = Array.from({ length: 50 }, (_, i) => fixture.put(`many-${i}.bin`, 1024));
  await add(page, many.join('\n'), 50);
  await expect(page.locator('tr[data-job]')).toHaveCount(50);
  await expectFitsWindow(page);
  const table = page.locator('.downloads-view .table-scroll');
  expect(await table.evaluate((el) => el.scrollHeight > el.clientHeight)).toBe(true);

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

/**
 * The open row menu: its items' names, and whether it lies inside the
 * window.
 * @param {import('@playwright/test').Page} page
 */
async function openMenu(page) {
  const menu = page.getByRole('menu', { name: 'Download actions' });
  await expect(menu).toBeVisible();
  const inside = await menu.evaluate((el) => {
    const box = el.getBoundingClientRect();
    const { clientWidth, clientHeight } = document.documentElement;
    return box.left >= 0 && box.top >= 0 && box.right <= clientWidth && box.bottom <= clientHeight;
  });
  expect(inside).toBe(true);
  // An open menu never makes the page scroll.
  await expectFitsWindow(page);
  return menu;
}

test('everyday controls: settings, the row menu, rename, remove, and copy link', async ({ browser }) => {
  const context = await browser.newContext();
  await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: daemon.origin });
  const page = await context.newPage();
  await page.goto(daemon.origin);
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');

  // Add links says where links will be saved: by default, the default
  // folder, on the server.
  const addLinks = page.getByRole('button', { name: 'Add links' });
  const addPage = page.locator('.add-page');
  const destination = addPage.locator('#collect-destination option:checked');
  await addLinks.click();
  await expect(destination).toHaveText(/** @type {string} */ (daemon.root));
  await expect(addPage.getByText('Server path')).toBeVisible();
  await addPage.getByRole('button', { name: 'Cancel' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/`);

  // A download that is running while Settings is shown.
  const running = 'while-in-settings.bin';
  await add(page, fixture.put(running, SIZE, STALL));

  // Settings is a page of its own, opened from the gear: a menu of
  // sections with the selected one beside it.
  const settingsButton = page.getByRole('button', { name: 'Settings', exact: true });
  await expect(settingsButton.locator('svg.icon-settings')).toHaveCount(1);
  await settingsButton.click();
  await expect(page).toHaveURL(`${daemon.origin}/settings`);
  await expect(page.getByRole('dialog')).toHaveCount(0);
  await expect(page.getByRole('heading', { name: 'Settings', level: 1 })).toBeFocused();
  await expect(page.locator('.downloads-view table.jobs')).toBeHidden();
  const sections = page.getByRole('navigation', { name: 'Settings sections' });
  const downloadsLink = sections.getByRole('link', { name: 'Downloads', exact: true });
  const folderLink = sections.getByRole('link', { name: 'Download folders' });
  const displayLink = sections.getByRole('link', { name: 'Display' });
  const accountsLink = sections.getByRole('link', { name: 'Accounts & services' });
  await expect(downloadsLink).toHaveAttribute('aria-current', 'page');
  expect(await sections.getByRole('link').allInnerTexts()).toEqual([
    'Downloads',
    'Download folders',
    'Accounts & services',
    'Plugins',
    'Display',
    'About',
  ]);
  const pane = page.locator('.settings-content');
  await expect(pane.getByLabel('Maximum simultaneous downloads')).toHaveValue('4');
  await folderLink.click();
  await expect(page).toHaveURL(`${daemon.origin}/settings/folder`);
  await expect(folderLink).toHaveAttribute('aria-current', 'page');
  await expect(displayLink).not.toHaveAttribute('aria-current');
  // The permitted server folders.
  await expect(pane.getByRole('heading', { name: 'Download folders' })).toBeVisible();
  /** @param {string} path */
  const folderRow = (path) => pane.locator('.folder-row', { has: page.getByText(path, { exact: true }) });
  await expect(folderRow(/** @type {string} */ (daemon.root)).locator('.tag')).toHaveText('Default');
  await expect(folderRow(/** @type {string} */ (daemon.root)).getByRole('button')).toHaveCount(0);
  await expect(pane.getByText('Server paths', { exact: true })).toBeVisible();
  // Nothing asks for a command line.
  await expect(page.locator('.settings-page code')).toHaveCount(0);
  await expectFitsWindow(page);
  // Theme and row density apply at once.
  await displayLink.click();
  await expect(page).toHaveURL(`${daemon.origin}/settings/display`);
  await expect(displayLink).toHaveAttribute('aria-current', 'page');
  await expect(folderLink).not.toHaveAttribute('aria-current');
  await expect(pane.getByRole('heading', { name: 'Download folders' })).toBeHidden();
  await expect(pane.getByRole('radio', { name: 'System', exact: true })).toBeChecked();
  await page.emulateMedia({ colorScheme: 'dark' });
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await pane.getByRole('radio', { name: 'Light', exact: true }).check();
  await expect(page.locator('html')).toHaveCSS('color-scheme', 'light');
  await page.reload();
  await expect(pane.getByRole('radio', { name: 'Light', exact: true })).toBeChecked();
  await expect(page.locator('html')).toHaveCSS('color-scheme', 'light');
  await pane.getByRole('radio', { name: 'Dark', exact: true }).check();
  await page.emulateMedia({ colorScheme: 'light' });
  await expect(page.locator('html')).toHaveCSS('color-scheme', 'dark');
  await pane.getByRole('radio', { name: 'System', exact: true }).check();
  await expect(page.locator('html')).toHaveCSS('color-scheme', 'light');
  await page.emulateMedia({ colorScheme: 'dark' });
  await expect(page.locator('html')).toHaveCSS('color-scheme', 'dark');
  await page.reload();
  await expect(pane.getByRole('radio', { name: 'System', exact: true })).toBeChecked();
  await page.emulateMedia({ colorScheme: 'light' });
  await pane.getByRole('radio', { name: 'Comfortable' }).check();
  expect(await page.evaluate(() => document.documentElement.dataset.density)).toBe('comfortable');
  await pane.getByRole('radio', { name: 'Compact' }).check();
  // About shows the version, and checks for a newer release only when
  // asked, linking to its page.
  await sections.getByRole('link', { name: 'About' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/settings/about`);
  await expect(pane.locator('[data-key="app-version"]')).toHaveText(/^\d+\.\d+\.\d+/);
  await pane.getByRole('button', { name: 'Check for updates' }).click();
  await expect(pane.locator('[data-key="update-message"]')).toHaveText('Version 99.0.0 is available. View release');
  await expect(pane.getByRole('link', { name: 'View release' })).toHaveAttribute(
    'href',
    `${fixture.base}/pages/releases/tag/v99.0.0`,
  );
  await expectFitsWindow(page);
  await page.goBack();
  await expect(displayLink).toHaveAttribute('aria-current', 'page');
  // From the keyboard: arrows move along the sections, Enter opens one,
  // and focus is drawn.
  await folderLink.focus();
  await page.keyboard.press('ArrowDown');
  await expect(accountsLink).toBeFocused();
  expect(await accountsLink.evaluate((el) => getComputedStyle(el).outlineStyle)).toBe('solid');
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(`${daemon.origin}/settings/accounts`);
  await expect(accountsLink).toHaveAttribute('aria-current', 'page');
  await expect(accountsLink).toBeFocused();
  // Only accounts added are listed, in a table: none yet.
  const accounts = pane.locator('section', { has: page.getByRole('heading', { name: /Accounts & services/ }) });
  const table = accounts.getByRole('table', { name: 'Accounts' });
  await expect(table.getByRole('columnheader')).toHaveText(['Service', 'Account', 'Status', 'Expires', 'Actions']);
  await expect(table.locator('tbody tr')).toHaveText(['No accounts added yet.']);
  // With nothing to report, no empty foot is left under the table.
  await expect(accounts.locator('.group-foot')).toBeHidden();
  await expect(accounts.getByText(/Links you add directly download without an account/)).toBeVisible();
  await expect(accounts.locator('.service-card')).toHaveCount(0);
  await expect(accounts.locator('input, select, textarea')).toHaveCount(0);
  await expect(accounts.getByRole('button')).toHaveText(['Add account…']);
  await expectFitsWindow(page);
  await expectServiceCatalogue(page, accounts);
  // The browser's Back goes to the previous section.
  await page.goBack();
  await expect(displayLink).toHaveAttribute('aria-current', 'page');
  // Any other folder can be made the default at once, with feedback.
  await folderLink.click();
  await pane.getByRole('button', { name: `Make ${daemon.archive} the default` }).click();
  await expect(pane.getByText(`New downloads now go to ${daemon.archive}.`)).toBeVisible();
  await expect(folderRow(/** @type {string} */ (daemon.archive)).locator('.tag')).toHaveText('Default');
  await expect(folderRow(/** @type {string} */ (daemon.root)).getByRole('button')).toHaveText(['Make default', 'Remove']);
  await expect(pane.getByRole('button', { name: `Make ${daemon.root} the default` })).toBeFocused();
  // A server folder is added by browsing to it: the dialog starts at the
  // selected folder, goes up, to a typed path, and into a folder.
  const shares = join(/** @type {string} */ (daemon.dir), 'shares');
  await mkdir(join(shares, 'films'), { recursive: true });
  await mkdir(join(shares, '.hidden'));
  await pane.getByRole('button', { name: 'Add folder…' }).click();
  const folders = page.getByRole('dialog', { name: 'Add a server folder' });
  const at = folders.getByLabel('Folder', { exact: true });
  await expect(at).toHaveValue(/** @type {string} */ (daemon.archive));
  await folders.getByRole('button', { name: 'Up' }).click();
  await expect(at).toHaveValue(/** @type {string} */ (daemon.dir));
  await expect(folders.getByRole('button', { name: 'shares' })).toBeVisible();
  // A folder that could not be listed cannot be added in place of the
  // one listed before it.
  const addThis = folders.getByRole('button', { name: 'Add this folder' });
  await expect(addThis).toBeEnabled();
  await at.fill(join(shares, 'missing'));
  await at.press('Enter');
  await expect(folders.getByText('The folder does not exist.')).toBeVisible();
  await expect(addThis).toBeDisabled();
  // Editing the path drops a listing still on its way, and adding waits
  // until the path typed is listed.
  const left = /** @type {string} */ (daemon.dir);
  const slow = `**/v1/folders?path=${encodeURIComponent(left)}`;
  let held = false;
  /** @type {() => void} */
  let answer = () => {};
  await page.route(slow, async (route) => {
    held = true;
    await new Promise((resolve) => {
      answer = () => resolve(undefined);
    });
    await route.continue();
  });
  await at.fill(left);
  await at.press('Enter');
  await expect.poll(() => held).toBe(true);
  await expect(addThis).toBeDisabled();
  await at.fill(shares);
  const late = page.waitForResponse((response) => response.url().includes(encodeURIComponent(left)));
  answer();
  await late;
  await page.unroute(slow);
  // The late answer would have been drawn by now.
  await page.waitForTimeout(200);
  await expect(at).toHaveValue(shares);
  await expect(addThis).toBeDisabled();
  await expect(folders.getByText('Press Go to list this folder before adding it.')).toBeVisible();
  await at.press('Enter');
  await expect(folders.getByRole('list', { name: 'Folders inside' }).getByRole('button')).toHaveText(['films']);
  await expect(addThis).toBeEnabled();
  await folders.getByRole('button', { name: 'films' }).click();
  await expect(at).toHaveValue(join(shares, 'films'));
  await expect(folders.getByText('There are no folders inside.')).toBeVisible();
  // A path typed over the folder shown is not added in its place.
  await at.fill(left);
  await expect(addThis).toBeDisabled();
  await at.fill(join(shares, 'films'));
  await expect(addThis).toBeDisabled();
  await folders.getByRole('button', { name: 'Go' }).click();
  await expect(addThis).toBeEnabled();
  await folders.getByRole('button', { name: 'Add this folder' }).click();
  await expect(folders).toBeHidden();
  await expect(pane.getByText('Folder added. Use Make default to save new downloads there.')).toBeVisible();
  await expect(folderRow(join(shares, 'films')).getByRole('button')).toHaveText(['Make default', 'Remove']);
  await expect(pane.getByRole('button', { name: 'Add folder…' })).toBeFocused();
  // Closing without adding changes nothing.
  await pane.getByRole('button', { name: 'Add folder…' }).click();
  await page.keyboard.press('Escape');
  await expect(pane.getByText('No folder was chosen; nothing changed.')).toBeVisible();
  // Any folder but the default can be removed; its files stay.
  const films = join(shares, 'films');
  // Links collected to be saved there do not keep it; they show it as
  // removed until given another folder. Folder changes are made at once,
  // so leaving does not ask.
  await expect(folderLink.locator('.unsaved')).toBeHidden();
  await page.locator('[data-key="settings-back"]').click();
  await expect(page).toHaveURL(`${daemon.origin}/`);
  await page.getByRole('button', { name: 'Add links' }).click();
  await page.getByLabel('Save to').selectOption({ label: films });
  await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(fixture.put('orphan.bin', MIB));
  await page.getByRole('button', { name: 'Collect links' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/links`);
  const orphan = page.locator('tr[data-link]', { has: page.locator('td.name', { hasText: /^orphan\.bin$/ }) });
  await expect(orphan.locator('td.folder')).toHaveText(films);
  await settingsButton.click();
  await folderLink.click();
  await writeFile(join(films, 'kept.bin'), 'kept');
  await expect(pane.getByRole('button', { name: /^Remove / })).toHaveCount(2);
  await pane.getByRole('button', { name: `Remove ${films}` }).click();
  await expect(pane.getByText(`Removed ${films}. Its files stay there;`, { exact: false })).toBeVisible();
  await expect(folderRow(films)).toHaveCount(0);
  await expect(folderRow(/** @type {string} */ (daemon.archive)).locator('.tag')).toHaveText('Default');
  await expect(pane.getByRole('button', { name: `Make ${daemon.root} the default` })).toBeFocused();
  expect(await readFile(join(films, 'kept.bin'), 'utf8')).toBe('kept');
  await page.locator('[data-key="settings-back"]').click();
  await page.getByRole('navigation', { name: 'Views' }).getByRole('link', { name: /^Links/ }).click();
  await expect(orphan.locator('td.folder')).toHaveText(`${films} (removed; choose another)`);
  await orphan.getByRole('checkbox').check();
  await expect(page.locator('.links-page .selection-summary')).toContainText(
    '1 link saving to a removed folder will not start; choose another with Save to.',
  );
  await page.getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(orphan).toHaveCount(0);
  expect(fixture.requested).not.toContain('orphan.bin');
  await page.getByRole('navigation', { name: 'Views' }).getByRole('link', { name: 'Downloads' }).click();
  await settingsButton.click();
  await folderLink.click();
  // The download kept running meanwhile, and finishes while Settings is shown.
  expect(await daemon.published(running).then(() => 'published', () => 'not yet')).toBe('not yet');
  fixture.release(running);
  await expect
    .poll(async () => (await daemon.published(running).catch(() => null))?.equals(content(0, SIZE)))
    .toBe(true);
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
  // Narrow windows put the sections above the content, without empty
  // scrolling.
  await page.setViewportSize({ width: 480, height: 720 });
  const stacked = await page.evaluate(() => {
    const nav = /** @type {Element} */ (document.querySelector('.settings-nav')).getBoundingClientRect();
    const panel = /** @type {Element} */ (document.querySelector('.settings-content')).getBoundingClientRect();
    return nav.bottom <= panel.top;
  });
  expect(stacked).toBe(true);
  await expectFitsWindow(page);
  await page.setViewportSize({ width: 1280, height: 720 });
  // Back to Downloads, where focus returns to the gear; Escape does the
  // same from anywhere on the page.
  await page.getByRole('button', { name: 'Back to Downloads' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/`);
  await expect(settingsButton).toBeFocused();
  await settingsButton.click();
  await expect(downloadsLink).toHaveAttribute('aria-current', 'page');
  await page.keyboard.press('Escape');
  await expect(page).toHaveURL(`${daemon.origin}/`);
  await expect(settingsButton).toBeFocused();
  // Links are now saved to the new default.
  await addLinks.click();
  await expect(destination).toHaveText(/** @type {string} */ (daemon.archive));
  await page.keyboard.press('Escape');
  await expect(addPage).toBeHidden();
  // A section opened directly, as after a reload, shows the same page.
  await page.goto(`${daemon.origin}/settings/display`);
  await expect(displayLink).toHaveAttribute('aria-current', 'page');
  await page.getByRole('button', { name: 'Back to Downloads' }).click();
  await expect(page.locator('.downloads-view table.jobs')).toBeVisible();

  // Right-click opens the menu of the row clicked. A browser cannot open
  // the server's files, so it does not offer to.
  await row(page, 'job-1').locator('td.host').click({ button: 'right' });
  let menu = await openMenu(page);
  await expect(row(page, 'job-1').getByRole('button', { name: /^Actions for / })).toHaveAttribute(
    'aria-expanded',
    'true',
  );
  await expect(menu.getByRole('menuitem', { name: /Open file|Open containing folder/ })).toHaveCount(0);
  // Pointer movement transfers the highlight from the initially focused
  // item, and keyboard navigation continues from the hovered item.
  const firstItem = menu.getByRole('menuitem', { name: 'Rename…' });
  const copyItem = menu.getByRole('menuitem', { name: 'Copy source link' });
  await copyItem.hover();
  await expect(copyItem).toBeFocused();
  await expect(firstItem).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)');
  await page.keyboard.press('ArrowUp');
  await expect(firstItem).toBeFocused();
  await copyItem.hover({ position: { x: 10, y: 10 } });
  await expect(copyItem).toBeFocused();
  await menu.getByRole('menuitem', { name: 'Copy source link' }).click();
  await expect(menu).toBeHidden();
  await expect(page.getByText(`Copied the link of ${TRICKY}.`)).toBeVisible();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(
    `${fixture.base}/files/${SOURCE_NAME}${SOURCE_SUFFIX}`,
  );

  // Items that do not apply say why.
  await row(page, 'job-2').click({ button: 'right' });
  menu = await openMenu(page);
  await expect(menu.getByRole('menuitem', { name: /^Rename/ })).toHaveAttribute('aria-disabled', 'true');
  await expect(menu.getByRole('menuitem', { name: /^Rename/ })).toContainText('Once it has finished');
  await expect(menu.getByRole('menuitem', { name: /^Remove…/ })).not.toHaveAttribute('aria-disabled');
  await expect(menu.getByRole('menuitem', { name: /^Remove…/ })).toHaveText('Remove…');
  // A press outside closes it.
  await page.locator('.filters').click({ position: { x: 5, y: 5 } });
  await expect(menu).toBeHidden();

  // Near the window's corner it still opens inside the window.
  const corner = await page.evaluate(() => {
    const { clientWidth, clientHeight } = document.documentElement;
    const table = /** @type {Element} */ (document.querySelector('.table-scroll')).getBoundingClientRect();
    return { x: clientWidth - 12, y: Math.min(clientHeight, table.bottom) - 12 };
  });
  await page.mouse.click(corner.x, corner.y, { button: 'right' });
  await openMenu(page);
  await page.keyboard.press('Escape');

  // A download goes to the new default; the details say where.
  // Jobs are listed oldest first, so it is on the next page.
  // Its link carries a token, as signed CDN links do.
  const signed = `${fixture.put('menu.bin', MIB)}?token=secret`;
  await add(page, signed);
  await page.getByRole('button', { name: 'Next page' }).click();
  await expect(page.locator('.downloads-view .pager .page')).toHaveText(/^Page 2 of 2$/);
  await expect(page.getByRole('button', { name: 'Next page' })).toBeDisabled();
  const menuRow = page.locator('tr[data-job]', { has: page.locator('td.name', { hasText: /^menu\.bin$/ }) });
  await expect(menuRow).toHaveCount(1);
  const job = await menuRow.getAttribute('data-job');
  if (!job) throw new Error('no job');
  await expectState(page, job, 'Done');
  expect((await daemon.published('menu.bin', daemon.archive)).equals(content(0, MIB))).toBe(true);
  await select(page, job);
  await expect(page.locator('dd[data-fact="saved-in"] .path')).toHaveText(/** @type {string} */ (daemon.archive));
  await expect(page.locator('dd[data-fact="file"] .path')).toHaveAttribute('title', `${daemon.archive}/menu.bin`);
  // The details show the link without its query, and copy all of it.
  const source = page.locator('dd[data-fact="source"]');
  await expect(source.locator('.path')).toHaveText(`${fixture.base}/files/menu.bin?…`);
  await expect(source).not.toContainText('secret');
  await source.getByRole('button', { name: 'Copy link' }).click();
  await expect(page.getByText('Copied the link of menu.bin.')).toBeVisible();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(signed);
  await page.getByRole('button', { name: 'Close details' }).click();

  // From the keyboard: the row's menu button, arrow keys, Escape back to it.
  const more = row(page, job).getByRole('button', { name: 'Actions for menu.bin' });
  await more.focus();
  await page.keyboard.press('Enter');
  menu = await openMenu(page);
  await expect(menu.getByRole('menuitem', { name: 'Rename…' })).toBeFocused();
  await page.keyboard.press('ArrowDown');
  await expect(menu.getByRole('menuitem', { name: 'Copy source link' })).toBeFocused();
  await page.keyboard.press('End');
  await expect(menu.getByRole('menuitem', { name: /^Remove…/ })).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(menu).toBeHidden();
  await expect(more).toBeFocused();
  // Shift+F10 opens it from the row's link too.
  await row(page, job).locator('td.name a').focus();
  await page.keyboard.press('Shift+F10');
  menu = await openMenu(page);

  // Rename: invalid names and collisions are refused, nothing is replaced.
  await menu.getByRole('menuitem', { name: 'Rename…' }).click();
  const rename = page.getByRole('dialog', { name: 'Rename menu.bin' });
  const name = rename.getByLabel('New file name');
  await expect(name).toBeFocused();
  await name.fill('a/b.bin');
  await rename.getByRole('button', { name: 'Rename' }).click();
  await expect(rename.getByText('A name cannot contain / or \\.')).toBeVisible();
  await writeFile(join(/** @type {string} */ (daemon.archive), 'taken.bin'), 'theirs');
  await name.fill('taken.bin');
  await rename.getByRole('button', { name: 'Rename' }).click();
  await expect(rename.getByText('A file with that name is already in the folder.')).toBeVisible();
  expect((await daemon.published('taken.bin', daemon.archive)).toString()).toBe('theirs');
  await name.fill('renamed.bin');
  await rename.getByRole('button', { name: 'Rename' }).click();
  await expect(rename).toBeHidden();
  await expect(page.getByText('Renamed menu.bin to renamed.bin.')).toBeVisible();
  await expect(row(page, job).locator('td.name')).toHaveText('renamed.bin');
  expect((await daemon.published('renamed.bin', daemon.archive)).equals(content(0, MIB))).toBe(true);
  await expect(daemon.published('menu.bin', daemon.archive)).rejects.toThrow();

  // Remove from list: the row and its count go, the file stays.
  // The first test left 52 downloads; this one added two more.
  await expect(page.getByRole('button', { name: 'All (54)' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Done (53)' })).toBeVisible();
  await row(page, job).getByRole('button', { name: 'Actions for renamed.bin' }).click();
  menu = await openMenu(page);
  await menu.getByRole('menuitem', { name: /^Remove…/ }).click();
  let removal = page.getByRole('dialog', { name: 'Remove download' });
  await expect(removal.getByRole('radio', { name: 'Remove from list only (keep files)' })).toBeChecked();
  await removal.getByRole('button', { name: 'Cancel', exact: true }).click();
  await expect(row(page, job)).toHaveCount(1);
  expect((await daemon.published('renamed.bin', daemon.archive)).equals(content(0, MIB))).toBe(true);
  await row(page, job).getByRole('button', { name: 'Actions for renamed.bin' }).click();
  menu = await openMenu(page);
  await menu.getByRole('menuitem', { name: /^Remove…/ }).click();
  removal = page.getByRole('dialog', { name: 'Remove download' });
  await removal.getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(row(page, job)).toHaveCount(0);
  await expect(page.getByText('Removed renamed.bin from the list. Remaining files stay in their folder.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'All (53)' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Done (52)' })).toBeVisible();
  expect((await daemon.published('renamed.bin', daemon.archive)).equals(content(0, MIB))).toBe(true);

  // The footer counts from where the page really starts: removing a
  // download on page 1 elsewhere moves this page's range.
  const pager = page.locator('.downloads-view .pager');
  await expect(pager.locator('.range')).toHaveText('51\u201353 of 53');
  const other = await context.newPage();
  await other.goto(`${daemon.origin}/`);
  const firstRow = other.locator('tr[data-job]').first();
  await firstRow.getByRole('button', { name: /^Actions for / }).click();
  await (await openMenu(other)).getByRole('menuitem', { name: /^Remove…/ }).click();
  await other.getByRole('dialog', { name: 'Remove download' }).getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(other.getByRole('button', { name: 'All (52)' })).toBeVisible();
  await other.close();
  await expect(pager.locator('.range')).toHaveText('50\u201352 of 52');
  await expect(pager.locator('.page')).toHaveText('Page 2 of 2');

  // Emptying the page goes back to the one before it.
  await page.getByRole('checkbox', { name: 'Select all downloads shown' }).check();
  await page.locator('tr[data-job]').first().getByRole('button', { name: /^Actions for / }).click();
  await (await openMenu(page)).getByRole('menuitem', { name: 'Remove…' }).click();
  await page.getByRole('dialog', { name: 'Remove 3 downloads' }).getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(pager.locator('.page')).toHaveText('Page 1 of 1');
  await expect(pager.locator('.range')).toHaveText('1\u201349 of 49');
  await expect(page.locator('tr[data-job]')).toHaveCount(49);

  // The chosen default outlives a reload.
  await page.reload();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
  await addLinks.click();
  await expect(destination).toHaveText(/** @type {string} */ (daemon.archive));
  await context.close();
});

test('collected links wait on the Links page until chosen ones are started', async ({ browser }) => {
  const context = await browser.newContext();
  await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: daemon.origin });
  const page = await context.newPage();
  await page.goto(daemon.origin);
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
  const all = page.getByRole('button', { name: /^All \(/ });
  await expect(all).toHaveAttribute('aria-label', /\(\d+\)$/);
  const jobsBefore = Number(/\((\d+)\)$/.exec((await all.getAttribute('aria-label')) ?? '')?.[1]);

  // The views are tabs in the toolbar, reached from the keyboard.
  const views = page.getByRole('navigation', { name: 'Views' });
  await expect(views.getByRole('link', { name: 'Downloads' })).toHaveAttribute('aria-current', 'page');
  await views.getByRole('link', { name: 'Links' }).focus();
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(`${daemon.origin}/links`);
  await expect(views.getByRole('link', { name: 'Links' })).toHaveAttribute('aria-current', 'page');
  // Previously accepted links moved to Downloads and remain absent after navigation.
  await expect(page.locator('tr.started')).toHaveCount(0);
  await expect(page.getByText('No links collected.')).toBeVisible();
  // With nothing to select, no empty summary line sits above the table.
  await expect(page.locator('.links-page .selection-summary')).toBeHidden();

  // Add links is a page of its own. Leaving it collects nothing, and
  // keeps what was typed.
  const first = fixture.put('review-a.bin', MIB);
  const signed = `${fixture.put('review-b.bin', MIB)}?token=secret`;
  const text = `${first}\nftp://example.com/other-scheme.bin\n${signed}`;
  const addPage = page.locator('.add-page');
  await page.getByRole('button', { name: 'Add links' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/add`);
  await expect(page.getByRole('heading', { name: 'Add links', level: 1 })).toBeVisible();
  await expect(page.getByLabel('Links, text with links in it, or a page’s HTML')).toBeFocused();
  await expect(addPage.getByText('Nothing downloads until you start it.', { exact: false })).toBeVisible();
  await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(text);
  await page.keyboard.press('Escape');
  await expect(page).toHaveURL(`${daemon.origin}/links`);
  await page.getByRole('button', { name: 'Add links' }).click();
  await expect(page.getByLabel('Links, text with links in it, or a page’s HTML')).toHaveValue(text);
  await addPage.getByRole('button', { name: 'Back to Links' }).click();
  await expect(page.getByText('No links collected.')).toBeVisible();

  // Collecting keeps the valid links and marks the invalid entry. They are
  // checked, which asks each file's server for its size, never its
  // content; nothing is downloaded.
  await collect(page, text, 'Collected 2 links for review. 1 entry is not a valid link; it is marked below.');
  await expect(views.getByRole('link', { name: 'Links (3)' })).toBeVisible();
  const linkRow = (/** @type {string} */ name) =>
    page.locator('tr[data-link]', { has: page.locator('td.name', { hasText: new RegExp(`^${name}$`) }) });
  await expect(linkRow('review-a.bin').locator('td.link-status')).toHaveText('Online');
  await expect(linkRow('review-a.bin').locator('td.host')).toHaveText('127.0.0.1');
  await expect(linkRow('review-a.bin').locator('td.num')).toHaveText('1.0 MiB');
  // Hovering the name shows just the name.
  await expect(linkRow('review-a.bin').locator('td.name')).toHaveAttribute('title', 'review-a.bin');
  await expect(linkRow('review-b.bin').locator('td.link-status')).toHaveText('Online');
  expect(fixture.checked.filter((name) => name === 'review-a.bin' || name === 'review-b.bin')).toHaveLength(2);
  await expect(linkRow('ftp://example\\.com/other-scheme\\.bin').locator('td.link-status')).toContainText('Invalid link');
  await expect(linkRow('review-b.bin')).toHaveCount(1);
  await expect(page.locator('.links-page')).not.toContainText('secret');
  expect(fixture.requested.filter((name) => name === 'review-a.bin' || name === 'review-b.bin')).toEqual([]);
  // The new links are selected, with focus on the first.
  await expect(page.locator('.links-page .selection-summary strong')).toHaveText('3 links selected');
  await expect(page.locator('tr[data-link] input[type="checkbox"]').first()).toBeFocused();

  // They outlive a reload; the engine holds them.
  await page.reload();
  await expect(linkRow('review-a.bin')).toHaveCount(1);
  await expect(linkRow('ftp://example\\.com/other-scheme\\.bin')).toHaveCount(1);

  // A column header sorts by it, again reverses, and the order is kept.
  const names = page.locator('.links-page tr[data-link] td.name');
  const nameHeader = page.locator('.links-page th', { has: page.getByRole('button', { name: 'Name', exact: true }) });
  await expect(page.locator('.links-page th[aria-sort]')).toHaveText('Added at');
  // The whole header cell is the button.
  const [cell, button] = await Promise.all([nameHeader.boundingBox(), nameHeader.getByRole('button').boundingBox()]);
  // (The cell's height also holds its 1px bottom border.)
  expect(Math.abs(Number(button?.width) - Number(cell?.width))).toBeLessThanOrEqual(1);
  expect(Math.abs(Number(button?.height) - Number(cell?.height))).toBeLessThanOrEqual(1);
  await nameHeader.getByRole('button').click();
  await expect(nameHeader).toHaveAttribute('aria-sort', 'ascending');
  await expect(names).toHaveText(['ftp://example.com/other-scheme.bin', 'review-a.bin', 'review-b.bin']);
  await expect(nameHeader.getByRole('button')).toBeFocused();
  await nameHeader.getByRole('button').click();
  await expect(nameHeader).toHaveAttribute('aria-sort', 'descending');
  await expect(names).toHaveText(['review-b.bin', 'review-a.bin', 'ftp://example.com/other-scheme.bin']);
  await page.reload();
  await expect(names).toHaveText(['review-b.bin', 'review-a.bin', 'ftp://example.com/other-scheme.bin']);
  await page.getByRole('button', { name: 'Added at', exact: true }).click();
  await expect(page.locator('.links-page th[aria-sort="ascending"]')).toHaveText('Added at');
  await expect(page.locator('.links-page tr[data-link] td.added').first()).toHaveText(/^Today, /);

  // Filtering never acts on what it hides.
  const start = page.getByRole('button', { name: 'Start selected' });
  // Ticking moves no row: the selection line keeps its room.
  const before = await linkRow('review-a.bin').boundingBox();
  await linkRow('review-a.bin').getByRole('checkbox').check();
  await expect(page.locator('.links-page .selection-summary')).toContainText('1 link selected');
  expect((await linkRow('review-a.bin').boundingBox())?.y).toBe(before?.y);
  await expect(page.locator('.links-page .selection-summary .path')).toHaveText(/** @type {string} */ (daemon.archive));
  await page.getByLabel('Filter links').fill('review-b');
  await expect(linkRow('review-a.bin')).toHaveCount(0);
  await expect(page.getByText('1 selected link hidden by the filter is left out.')).toBeVisible();
  await expect(start).toBeDisabled();
  await page.getByRole('checkbox', { name: 'Select all links shown' }).check();
  await expect(page.locator('.links-page .selection-summary strong')).toHaveText('1 link selected');
  await page.getByRole('checkbox', { name: 'Select all links shown' }).uncheck();
  await page.getByLabel('Filter links').fill('');

  // Ctrl+A (Cmd+A on a Mac) ticks every link shown instead of selecting
  // the page's text, once links are ticked or the table has focus. In the
  // filter field it stays the field's own.
  const summary = page.locator('.links-page .selection-summary strong');
  await page.keyboard.press('ControlOrMeta+A');
  await expect(summary).toHaveText('1 link selected');
  await page.getByLabel('Filter links').blur();
  await page.keyboard.press('ControlOrMeta+A');
  await expect(summary).toHaveText('3 links selected');
  expect(await page.evaluate(() => String(window.getSelection()))).toBe('');
  const selectAll = page.getByRole('checkbox', { name: 'Select all links shown' });
  await selectAll.uncheck();
  await expect(page.locator('.links-page .selection-summary')).not.toContainText('selected');
  await page.keyboard.press('ControlOrMeta+A');
  await expect(summary).toHaveText('3 links selected');
  await selectAll.uncheck();
  await linkRow('review-a.bin').getByRole('checkbox').check();
  await expect(summary).toHaveText('1 link selected');

  // A new folder for the selected link only: the default stays.
  await page.getByRole('button', { name: 'Save to…' }).click();
  const move = page.getByRole('dialog', { name: 'Change where links are saved' });
  await move.getByRole('radio', { name: new RegExp(`^${daemon.root}`) }).check();
  await move.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText(`1 link will be saved to ${daemon.root}.`)).toBeVisible();
  await expect(linkRow('review-a.bin').locator('td.folder')).toHaveText(/** @type {string} */ (daemon.root));
  await expect(linkRow('review-b.bin').locator('td.folder')).toHaveText(/** @type {string} */ (daemon.archive));
  await page.getByRole('button', { name: 'Add links' }).click();
  await expect(addPage.locator('#collect-destination option:checked')).toHaveText(
    /** @type {string} */ (daemon.archive),
  );
  await addPage.getByRole('button', { name: 'Cancel' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/links`);
  await expect(page.getByRole('button', { name: 'Add links' })).toBeFocused();

  // The row menu, from the keyboard: an invalid link cannot start, and
  // can be removed.
  await linkRow('ftp://example\\.com/other-scheme\\.bin').getByRole('checkbox').focus();
  await page.keyboard.press('Shift+F10');
  let menu = page.getByRole('menu', { name: 'Link actions' });
  await expect(menu).toBeVisible();
  await expect(menu.getByRole('menuitem', { name: /^Start download/ })).toHaveAttribute('aria-disabled', 'true');
  await expect(menu.getByRole('menuitem', { name: /^Start download/ })).toContainText('Not a valid link');
  await page.keyboard.press('End');
  await page.keyboard.press('Enter');
  await expect(linkRow('ftp://example\\.com/other-scheme\\.bin')).toHaveCount(0);
  await expect(page.getByText('Removed 1 link from the list.')).toBeVisible();

  // Copy link gives the whole link, token and all.
  await linkRow('review-b.bin').locator('td.host').click({ button: 'right' });
  menu = page.getByRole('menu', { name: 'Link actions' });
  await menu.getByRole('menuitem', { name: 'Copy link' }).click();
  await expect(page.getByText('Copied the link of review-b.bin.')).toBeVisible();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(signed);

  // Only the selected link starts, in its own folder.
  await expect(start).toBeEnabled();
  await start.click();
  await expect(page.getByText('Started 1 download.')).toBeVisible();
  await expect(page).toHaveURL(`${daemon.origin}/`);
  await views.getByRole('link', { name: 'Links (1)' }).click();
  await expect(linkRow('review-a.bin')).toHaveCount(0);
  await expect(linkRow('review-b.bin').locator('td.link-status')).toHaveText('Online');
  await expect(views.getByRole('link', { name: 'Links (1)' })).toBeVisible();
  await expect
    .poll(async () => (await daemon.published('review-a.bin', daemon.root).catch(() => null))?.equals(content(0, MIB)))
    .toBe(true);

  // Narrow windows and the dark theme: no sideways page scrolling, and
  // the toolbar still offers every view.
  await page.setViewportSize({ width: 420, height: 760 });
  await page.emulateMedia({ colorScheme: 'dark' });
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(
    true,
  );
  expect(await page.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe('rgb(15, 19, 24)');
  await expect(views.getByRole('link', { name: 'Downloads' })).toBeVisible();
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.emulateMedia({ colorScheme: 'light' });

  // Remove the remaining unstarted link; Downloads keeps the accepted one.
  await linkRow('review-b.bin').getByRole('checkbox').check();
  await page.getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(page.getByText('No links collected.')).toBeVisible();
  await expect(views.getByRole('link', { name: 'Links', exact: true })).toBeVisible();
  await views.getByRole('link', { name: 'Downloads' }).click();
  await expect(page.getByRole('button', { name: `All (${jobsBefore + 1})` })).toBeVisible();
  await expect(daemon.published('review-b.bin', daemon.archive)).rejects.toThrow();

  // A page's address leads to the files on it, which start like any link.
  fixture.put('release-a.bin', 2 * MIB, null, 'Release A.bin');
  const release = fixture.page(
    'release',
    '<html><body><a href="../files/release-a.bin">A</a> <a href="../files/release-gone.zip">B</a> ' +
      '<a href="other">Another page</a></body></html>',
  );
  await collect(page, release, 'Collected 1 link for review.');
  const foundRow = (/** @type {string} */ name) =>
    page.locator('tr[data-link]', { has: page.locator('td.name .name-text', { hasText: new RegExp(`^${name}$`) }) });
  // The page is read once; its files are listed with where they were found.
  const pageRow = page.locator('tr[data-link]', { has: page.locator('td[data-check="page"]') });
  await expect(pageRow.locator('td.link-status')).toHaveText(
    'Web page2 files found on it, listed below it. 1 link to other pages left alone.',
  );
  await expect(foundRow('Release A\\.bin').locator('td.link-status')).toHaveText('Online');
  await expect(foundRow('Release A\\.bin').locator('td.num')).toHaveText('2.0 MiB');
  await expect(foundRow('Release A\\.bin').locator('.found-on')).toHaveText(`Found on ${release}`);
  await expect(foundRow('release-gone\\.zip').locator('td.link-status')).toContainText('Offline');
  expect(fixture.requested).not.toContain('release-a.bin');

  // Directly, the page itself is refused, and started through its files.
  await page.getByRole('checkbox', { name: 'Select all links shown' }).uncheck();
  await pageRow.getByRole('button', { name: /^Actions for / }).click();
  menu = page.getByRole('menu', { name: 'Link actions' });
  await menu.getByRole('menuitem', { name: /^Start download/ }).click();
  await expect(
    page.getByText('1 link not started: This link is a web page; the files found on it are listed beside it.', { exact: false }),
  ).toBeVisible();
  await foundRow('Release A\\.bin').getByRole('checkbox').check();
  await page.getByRole('button', { name: 'Start selected' }).click();
  await expect(page.getByText('Started 1 download.')).toBeVisible();
  await expect
    .poll(async () => (await daemon.published('Release A.bin', daemon.archive).catch(() => null))?.equals(content(0, 2 * MIB)))
    .toBe(true);
  // The page and what is left of its links can be removed.
  await views.getByRole('link', { name: /^Links/ }).click();
  await page.getByRole('checkbox', { name: 'Select all links shown' }).check();
  await page.getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(page.getByText('No links collected.')).toBeVisible();
  await context.close();
});

test('the download limit and speed limit are server settings every window shares', async ({ browser }) => {
  const context = await browser.newContext();
  const first = await context.newPage();
  await first.goto(daemon.origin);
  await first.getByLabel('Owner name').fill('owner');
  await first.getByLabel('Password').fill(PASSWORD);
  await first.getByRole('button', { name: 'Sign in' }).click();
  await expect(first.locator('.statusbar .status')).toHaveText('Live');
  const second = await context.newPage();
  await second.goto(`${daemon.origin}/settings`);
  await first.goto(`${daemon.origin}/settings/downloads`);

  /** @param {import('@playwright/test').Page} page */
  const parts = (page) => {
    const pane = page.locator('.settings-content');
    return {
      pane,
      limit: pane.getByLabel('Maximum simultaneous downloads'),
      save: pane.getByRole('button', { name: 'Save' }),
      cancel: pane.getByRole('button', { name: 'Cancel' }),
      useDefault: pane.getByRole('button', { name: 'Use defaults' }),
      unsaved: page.locator('[data-key="settings-section-downloads"] .unsaved'),
    };
  };
  const a = parts(first);
  const b = parts(second);
  await expect(a.limit).toHaveValue('4');
  await expect(a.pane.getByText('Allowed: 1–64. Using the default.')).toBeVisible();
  await expect(a.pane.getByText('Applies to the server', { exact: true })).toBeVisible();
  await expect(a.save).toBeDisabled();
  await expect(a.useDefault).toBeDisabled();

  // Only whole numbers in range can be saved.
  await a.limit.fill('70');
  await expect(a.pane.getByText('Enter a whole number from 1 to 64.')).toBeVisible();
  await expect(a.save).toBeDisabled();
  await a.limit.fill('6');
  await expect(a.pane.getByText('Enter a whole number from 1 to 64.')).toHaveCount(0);
  await expect(a.unsaved).toBeVisible();

  // Another window has a draft of its own when this one saves: it hears
  // of the change and keeps its draft.
  await b.limit.fill('3');
  await a.save.click();
  await expect(a.pane.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(a.unsaved).toBeHidden();
  await expect(b.pane.getByText('The settings were changed elsewhere. Your unsaved values are kept.')).toBeVisible();
  await expect(b.pane.getByText('Allowed: 1–64. Saved; the default is 4.')).toBeVisible();
  await expect(b.limit).toHaveValue('3');
  await b.save.click();
  await expect(b.pane.getByText('Saved.', { exact: true })).toBeVisible();
  // The first window follows, having nothing unsaved.
  await expect(a.limit).toHaveValue('3');

  // Cancel restores what applies; Use defaults drops the saved value.
  await a.limit.fill('9');
  await a.cancel.click();
  await expect(a.limit).toHaveValue('3');
  await expect(a.pane.getByText('Changes discarded.')).toBeVisible();
  await a.useDefault.click();
  await expect(a.pane.getByText('Using the default: 4 at once.')).toBeVisible();
  await expect(a.limit).toHaveValue('4');
  await expect(b.limit).toHaveValue('4');
  await expect(a.useDefault).toBeDisabled();
  await expectFitsWindow(first);

  // A failed save keeps the draft.
  await first.route('**/v1/settings', (route) =>
    route.request().method() === 'PUT'
      ? route.fulfill({ status: 503, contentType: 'application/json', body: '{"code":"overloaded","message":"busy"}' })
      : route.continue(),
  );
  await a.limit.fill('5');
  await a.save.click();
  await expect(a.limit).toHaveValue('5');
  await expect(a.save).toBeEnabled();
  await first.unroute('**/v1/settings');
  await a.cancel.click();

  // The total speed limit: shared too, and saved with the count.
  {
    /** @param {import('@playwright/test').Page} page */
    const speedParts = (page) => {
      const pane = page.locator('.settings-content');
      const choice = pane.getByRole('radiogroup', { name: 'Total download speed limit' });
      return {
        pane,
        unlimited: choice.getByRole('radio', { name: 'Unlimited' }),
        limited: choice.getByRole('radio', { name: 'Limit to' }),
        rate: pane.getByRole('spinbutton', { name: 'Total download speed limit' }),
        unit: pane.getByLabel('Speed limit unit'),
        count: pane.getByLabel('Maximum simultaneous downloads'),
        save: pane.getByRole('button', { name: 'Save' }),
        useDefault: pane.getByRole('button', { name: 'Use defaults' }),
      };
    };
    const a = speedParts(first);
    const b = speedParts(second);
    await first.goto(`${daemon.origin}/settings/downloads`);
    await second.goto(`${daemon.origin}/settings/downloads`);
    await expect(a.unlimited).toBeChecked();
    await expect(a.rate).toBeDisabled();
    await expect(a.pane.getByText('Shared by all downloads together, not given to each one.', { exact: false })).toBeVisible();
    await expect(a.pane.getByText('Allowed: Unlimited, or 1 KiB/s–1 TiB/s. Using the default.')).toBeVisible();

    // A limit needs a speed in range; a fraction is fine.
    await a.limited.check();
    await expect(a.rate).toBeFocused();
    await expect(a.save).toBeDisabled();
    await a.rate.fill('0.5');
    await a.unit.selectOption('KiB');
    await expect(a.pane.getByText('Enter a speed from 1 KiB/s to 1 TiB/s.')).toBeVisible();
    await a.rate.fill('1.5');
    await a.unit.selectOption('MiB');
    await expect(a.pane.getByText('Enter a speed from 1 KiB/s to 1 TiB/s.')).toHaveCount(0);

    // Saving both at once, while the other window has a draft of its own.
    await b.count.fill('5');
    await a.count.fill('3');
    await a.save.click();
    await expect(a.pane.getByText('Saved.', { exact: true })).toBeVisible();
    await expect(b.pane.getByText('The settings were changed elsewhere. Your unsaved values are kept.')).toBeVisible();
    await expect(b.count).toHaveValue('5');
    // The other window shows the saved limit, which its draft did not touch.
    await expect(b.limited).toBeChecked();
    await expect(b.rate).toHaveValue('1.5');
    await expect(b.unit).toHaveValue('MiB');
    await expect(b.pane.getByText('Allowed: Unlimited, or 1 KiB/s–1 TiB/s. Saved; the default is Unlimited.')).toBeVisible();
    await b.count.fill('3');

    // Unlimited is a choice of its own, kept as saved.
    await a.unlimited.check();
    await expect(a.rate).toBeDisabled();
    await a.save.click();
    await expect(a.pane.getByText('Saved.', { exact: true })).toBeVisible();
    await expect(b.unlimited).toBeChecked();
    await expect(a.pane.getByText('Allowed: Unlimited, or 1 KiB/s–1 TiB/s. Saved; the default is Unlimited.')).toBeVisible();

    // Use defaults drops both saved values.
    await a.useDefault.click();
    await expect(a.pane.getByText('Using the defaults: 4 at once, Unlimited.')).toBeVisible();
    await expect(a.count).toHaveValue('4');
    await expect(a.useDefault).toBeDisabled();
    await expectFitsWindow(first);
    // On a phone the page scrolls down, never sideways.
    await first.setViewportSize({ width: 390, height: 844 });
    const [wide, width] = await first.evaluate(() => [
      document.documentElement.scrollWidth,
      document.documentElement.clientWidth,
    ]);
    expect(wide).toBe(width);
  }
  await context.close();
});

test('Retry continues a cancelled download, once, and says when it had to start over', async ({ browser }) => {
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto(daemon.origin);
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');

  // A download cancelled past its first checkpoint keeps its saved data.
  // The earlier test made the archive folder the default.
  const NAME = 'again.bin';
  await add(page, fixture.put(NAME, 10 * MIB, 9 * MIB));
  await page.getByRole('button', { name: /^Active \(/ }).click();
  // The source filename is available while the stable job ID identifies it.
  const again = page.locator('tr[data-job]');
  await expect(again).toHaveCount(1);
  await expect(again.locator('.bytes')).toHaveText(/^[89]\.\d MiB of 10\.0 MiB$/);
  const job = /** @type {string} */ (await again.getAttribute('data-job'));
  await select(page, job);
  const retry = page.getByRole('button', { name: `Retry ${NAME}` });
  await expect(retry).toBeDisabled();
  await page.getByRole('button', { name: `Cancel ${NAME}` }).click();
  await page.getByRole('button', { name: /^Cancelled \(/ }).click();
  await expectState(page, job, 'Cancelled');
  await expect(retry).toBeEnabled();
  await expect(retry).toHaveAttribute('title', /^Continues from the [89]\.\d MiB saved when it can$/);

  // The row menu offers it too, with no note beside it.
  await row(page, job).click({ button: 'right' });
  let menu = await openMenu(page);
  await expect(menu.getByRole('menuitem', { name: /^Retry/ })).not.toHaveAttribute('aria-disabled');
  await expect(menu.getByRole('menuitem', { name: /^Retry/ })).toHaveText('Retry');
  await page.keyboard.press('Escape');

  // A retry sent for a revision the job has left is refused and explained
  // from the job's state; nothing starts.
  const commands = `**/v1/jobs/${job}/commands`;
  await page.route(commands, async (route) => {
    const body = JSON.parse(route.request().postData() ?? '{}');
    await route.continue({ postData: JSON.stringify({ ...body, expected_revision: body.expected_revision - 1 }) });
  });
  await retry.click();
  await expect(page.getByText(`${NAME} changed in the meantime and is still stopped.`, { exact: false })).toBeVisible();
  await page.unroute(commands);
  await expectState(page, job, 'Cancelled');

  // While the engine has not answered, Retry cannot be sent again, from
  // the toolbar or the menu.
  /** @type {() => void} */
  let answer = () => {};
  let held = 0;
  await page.route(commands, async (route) => {
    held += 1;
    await new Promise((resolve) => {
      answer = () => resolve(undefined);
    });
    await route.continue();
  });
  const requestsBefore = fixture.requested.length;
  await retry.click();
  await expect.poll(() => held).toBe(1);
  await expect(retry).toBeDisabled();
  await row(page, job).click({ button: 'right' });
  menu = await openMenu(page);
  await expect(menu.getByRole('menuitem', { name: /^Retry/ })).toHaveAttribute('aria-disabled', 'true');
  await expect(menu.getByRole('menuitem', { name: /^Retry/ })).toHaveText('Retry');
  await page.keyboard.press('Escape');
  answer();
  await expect(page.getByText(`Retrying ${NAME}. It continues from the`, { exact: false })).toBeVisible();
  await page.unroute(commands);
  expect(held).toBe(1);

  // It continues from the saved data: one request, for the rest.
  await page.getByRole('button', { name: /^All \(/ }).click();
  await page.goto(`${daemon.origin}/jobs/${job}`);
  await expect(page.locator('dd[data-fact="state"]')).toHaveText('Downloading');
  await expect.poll(() => fixture.requested.length).toBe(requestsBefore + 1);
  const resumedAt = Number(/^bytes=(\d+)-$/.exec(fixture.ranges.at(-1) ?? '')?.[1]);
  expect(resumedAt).toBeGreaterThanOrEqual(8 * MIB);
  // Reloading changes nothing about the accepted retry.
  await page.reload();
  await expect(page.locator('dd[data-fact="state"]')).toHaveText('Downloading');
  expect(fixture.requested.length).toBe(requestsBefore + 1);
  fixture.release(NAME);
  await expect(page.locator('dd[data-fact="state"]')).toHaveText('Done');
  expect((await daemon.published(NAME, daemon.archive)).equals(content(0, 10 * MIB))).toBe(true);
  await expect(page.locator('[data-note="restart"]')).toHaveCount(0);
  await expect(page.getByRole('button', { name: `Retry ${NAME}` })).toBeDisabled();
  await page.getByRole('button', { name: 'Close details' }).click();

  // A file replaced on the server cannot be continued: Retry starts over,
  // and the details say why.
  const CHANGED = 'changed.bin';
  await add(page, fixture.put(CHANGED, 10 * MIB, 9 * MIB));
  await page.getByRole('button', { name: /^Active \(/ }).click();
  const changed = page.locator('tr[data-job]');
  await expect(changed).toHaveCount(1);
  await expect(changed.locator('.bytes')).toHaveText(/^[89]\.\d MiB of 10\.0 MiB$/);
  const second = /** @type {string} */ (await changed.getAttribute('data-job'));
  await select(page, second);
  await page.getByRole('button', { name: `Cancel ${CHANGED}` }).click();
  await expect(page.locator('dd[data-fact="state"]')).toHaveText('Cancelled');
  fixture.replace(CHANGED);
  fixture.release(CHANGED);
  await page.getByRole('button', { name: `Retry ${CHANGED}` }).click();
  await expect(page.locator('dd[data-fact="state"]')).toHaveText('Done');
  await expect(page.locator('[data-note="restart"]')).toHaveText(
    'Started over from the beginning: the file on the server changed since the saved data was downloaded.',
  );
  await page.getByRole('tab', { name: 'Engine details' }).click();
  await expect(page.locator('dd[data-fact="restarted-because"]')).toHaveText(
    'the file on the server changed since the saved data was downloaded',
  );
  expect((await daemon.published(CHANGED, daemon.archive)).equals(content(0, 10 * MIB))).toBe(true);
  await context.close();
});

test('accounts are signed in on the service, checked, disabled, signed in again, and removed', async ({
  browser,
}) => {
  const context = await browser.newContext();
  /** @type {string[]} */
  const offOrigin = [];
  context.on('request', (request) => {
    if (!request.url().startsWith(`${daemon.origin}/`)) offOrigin.push(request.url());
  });
  const page = await context.newPage();
  await page.goto(daemon.origin);
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
  await page.goto(`${daemon.origin}/settings/accounts`);
  const pane = page.locator('.settings-content');
  const accounts = pane.locator('section', { has: page.getByRole('heading', { name: /Accounts & services/ }) });
  const table = accounts.getByRole('table', { name: 'Accounts' });
  const status = accounts.locator('[data-key="accounts-message"]');
  await expect(table.locator('tbody tr')).toHaveText(['No accounts added yet.']);
  await expect(accounts.getByText(/Signing in to a service and downloading through it are separate/)).toBeVisible();

  // Pick Real-Debrid, name the account, and approve on the service's site.
  await accounts.getByRole('button', { name: 'Add account…' }).click();
  await page.getByRole('dialog', { name: 'Add account' }).getByRole('button', { name: 'Sign in to Real-Debrid' }).click();
  const signIn = page.getByRole('dialog', { name: 'Sign in to Real-Debrid' });

  // Closed while it asks for a code: the sign-in that started anyway is
  // cancelled when the answer arrives, not left holding a slot.
  /** @type {() => void} */
  let release = () => {};
  const held = new Promise((resolve) => (release = () => resolve(undefined)));
  await page.route('**/v1/accounts/authorizations', async (route) => {
    await held;
    await route.continue();
  });
  await signIn.getByRole('button', { name: 'Continue' }).click();
  await expect(signIn.getByRole('status')).toHaveText('Asking for a code…');
  await page.keyboard.press('Escape');
  await expect(signIn).toHaveCount(0);
  const cancel = page.waitForRequest(
    (request) => request.method() === 'POST' && /\/v1\/accounts\/authorizations\/[^/]+\/cancel$/.test(request.url()),
  );
  release();
  const cancelled = await (await cancel).response();
  expect(cancelled?.status()).toBe(200);
  expect((await cancelled?.json()).state).toBe('cancelled');
  await page.unroute('**/v1/accounts/authorizations');
  await accounts.getByRole('button', { name: 'Add account…' }).click();
  await page.getByRole('dialog', { name: 'Add account' }).getByRole('button', { name: 'Sign in to Real-Debrid' }).click();

  await expect(signIn.getByText('Wayfetch never asks for your password', { exact: false })).toBeVisible();
  await expect(signIn.getByText(/Downloading through Real-Debrid is not supported yet/)).toHaveCount(0);
  await expect(signIn.locator('input[type="password"]')).toHaveCount(0);
  const name = signIn.getByLabel('Name in Wayfetch (optional)');
  await expect(name).toBeFocused();
  await name.fill('Main');
  await signIn.getByRole('button', { name: 'Continue' }).click();
  await expect(signIn.locator('.user-code')).toHaveText(RD_USER_CODE);
  await expect(signIn.getByRole('link')).toHaveAttribute('href', `${realDebrid.base}device`);
  await expect(signIn.getByRole('status')).toHaveText(/Waiting for you to approve Wayfetch on Real-Debrid/);
  // The stand-in approves on its second poll; the dialog closes then.
  await expect(signIn).toHaveCount(0);
  await expect(status).toHaveText('Account added and signed in.');
  const added = table.locator('tbody tr');
  await expect(added).toHaveCount(1);
  await expect(added.locator('td').nth(0)).toHaveText('Real-Debrid');
  await expect(added.locator('td').nth(1)).toHaveText(/^Main\s*Premium\s*synthetic-user$/);
  // One menu holds the row's commands; nothing else is shown while the
  // account needs nothing.
  const more = added.getByRole('button', { name: 'Actions for Main' });
  await expect(added.locator('td.row-actions button')).toHaveCount(1);
  await expect(more).toHaveAttribute('aria-haspopup', 'menu');
  const menu = page.getByRole('menu', { name: 'Account actions' });
  /** @param {string} name */
  const item = (name) => menu.getByRole('menuitem', { name });
  /** @param {string} name */
  const run = async (name) => {
    await more.click();
    await item(name).click();
    await expect(menu).toBeHidden();
  };
  await expect(added.locator('td').nth(2)).toHaveText('Signed in');
  await expect(added.locator('td').nth(3)).not.toHaveText('—');

  // Checked on request only; listing never asks the service.
  const users = realDebrid.requested.filter((path) => path === '/rest/1.0/user').length;
  await page.reload();
  await expect(added.locator('td').nth(2)).toHaveText('Signed in');
  expect(realDebrid.requested.filter((path) => path === '/rest/1.0/user').length).toBe(users);
  await more.click();
  await expect(menu.getByRole('menuitem')).toHaveText(['Check', 'Sign in again…', 'Disable', 'Remove…']);
  await expect(item('Check')).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(menu).toBeHidden();
  await expect(more).toBeFocused();
  // Right-clicking the row opens the same menu.
  await added.locator('td').nth(0).click({ button: 'right' });
  await expect(menu).toBeVisible();
  await expect(added).toHaveClass(/menu-open/);
  await page.keyboard.press('Escape');
  await run('Check');
  await expect(status).toHaveText('Checked: Signed in.');
  expect(realDebrid.requested.filter((path) => path === '/rest/1.0/user').length).toBe(users + 1);

  // Disabled: kept, but not used.
  await run('Disable');
  await expect(status).toHaveText('Disabled: kept, but not used.');
  await expect(added.locator('td').nth(2)).toHaveText('Disabled');
  await more.click();
  await expect(item('Check Account is disabled')).toHaveAttribute('aria-disabled', 'true');
  await expect(item('Sign in again… Account is disabled')).toHaveAttribute('aria-disabled', 'true');
  await page.keyboard.press('Escape');
  await run('Enable');
  await expect(added.locator('td').nth(2)).toHaveText('Signed in');

  // Signed in again: new sign-in, same account.
  await run('Sign in again…');
  const again = page.getByRole('dialog', { name: 'Sign in to Real-Debrid again' });
  await expect(again.locator('.user-code')).toHaveText(RD_USER_CODE);
  await expect(again.getByRole('textbox')).toHaveCount(0);
  await expect(again).toHaveCount(0);
  await expect(status).toHaveText('Signed in again.');
  await expect(table.locator('tbody tr')).toHaveCount(1);

  // A sign-in closed before approval is cancelled.
  const polls = realDebrid.requested.length;
  await accounts.getByRole('button', { name: 'Add account…' }).click();
  await page.getByRole('dialog', { name: 'Add account' }).getByRole('button', { name: 'Sign in to Real-Debrid' }).click();
  await signIn.getByRole('button', { name: 'Continue' }).click();
  await expect(signIn.locator('.user-code')).toHaveText(RD_USER_CODE);
  await signIn.getByRole('button', { name: 'Cancel' }).click();
  await expect(signIn).toHaveCount(0);
  await page.waitForTimeout(2500);
  const after = realDebrid.requested.length;
  await page.waitForTimeout(2500);
  expect(realDebrid.requested.length).toBe(after);
  expect(after).toBeGreaterThan(polls);
  await expect(table.locator('tbody tr')).toHaveCount(1);

  // Removed after confirming.
  await run('Remove…');
  const confirm = page.getByRole('dialog', { name: 'Remove account' });
  await expect(confirm.getByText(/The account itself stays at the service/)).toBeVisible();
  await confirm.getByRole('button', { name: 'Remove' }).click();
  await expect(status).toHaveText('Removed.');
  await expect(table.locator('tbody tr')).toHaveText(['No accounts added yet.']);

  // Nothing the service issued reached the page, its storage, or its
  // address, and the page itself contacted only the daemon.
  const seen = await page.evaluate(() =>
    JSON.stringify([
      document.documentElement.outerHTML,
      location.href,
      Object.entries(localStorage),
      Object.entries(sessionStorage),
    ]),
  );
  for (const secret of Object.values(RD_SECRETS)) {
    expect(seen).not.toContain(secret);
    expect(daemon.log).not.toContain(secret);
  }
  expect(offOrigin).toEqual([]);
  await context.close();
});

test('native provider links require sign-in and provider Forbidden keeps the Wayfetch session', async ({ browser }) => {
  const nativeProvider = await new RealDebridFixture().start();
  const local = await new Daemon(await freePort(), { realDebrid: nativeProvider.base }).init();
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    await local.start();
    await page.goto(local.origin);
    await page.getByLabel('Setup secret').fill((await local.setupSecret()).trim());
    await page.getByLabel('Owner name').fill('owner');
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
    await page.getByLabel('Repeat password').fill(PASSWORD);
    await page.getByRole('button', { name: 'Create owner' }).click();
    await expect(page.locator('.statusbar .status')).toHaveText('Live');
    /** @param {string} link */
    const collectOne = async (link) => {
      await page.goto(`${local.origin}/links`);
      await page.getByRole('button', { name: 'Add links' }).click();
      await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(link);
      await page.getByRole('button', { name: 'Collect links' }).click();
      await expect(page.getByText('Collected 1 link for review.')).toBeVisible();
      await page.locator('tr[data-link] input[type="checkbox"]').check();
      await page.getByRole('button', { name: 'Start selected' }).click();
      await expect(page.getByRole('dialog')).toHaveCount(0);
    };
    const jobs = async () => (await (await page.request.get(`${local.origin}/v1/jobs?page_size=500`)).json()).items;
    await collectOne('https://real-debrid.com/d/SYNTHETICNATIVE123');
    await expect(page.locator('.links-page .notice')).toContainText('Sign-in required');
    await expect(page.locator('.links-page .notice')).toContainText('Settings > Accounts & services');
    await expect(page.locator('tr[data-link]')).toHaveCount(1);
    expect(await jobs()).toEqual([]);
    expect(nativeProvider.requested).toEqual([]);

    await page.goto(`${local.origin}/settings/accounts`);
    await page.getByRole('button', { name: 'Add account…' }).click();
    await page.getByRole('dialog', { name: 'Add account' }).getByRole('button', { name: 'Sign in to Real-Debrid' }).click();
    const signIn = page.getByRole('dialog', { name: 'Sign in to Real-Debrid' });
    await signIn.getByRole('button', { name: 'Continue' }).click();
    await expect(signIn).toHaveCount(0);
    await expect(page.locator('[data-key="accounts-message"]')).toHaveText('Account added and signed in.');
    await page.goto(`${local.origin}/links`);
    await page.locator('tr[data-link] input[type="checkbox"]').check();
    await page.getByRole('button', { name: 'Start selected' }).click();
    await expect(page.getByText('Started 1 download.')).toBeVisible();
    await expect.poll(async () => (await jobs())[0]?.stage).toBe('complete');
    const native = (await jobs())[0];
    expect(native.route.service).toBe('real-debrid');
    expect(nativeProvider.requested.filter((path) => path === '/rest/1.0/hosts/domains')).toHaveLength(0);
    expect(nativeProvider.requested.filter((path) => path === '/rest/1.0/unrestrict/link')).toHaveLength(1);

    nativeProvider.refuseUnrestrict = 403;
    await collectOne('https://real-debrid.com/d/SYNTHETICDENIED123');
    await expect(page.getByText('Started 1 download.')).toBeVisible();
    await expect.poll(async () => (await jobs()).find((/** @type {{ id: string }} */ job) => job.id !== native.id)?.stage).toBe('failed');
    const denied = (await jobs()).find((/** @type {{ id: string }} */ job) => job.id !== native.id);
    expect(denied.failure.code).toBe('invalid_credentials');
    await page.goto(`${local.origin}/jobs/${denied.id}`);
    await expect(page.locator('.inspector')).toContainText('Forbidden');
    await expect(page.locator('.inspector')).toContainText('Settings > Accounts & services');
    await expect(page.locator('.inspector')).not.toContainText('synthetic-private-provider-detail');
    await expect(page.locator('.statusbar .status')).toHaveText('Live');
    expect((await (await page.request.get(`${local.origin}/v1/auth/session`)).json()).authenticated).toBe(true);
    expect(nativeProvider.requested.filter((path) => path === '/rest/1.0/unrestrict/link')).toHaveLength(2);
  } finally {
    await context.close();
    await local.remove();
    await nativeProvider.stop();
  }
});

test('links download through a chosen Real-Debrid account, and a lost answer is asked again only once confirmed', async ({
  browser,
}) => {
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto(daemon.origin);
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');

  // Without an account, starting asks nothing.
  const direct = fixture.put('direct-before-account.bin', MIB);
  await add(page, direct);
  await expect(page.getByRole('dialog')).toHaveCount(0);

  await page.goto(`${daemon.origin}/settings/accounts`);
  const accounts = page.locator('.settings-content section', { has: page.getByRole('heading', { name: /Accounts & services/ }) });
  await accounts.getByRole('button', { name: 'Add account…' }).click();
  await page.getByRole('dialog', { name: 'Add account' }).getByRole('button', { name: 'Sign in to Real-Debrid' }).click();
  const signIn = page.getByRole('dialog', { name: 'Sign in to Real-Debrid' });
  await signIn.getByLabel('Name in Wayfetch (optional)').fill('Main');
  await signIn.getByRole('button', { name: 'Continue' }).click();
  await expect(signIn).toHaveCount(0);
  await expect(accounts.locator('[data-key="accounts-message"]')).toHaveText('Account added and signed in.');
  await page.locator('[data-key="settings-back"]').click();

  // Collecting generates nothing at the provider.
  const asked = () => realDebrid.requested.filter((path) => path === '/rest/1.0/unrestrict/link').length;
  const original = 'https://hoster.example/file/through-account';
  const lost = 'https://hoster.example/file/answer-lost';
  const mixedDirect = fixture.put('mixed-direct.bin', MIB);
  await collect(page, `${original}\n${lost}\n${mixedDirect}`, 'Collected 3 links for review.');
  expect(asked()).toBe(0);
  const linkRow = (/** @type {string} */ name) =>
    page.locator('tr[data-link]', { has: page.locator('td.name', { hasText: new RegExp(`^${name}$`) }) });

  // Explicit override remains in the existing menu. Cancelling it keeps
  // the link; ordinary Start routes automatically without a prompt.
  await page.getByRole('checkbox', { name: 'Select all links shown' }).uncheck();
  await linkRow('through-account').locator('input[type="checkbox"]').check();
  await linkRow('through-account').getByRole('button', { name: 'Actions for through-account' }).click();
  await page.getByRole('menuitem', { name: 'Download via…' }).click();
  const choose = page.getByRole('dialog', { name: 'Start 1 link' });
  await expect(choose.getByRole('radio', { name: /^Direct/ })).toBeChecked();
  await expect(choose.getByRole('radio')).toHaveCount(2);
  expect(asked()).toBe(0);
  await choose.getByRole('button', { name: 'Cancel' }).click();
  // Its host is beyond what this daemon may reach, but the account's
  // provider checks it, without generating a link.
  await expect(linkRow('through-account').locator('td.link-status')).toContainText('Online');
  await expect(linkRow('through-account').locator('td.link-status')).toHaveAttribute('title', 'Checked through Real-Debrid.');
  expect(realDebrid.requested).toContain('/rest/1.0/unrestrict/check');
  expect(asked()).toBe(0);
  await linkRow('mixed-direct.bin').locator('input[type="checkbox"]').check();
  await page.getByRole('button', { name: 'Start selected' }).click();
  await expect(page.getByRole('dialog')).toHaveCount(0);
  await expect(page.getByText('Started 2 downloads.')).toBeVisible();
  // Downloads routed through an account, as the API reports them.
  const routed = async () => {
    const response = await page.request.get(`${daemon.origin}/v1/jobs?page_size=500`);
    expect(response.ok()).toBe(true);
    return (await response.json()).items.filter((/** @type {{ route?: unknown }} */ job) => job.route);
  };
  await expect.poll(async () => (await routed()).map((/** @type {{ stage: string }} */ j) => j.stage)).toEqual(['complete']);
  const [through] = await routed();
  expect(through.route.service).toBe('real-debrid');
  expect(asked()).toBe(1);
  await expect.poll(async () => {
    const jobs = (await (await page.request.get(`${daemon.origin}/v1/jobs?page_size=500`)).json()).items;
    return jobs.find((/** @type {{ artifact?: { path: string } }} */ job) => job.artifact?.path === 'mixed-direct.bin')?.stage;
  }).toBe('complete');
  const mixedJobs = (await (await page.request.get(`${daemon.origin}/v1/jobs?page_size=500`)).json()).items;
  const mixedJob = mixedJobs.find((/** @type {{ artifact?: { path: string } }} */ job) => job.artifact?.path === 'mixed-direct.bin');
  expect(mixedJob).toBeDefined();
  expect(mixedJob.route).toBeUndefined();
  // The provider's file name, saved in the folder the link was collected to.
  expect(through.artifact.path).toBe(realDebrid.filename);
  const settings = await (await page.request.get(`${daemon.origin}/v1/settings`)).json();
  const folder = settings.roots.find((/** @type {{ id: string }} */ r) => r.id === through.artifact.root).path;
  const saved = await readFile(join(folder, through.artifact.path));
  expect(saved.equals(content(0, realDebrid.size))).toBe(true);
  expect(realDebrid.downloadAuth).toEqual([undefined]);
  await page.goto(`${daemon.origin}/jobs/${through.id}`);
  await expect(page.locator('[data-fact="download-via"]')).toHaveText('Real-Debrid, Main');

  // The answer to the next request is lost: the download stops, saying
  // the outcome is unknown, and Retry asks before asking again.
  realDebrid.lose = 1;
  await page.goto(`${daemon.origin}/links`);
  await linkRow('answer-lost').locator('input[type="checkbox"]').check();
  await page.getByRole('button', { name: 'Start selected' }).click();
  await expect(page.getByRole('dialog')).toHaveCount(0);
  await expect(page.getByText('Started 1 download.')).toBeVisible();
  await expect.poll(async () => (await routed()).length).toBe(2);
  const lostJob = (await routed()).find((/** @type {{ id: string }} */ j) => j.id !== through.id);
  await page.goto(`${daemon.origin}/jobs/${lostJob.id}`);
  await expect(page.locator('[data-fact="download-via"]')).toHaveText('Real-Debrid, Main');
  await expect(page.locator('.inspector')).toContainText('the provider may do that work twice');
  expect(asked()).toBe(2);
  const retry = page.getByRole('button', { name: `Retry ${lostJob.id}` });
  await retry.click();
  const confirm = page.getByRole('dialog', { name: /^Retry / });
  await expect(confirm).toContainText('may count against the account a second time');
  await confirm.getByRole('button', { name: 'Cancel' }).click();
  await expect(confirm).toHaveCount(0);
  expect(asked()).toBe(2);
  await retry.click();
  await confirm.getByRole('button', { name: 'Ask again' }).click();
  await expect
    .poll(async () => (await routed()).find((/** @type {{ id: string }} */ j) => j.id === lostJob.id).stage)
    .toBe('complete');
  expect(asked()).toBe(3);
  expect(realDebrid.downloadAuth).toEqual([undefined, undefined]);

  // An explicit Direct override on a supported host bypasses the account
  // through the real HTTP transport. The synthetic hostname cannot resolve,
  // but the job still keeps Direct and never asks the provider.
  await page.goto(`${daemon.origin}/links`);
  await collect(page, 'https://hoster.example/file/direct-override', 'Collected 1 link for review.');
  const beforeDirect = (await (await page.request.get(`${daemon.origin}/v1/jobs?page_size=500`)).json()).items.map((/** @type {{ id: string }} */ job) => job.id);
  await linkRow('direct-override').getByRole('button', { name: 'Actions for direct-override' }).click();
  await page.getByRole('menuitem', { name: 'Download via…' }).click();
  await choose.getByRole('radio', { name: /^Direct/ }).check();
  await choose.getByRole('button', { name: 'Start' }).click();
  await expect(page.getByText('Started 1 download.')).toBeVisible();
  const allJobs = (await (await page.request.get(`${daemon.origin}/v1/jobs?page_size=500`)).json()).items;
  const explicitDirect = allJobs.find((/** @type {{ id: string }} */ job) => !beforeDirect.includes(job.id));
  expect(explicitDirect).toBeDefined();
  expect(explicitDirect.route).toBeUndefined();
  expect(asked()).toBe(3);
  await page.goto(`${daemon.origin}/jobs/${explicitDirect.id}`);
  await expect(page.locator('[data-fact="download-via"]')).toHaveText('Direct');

  // No generated link or credential reached the page, its storage, or the
  // daemon's output.
  const seen = await page.evaluate(() =>
    JSON.stringify([document.documentElement.outerHTML, location.href, Object.entries(localStorage), Object.entries(sessionStorage)]),
  );
  for (const secret of [...Object.values(RD_SECRETS), RD_SIGNATURE]) {
    expect(seen).not.toContain(secret);
    expect(daemon.log).not.toContain(secret);
  }
  await context.close();
});

/**
 * The window sizes Settings is checked at: wide, the desktop app's default
 * and minimum, and a phone.
 */
const WIDTHS = [1280, 1100, 640, 360];

/**
 * Where the Settings button is at each of `WIDTHS`, after checking that it
 * is the toolbar's only one, at the right end of its first row, clear of
 * every other control, with nothing scrolling sideways.
 * @param {import('@playwright/test').Page} page
 */
async function settingsPlaces(page) {
  /** @type {string[]} */
  const places = [];
  for (const width of WIDTHS) {
    await page.setViewportSize({ width, height: 720 });
    const place = await page.evaluate(() => {
      const shown = [...document.querySelectorAll('[data-key="open-settings"]')].filter(
        (el) => /** @type {HTMLElement} */ (el).offsetParent !== null,
      );
      if (shown.length !== 1) return `${shown.length} Settings buttons`;
      const button = shown[0];
      const toolbar = /** @type {HTMLElement} */ (button.closest('.toolbar'));
      const box = button.getBoundingClientRect();
      const bar = toolbar.getBoundingClientRect();
      const brand = /** @type {Element} */ (toolbar.querySelector('.brand')).getBoundingClientRect();
      const style = getComputedStyle(toolbar);
      if (Math.round(bar.right - parseFloat(style.paddingRight) - box.right) !== 0) return 'not at the right end';
      if (box.top >= brand.bottom) return 'not in the first row';
      for (const other of toolbar.querySelectorAll('button, a, input, .tool-group')) {
        if (other === button || /** @type {HTMLElement} */ (other).offsetParent === null) continue;
        const r = other.getBoundingClientRect();
        if (r.left < box.right && box.left < r.right && r.top < box.bottom && box.top < r.bottom) {
          return `overlaps ${other.textContent}`;
        }
      }
      const root = document.documentElement;
      if (root.scrollWidth > root.clientWidth) return 'scrolls sideways';
      return `${box.left},${box.top} ${box.width}x${box.height}`;
    });
    places.push(`${width}: ${place}`);
  }
  await page.setViewportSize({ width: 1280, height: 720 });
  return places;
}

/**
 * Checks that the shown filter field keeps the toolbar's first row at the
 * desktop app's default width, beside the tools rather than below them.
 * @param {import('@playwright/test').Page} page
 */
async function expectSearchInFirstRow(page) {
  await page.setViewportSize({ width: 1100, height: 720 });
  const place = await page.evaluate(() => {
    const search = [...document.querySelectorAll('.toolbar .search')].find(
      (el) => /** @type {HTMLElement} */ (el).offsetParent !== null,
    );
    if (!search) return 'no filter field';
    const brand = /** @type {Element} */ (search.closest('.toolbar')?.querySelector('.brand'));
    return search.getBoundingClientRect().top < brand.getBoundingClientRect().bottom ? 'first row' : 'wrapped';
  });
  await page.setViewportSize({ width: 1280, height: 720 });
  expect(place).toBe('first row');
}

test('Settings stays at the right end of the toolbar in every view and state', async ({ browser }) => {
  // A daemon of its own, so Downloads starts empty.
  const own = await new Daemon(await freePort()).init();
  await own.start();
  try {
    const page = await (await browser.newContext()).newPage();
    await page.goto(own.origin);
    await page.getByLabel('Setup secret').fill((await own.setupSecret()).trim());
    await page.getByLabel('Owner name').fill('owner');
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
    await page.getByLabel('Repeat password').fill(PASSWORD);
    await page.getByRole('button', { name: 'Create owner' }).click();
    await expect(page.getByText('No downloads yet.')).toBeVisible();

    // Downloads with no jobs sets the place every other view keeps.
    const places = await settingsPlaces(page);
    expect(places.join('\n')).not.toMatch(/: [^\d]/);
    await expectSearchInFirstRow(page);
    const expectSame = async () => expect(await settingsPlaces(page)).toEqual(places);
    const settings = page.locator('[data-key="open-settings"]:visible');
    await expect(settings).toHaveAccessibleName('Settings');
    await expect(settings.locator('svg.icon-settings')).toHaveCount(1);

    await page.getByRole('button', { name: 'Add links' }).click();
    await expect(page).toHaveURL(`${own.origin}/add`);
    await expectSame();

    // Links, with a notice and a count, then with links selected.
    const NAME = 'settings-place.bin';
    await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(fixture.put(NAME, 10 * MIB, 9 * MIB));
    await page.getByRole('button', { name: 'Collect links' }).click();
    await expect(page).toHaveURL(`${own.origin}/links`);
    await expect(page.getByText('Collected 1 link for review.')).toBeVisible();
    await expect(page.getByRole('link', { name: 'Links (1)' })).toBeVisible();
    await expectSame();
    await expectSearchInFirstRow(page);
    await expect(page.locator('.links-page .selection-summary strong')).toHaveText('1 link selected');
    await expectSame();

    // Downloads with a job in progress, then with it selected.
    await page.getByRole('button', { name: 'Start selected' }).click();
    const job = page.locator('tr[data-job="job-1"]');
    await expect(job.locator('td[data-state]')).toHaveText('Downloading');
    await expectSame();
    await job.locator('td.name a').click();
    await expect(page.getByRole('button', { name: `Pause ${NAME}` })).toBeEnabled();
    await expectSame();
    await expectSearchInFirstRow(page);

    // Settings itself has Back and its sections instead.
    await settings.click();
    await expect(page).toHaveURL(`${own.origin}/settings`);
    await expect(page.locator('[data-key="open-settings"]:visible')).toHaveCount(0);
    await page.getByRole('navigation', { name: 'Settings sections' }).getByRole('link', { name: 'Display' }).click();
    await expect(page).toHaveURL(`${own.origin}/settings/display`);
    await page.getByRole('button', { name: 'Back to Downloads' }).click();
    await expect(page).toHaveURL(`${own.origin}/jobs/job-1`);
    await expect(settings).toBeFocused();
    await expectSame();
    fixture.release(NAME);
    await expect(job.locator('td[data-state]')).toHaveText('Done');
    await expectSame();
  } finally {
    await own.remove();
  }
});

test('removing a download requires an explicit file choice and deletes only its data', async ({ browser }) => {
  const own = await new Daemon(await freePort()).init();
  await own.start();
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    await page.goto(own.origin);
    await page.getByLabel('Setup secret').fill((await own.setupSecret()).trim());
    await page.getByLabel('Owner name').fill('owner');
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
    await page.getByLabel('Repeat password').fill(PASSWORD);
    await page.getByRole('button', { name: 'Create owner' }).click();
    await expect(page.getByText('No downloads yet.')).toBeVisible();
    // A name filter left on Downloads cannot hide what is started next.
    await page.locator('#filter-names').fill('nothing matches this');
    await page.getByRole('button', { name: 'Add links' }).click();
    await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(fixture.put('delete-choice.bin', MIB));
    await page.getByRole('button', { name: 'Collect links' }).click();
    await expect(page.locator('.links-page .selection-summary strong')).toHaveText('1 link selected');
    await page.getByRole('button', { name: 'Start selected' }).click();
    await expect(page.getByText('Started 1 download.')).toBeVisible();
    await expect(page.locator('#filter-names')).toHaveValue('');
    await expect(row(page, 'job-1')).toBeVisible();
    await page.goto(`${own.origin}/links`);
    await expect(page.locator('.links-page .selection-summary')).not.toContainText('already in Downloads');
    await expect(page.locator('.links-page tr[data-link]')).toHaveCount(0);
    await page.getByRole('link', { name: 'Downloads', exact: true }).click();
    await expectState(page, 'job-1', 'Done');
    await writeFile(join(/** @type {string} */ (own.root), 'unrelated.bin'), 'keep me');
    await row(page, 'job-1').getByRole('button', { name: 'Actions for delete-choice.bin' }).click();
    await page.getByRole('menuitem', { name: /^Remove…/ }).click();
    const dialog = page.getByRole('dialog', { name: 'Remove download' });
    await expect(dialog.getByRole('radio', { name: 'Remove from list only (keep files)' })).toBeChecked();
    await dialog.getByRole('radio', { name: 'Delete downloaded file/data and remove from list' }).check();
    await expect(dialog.getByRole('button', { name: 'Delete files and remove' })).toBeVisible();
    await dialog.getByRole('button', { name: 'Cancel', exact: true }).click();
    expect((await own.published('delete-choice.bin')).equals(content(0, MIB))).toBe(true);
    await expect(row(page, 'job-1')).toHaveCount(1);
    await row(page, 'job-1').getByRole('button', { name: 'Actions for delete-choice.bin' }).click();
    await page.getByRole('menuitem', { name: /^Remove…/ }).click();
    await expect(dialog.getByRole('radio', { name: 'Remove from list only (keep files)' })).toBeChecked();
    await dialog.getByRole('radio', { name: 'Delete downloaded file/data and remove from list' }).check();
    await dialog.getByRole('button', { name: 'Delete files and remove' }).click();
    await expect(dialog).toHaveCount(0);
    await expect(row(page, 'job-1')).toHaveCount(0);
    await expect(own.published('delete-choice.bin')).rejects.toThrow();
    expect(await readFile(join(/** @type {string} */ (own.root), 'unrelated.bin'), 'utf8')).toBe('keep me');
    await page.reload();
    await expect(page.getByText('No downloads yet.')).toBeVisible();
  } finally {
    await context.close();
    await own.remove();
  }
});

test('ticked downloads are paused, resumed, cancelled, and removed together', async ({ browser }) => {
  const own = await new Daemon(await freePort()).init();
  await own.start();
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    await page.goto(own.origin);
    await page.getByLabel('Setup secret').fill((await own.setupSecret()).trim());
    await page.getByLabel('Owner name').fill('owner');
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
    await page.getByLabel('Repeat password').fill(PASSWORD);
    await page.getByRole('button', { name: 'Create owner' }).click();
    await expect(page.getByText('No downloads yet.')).toBeVisible();
    await page.getByRole('button', { name: 'Add links' }).click();
    await page
      .getByLabel('Links, text with links in it, or a page’s HTML')
      .fill([fixture.put('bulk-a.bin', 10 * MIB, 9 * MIB), fixture.put('bulk-b.bin', 10 * MIB, 9 * MIB), fixture.put('bulk-c.bin', MIB)].join('\n'));
    await page.getByRole('button', { name: 'Collect links' }).click();
    await expect(page.locator('.links-page .selection-summary strong')).toHaveText('3 links selected');
    await page.getByRole('button', { name: 'Start selected' }).click();
    await expect(page).toHaveURL(`${own.origin}/`);
    await expect(page.getByText('Started 3 downloads.')).toBeVisible();
    const byName = (/** @type {string} */ name) => page.locator('tr[data-job]', { hasText: name });
    const [a, b, c] = ['bulk-a.bin', 'bulk-b.bin', 'bulk-c.bin'].map(byName);
    await expect(c.locator('td[data-state]')).toHaveText('Done');
    const summary = page.locator('.downloads-view .selection-summary');

    // Rows alternate by position, and a finished bar says so in words.
    expect(await page.locator('tr[data-job]').evaluateAll((rows) => rows.map((r) => r.getAttribute('data-stripe')))).toEqual([
      'base',
      'alternate',
      'base',
    ]);
    const bar = c.getByRole('progressbar', { name: 'bulk-c.bin progress' });
    await expect(bar).toHaveAttribute('aria-valuenow', '100');
    await expect(bar).toHaveAttribute('aria-valuetext', 'Done; 100% downloaded');
    await expect(c.locator('.dot')).toHaveAttribute('data-status', 'done');

    // The table's footer says which rows are shown; one page pages nowhere.
    const pager = page.locator('.downloads-view .pager');
    await expect(pager.locator('.range')).toHaveText('1–3 of 3');
    await expect(pager.locator('.page')).toHaveText('Page 1 of 1');
    await expect(pager.getByRole('button', { name: 'Previous page' })).toBeDisabled();
    await expect(pager.getByRole('button', { name: 'Next page' })).toBeDisabled();

    // A Size, Name, Host or State header sorts the page by it, again
    // reverses it, and a third click goes back to the engine's order; stripes follow the new order.
    const names = () => page.locator('tr[data-job] td.name').allTextContents();
    const sizeHeader = page.locator('.downloads-view th.num', { hasText: 'Size' });
    await expect(page.locator('.downloads-view th[aria-sort]')).toHaveCount(0);
    const engineOrder = await names();
    await sizeHeader.getByRole('button').click();
    await expect(sizeHeader).toHaveAttribute('aria-sort', 'ascending');
    expect((await names())[0]).toBe('bulk-c.bin');
    expect(await page.locator('tr[data-job]').evaluateAll((rows) => rows.map((r) => r.getAttribute('data-stripe')))).toEqual([
      'base',
      'alternate',
      'base',
    ]);
    await sizeHeader.getByRole('button').click();
    await expect(sizeHeader).toHaveAttribute('aria-sort', 'descending');
    expect((await names())[2]).toBe('bulk-c.bin');
    await sizeHeader.getByRole('button').click();
    await expect(page.locator('.downloads-view th[aria-sort]')).toHaveCount(0);
    expect(await names()).toEqual(engineOrder);
    // By keyboard too: the redrawn header keeps focus on the same button.
    const nameSort = page.locator('.downloads-view th.name button.sort');
    await nameSort.focus();
    await page.keyboard.press('Enter');
    await expect(page.locator('.downloads-view th.name')).toHaveAttribute('aria-sort', 'ascending');
    await expect(nameSort).toBeFocused();
    await page.keyboard.press('Enter');
    await expect(page.locator('.downloads-view th.name')).toHaveAttribute('aria-sort', 'descending');
    await expect(nameSort).toBeFocused();
    await page.keyboard.press('Enter');
    await expect(page.locator('.downloads-view th[aria-sort]')).toHaveCount(0);
    // Amounts that change during a transfer do not sort, so rows stay put.
    for (const label of ['Progress', 'Speed', 'Left']) {
      await expect(page.locator('.downloads-view thead th', { hasText: label }).locator('button')).toHaveCount(0);
    }

    // The line above the table keeps its room while nothing is ticked, so
    // ticking moves no row.
    await expect(summary).toBeEmpty();
    await expect(page.locator('.downloads-view .list-bar')).toBeVisible();
    const before = await a.boundingBox();
    await a.getByRole('checkbox').check();
    await expect(summary.locator('strong')).toHaveText('1 download selected');
    expect((await a.boundingBox())?.y).toBe(before?.y);

    // Ctrl+A (Cmd+A on a Mac) ticks every download shown instead of the
    // page's text; in the filter field it stays the field's own.
    await page.locator('#filter-names').focus();
    await page.keyboard.press('ControlOrMeta+A');
    await expect(summary.locator('strong')).toHaveText('1 download selected');
    await page.locator('#filter-names').blur();
    await page.keyboard.press('ControlOrMeta+A');
    await expect(summary.locator('strong')).toHaveText('3 downloads selected');
    expect(await page.evaluate(() => String(window.getSelection()))).toBe('');
    await b.getByRole('checkbox').uncheck();
    await c.getByRole('checkbox').uncheck();
    await expect(summary.locator('strong')).toHaveText('1 download selected');

    // Ticking two: the toolbar acts on both, not on a download in the details.
    await b.getByRole('checkbox').check();
    await expect(summary.locator('strong')).toHaveText('2 downloads selected');
    await expect(a).toHaveClass(/ticked/);
    await page.getByRole('button', { name: 'Pause 2 downloads' }).click();
    await expect(page.getByText('Paused 2 downloads.')).toBeVisible();
    await expect(a.locator('td[data-state]')).toHaveText('Paused');
    await expect(b.locator('td[data-state]')).toHaveText('Paused');

    // Select all; a command skips those it does not apply to and says so.
    await page.getByRole('checkbox', { name: 'Select all downloads shown' }).check();
    await expect(summary.locator('strong')).toHaveText('3 downloads selected');
    await page.getByRole('button', { name: 'Resume 2 downloads' }).click();
    await expect(page.getByText('Resumed 2 downloads. 1 selected download was left as it was: Resume does not apply.')).toBeVisible();

    // A ticked row's menu acts on every ticked download.
    await a.click({ button: 'right' });
    let menu = await openMenu(page);
    await expect(menu.getByRole('menuitem', { name: 'Rename…' })).toHaveCount(0);
    await menu.getByRole('menuitem', { name: 'Cancel', exact: true }).click();
    await expect(page.getByText('Cancelled 2 downloads.')).toBeVisible();
    await expect(a.locator('td[data-state]')).toHaveText('Cancelled');
    await expect(b.locator('td[data-state]')).toHaveText('Cancelled');

    // Ticks hidden by the name filter leave the toolbar nothing to act on;
    // it never falls back to the download in the details.
    await page.getByRole('button', { name: 'Clear selection' }).click();
    await b.locator('td.name a').click();
    await expect(page.getByRole('button', { name: 'Retry bulk-b.bin' })).toBeEnabled();
    await a.getByRole('checkbox').check();
    await page.locator('#filter-names').fill('bulk-c');
    await expect(summary).toContainText('1 selected download hidden by the filter is left out.');
    for (const action of ['Pause', 'Resume', 'Cancel', 'Retry']) {
      await expect(page.locator(`.downloads-view [data-action="${action.toLowerCase()}"]`)).toBeDisabled();
    }
    await page.locator('#filter-names').fill('');
    await page.getByRole('button', { name: 'Close details' }).click();

    // An unticked row's menu acts on that row alone.
    await page.getByRole('button', { name: 'Clear selection' }).click();
    await expect(summary).toBeEmpty();
    await c.click({ button: 'right' });
    menu = await openMenu(page);
    await expect(menu.getByRole('menuitem', { name: 'Rename…' })).toBeVisible();
    await page.keyboard.press('Escape');

    await page.getByRole('checkbox', { name: 'Select all downloads shown' }).check();
    await b.getByRole('button', { name: 'Actions for bulk-b.bin' }).click();
    menu = await openMenu(page);
    await menu.getByRole('menuitem', { name: 'Remove…' }).click();
    const dialog = page.getByRole('dialog', { name: 'Remove 3 downloads' });
    await expect(dialog.getByRole('radio', { name: 'Remove from list only (keep files)' })).toBeChecked();
    // Deleting all three at once, in one folder, waits for each other
    // rather than refusing as busy.
    await dialog.getByRole('radio', { name: 'Delete downloaded files/data and remove from list' }).check();
    await dialog.getByRole('button', { name: 'Delete files and remove' }).click();
    await expect(dialog).toHaveCount(0);
    await expect(page.getByText('Deleted downloaded data for 3 downloads and removed them from the list.')).toBeVisible();
    await expect(page.locator('tr[data-job]')).toHaveCount(0);
    await expect(summary).toBeEmpty();
    await expect(own.published('bulk-c.bin')).rejects.toThrow();
  } finally {
    await context.close();
    await own.remove();
  }
});

test('a transferring download shows live progress, speed, and time left', async ({ browser }) => {
  const own = await new Daemon(await freePort()).init();
  await own.start();
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    await page.goto(own.origin);
    await page.getByLabel('Setup secret').fill((await own.setupSecret()).trim());
    await page.getByLabel('Owner name').fill('owner');
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
    await page.getByLabel('Repeat password').fill(PASSWORD);
    await page.getByRole('button', { name: 'Create owner' }).click();
    await expect(page.getByText('No downloads yet.')).toBeVisible();

    // 512 KiB/s makes an 8 MiB file take about 16 seconds.
    await page.goto(`${own.origin}/settings/downloads`);
    const pane = page.locator('.settings-content');
    await pane.getByRole('radio', { name: 'Limit to' }).check();
    await pane.getByRole('spinbutton', { name: 'Total download speed limit' }).fill('512');
    await pane.getByLabel('Speed limit unit').selectOption('KiB');
    await pane.getByRole('button', { name: 'Save' }).click();
    await expect(pane.getByText('Saved.', { exact: true })).toBeVisible();

    await page.goto(own.origin);
    await page.getByRole('button', { name: 'Add links' }).click();
    await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(fixture.put('live.bin', 8 * MIB));
    await page.getByRole('button', { name: 'Collect links' }).click();
    await expect(page.locator('.links-page .selection-summary strong')).toHaveText('1 link selected');
    await page.getByRole('button', { name: 'Start selected' }).click();
    await expect(page.getByText('Started 1 download.')).toBeVisible();
    const live = page.locator('tr[data-job]', { hasText: 'live.bin' });
    const [speed, left] = [live.locator('td.num').nth(1), live.locator('td.num').nth(2)];
    await expect(live.locator('td[data-state]')).toHaveText('Downloading');
    // The first durable checkpoint is the whole file, so a percentage
    // between comes from live samples alone.
    await expect(live.locator('.percent')).toHaveText(/^[1-9]\d?(\.\d)?%$/);
    await expect(speed).toHaveText(/^\d+\.\d KiB\/s$/);
    await expect(left).toHaveText(/^\d+ s$/);
    await expect(page.locator('.statusbar .speed')).toHaveText(/^Down \d+\.\d KiB\/s$/);

    // Paused, it shows neither speed nor time left.
    await live.click({ button: 'right' });
    const menu = await openMenu(page);
    await menu.getByRole('menuitem', { name: 'Pause' }).click();
    await expect(live.locator('td[data-state]')).toHaveText('Paused');
    await expect(speed).toHaveText('');
    await expect(left).toHaveText('');
    await expect(page.locator('.statusbar .speed')).toHaveText('');
  } finally {
    await context.close();
    await own.remove();
  }
});
