// The bundled UI in a real browser against a real daemon: owner setup,
// downloads of local fixtures, pause, resume, cancel, reloading and closing
// the tab, a daemon restart, and sign-out, from a fresh profile with no
// WAN access.

import { expect, test } from '@playwright/test';

import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

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
 * @param {string} [added]
 */
async function add(page, url, added = 'Added 1 download.') {
  const links = page.getByLabel('Links to download, one per line');
  if (!(await links.isVisible())) await page.getByRole('button', { name: 'Add links' }).click();
  await links.fill(url);
  await page.getByRole('button', { name: 'Add', exact: true }).click();
  await expect(page.getByText(added)).toBeVisible();
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
  await add(page, fixture.put(TRICKY, SIZE, STALL));
  await page.getByRole('button', { name: 'Close', exact: true }).click();
  await expect(page.getByLabel('Links to download, one per line')).toBeHidden();
  await expect(page.getByRole('button', { name: 'Add links' })).toHaveAttribute('aria-expanded', 'false');
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
  await expectFitsWindow(page);
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
  // What the file is, where it came from, and where it is: a server
  // folder, which a browser cannot open.
  await expect(page.locator('dd[data-fact="file"] .path')).toHaveText(TRICKY);
  await expect(page.locator('dd[data-fact="file"] .path')).toHaveAttribute('title', `${daemon.root}/${TRICKY}`);
  await expect(page.locator('dd[data-fact="type"]')).toHaveText('BIN file');
  await expect(page.locator('dd[data-fact="saved-in"] .path')).toHaveText(/** @type {string} */ (daemon.root));
  await expect(page.locator('dd[data-fact="saved-in"]').getByText('Server path')).toBeVisible();
  await expect(page.locator('dd[data-fact="saved-in"] button')).toHaveCount(0);
  await expect(page.locator('dd[data-fact="source"] .path')).toHaveText(
    `${fixture.base}/files/${encodeURIComponent(TRICKY)}`,
  );
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
  // While only cancelled jobs are listed, the job joins the list once cancelled.
  await page.getByRole('button', { name: /^Cancelled \(/ }).click();
  await expect(page.getByText('No downloads in this state.')).toBeVisible();
  await page.getByRole('button', { name: 'Cancel job-2' }).click();
  await expectState(page, 'job-2', 'Cancelled');
  await expect(page.getByRole('button', { name: 'Cancel job-2' })).toBeDisabled();
  await page.getByRole('button', { name: /^All \(/ }).click();
  // The counts follow the change.
  await expect(page.getByRole('button', { name: 'Cancelled (1)' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'All (2)' })).toBeVisible();

  // A long list scrolls inside the table, not the page.
  const many = Array.from({ length: 50 }, (_, i) => fixture.put(`many-${i}.bin`, 1024));
  await add(page, many.join('\n'), 'Added 50 downloads.');
  await expect(page.locator('tr[data-job]')).toHaveCount(50);
  await expectFitsWindow(page);
  const table = page.locator('.table-scroll');
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

  // The add form says where new downloads go: a folder on the server.
  await page.getByRole('button', { name: 'Add links' }).click();
  const destination = page.locator('[data-key="add-destination"]');
  await expect(destination.locator('.path')).toHaveText(/** @type {string} */ (daemon.root));
  await expect(destination.getByText('Server path')).toBeVisible();

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
  await expect(page.locator('table.jobs')).toBeHidden();
  const sections = page.getByRole('navigation', { name: 'Settings sections' });
  const folderLink = sections.getByRole('link', { name: 'Download folder' });
  const displayLink = sections.getByRole('link', { name: 'Display' });
  const accountsLink = sections.getByRole('link', { name: 'Accounts & services' });
  await expect(folderLink).toHaveAttribute('aria-current', 'page');
  await expect(displayLink).not.toHaveAttribute('aria-current');
  const pane = page.locator('.settings-content');
  // The permitted server folders.
  await expect(pane.getByRole('heading', { name: 'Download folder' })).toBeVisible();
  const archive = pane.getByRole('radio', { name: new RegExp(`^${daemon.archive}`) });
  await expect(pane.getByRole('radio', { name: new RegExp(`^${daemon.root}`) })).toBeChecked();
  await expect(pane.getByText('Server path')).toHaveCount(2);
  // Nothing asks for a command line.
  await expect(page.locator('.settings-page code')).toHaveCount(0);
  await expectFitsWindow(page);
  // Row density applies at once.
  await displayLink.click();
  await expect(page).toHaveURL(`${daemon.origin}/settings/display`);
  await expect(displayLink).toHaveAttribute('aria-current', 'page');
  await expect(folderLink).not.toHaveAttribute('aria-current');
  await expect(pane.getByRole('heading', { name: 'Download folder' })).toBeHidden();
  await pane.getByRole('radio', { name: 'Comfortable' }).check();
  expect(await page.evaluate(() => document.documentElement.dataset.density)).toBe('comfortable');
  await pane.getByRole('radio', { name: 'Compact' }).check();
  // From the keyboard: arrows move along the sections, Enter opens one,
  // and focus is drawn.
  await displayLink.focus();
  await page.keyboard.press('ArrowDown');
  await expect(accountsLink).toBeFocused();
  expect(await accountsLink.evaluate((el) => getComputedStyle(el).outlineStyle)).toBe('solid');
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(`${daemon.origin}/settings/accounts`);
  await expect(accountsLink).toHaveAttribute('aria-current', 'page');
  await expect(accountsLink).toBeFocused();
  // Accounts cannot be connected yet.
  const accounts = pane.locator('section', { has: page.getByRole('heading', { name: /Accounts & services/ }) });
  await expect(accounts.getByText('Not available yet', { exact: true })).toBeVisible();
  await expect(accounts.getByText(/such as Real-Debrid/)).toBeVisible();
  await expect(accounts.locator('input, select, textarea, button')).toHaveCount(0);
  await expectFitsWindow(page);
  // The browser's Back goes to the previous section.
  await page.goBack();
  await expect(displayLink).toHaveAttribute('aria-current', 'page');
  // A new default is saved with feedback.
  await folderLink.click();
  await archive.check();
  await pane.getByRole('button', { name: 'Save' }).click();
  await expect(pane.getByText(`Saved. New downloads go to ${daemon.archive}.`)).toBeVisible();
  await expect(pane.getByRole('button', { name: 'Save' })).toBeFocused();
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
  await at.fill(join(shares, 'missing'));
  await at.press('Enter');
  await expect(folders.getByText('The folder does not exist.')).toBeVisible();
  await at.fill(shares);
  await at.press('Enter');
  await expect(folders.getByRole('list', { name: 'Folders inside' }).getByRole('button')).toHaveText(['films']);
  await folders.getByRole('button', { name: 'films' }).click();
  await expect(at).toHaveValue(join(shares, 'films'));
  await expect(folders.getByText('There are no folders inside.')).toBeVisible();
  await folders.getByRole('button', { name: 'Add this folder' }).click();
  await expect(folders).toBeHidden();
  await expect(pane.getByText('Folder added. Save to use it for new downloads.')).toBeVisible();
  await expect(pane.getByRole('radio', { name: new RegExp(`^${join(shares, 'films')}`) })).toBeChecked();
  await expect(pane.getByRole('button', { name: 'Add folder…' })).toBeFocused();
  // Closing without adding changes nothing.
  await pane.getByRole('button', { name: 'Add folder…' }).click();
  await page.keyboard.press('Escape');
  await expect(pane.getByText('No folder was chosen; nothing changed.')).toBeVisible();
  // Only a folder the user added can be removed; its files stay.
  const films = join(shares, 'films');
  await writeFile(join(films, 'kept.bin'), 'kept');
  await expect(pane.getByRole('button', { name: /^Remove / })).toHaveCount(1);
  await pane.getByRole('button', { name: `Remove ${films}` }).click();
  await expect(pane.getByText(`Removed ${films}. Its files stay there;`, { exact: false })).toBeVisible();
  await expect(pane.getByRole('radio', { name: new RegExp(`^${films}`) })).toHaveCount(0);
  await expect(pane.getByRole('radio', { name: new RegExp(`^${daemon.archive}`) })).toBeChecked();
  await expect(pane.getByRole('radio', { name: new RegExp(`^${daemon.archive}`) })).toBeFocused();
  expect(await readFile(join(films, 'kept.bin'), 'utf8')).toBe('kept');
  await pane.getByRole('radio', { name: new RegExp(`^${daemon.archive}`) }).check();
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
  await destination.getByRole('button', { name: 'Change…' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/settings/folder`);
  await expect(folderLink).toHaveAttribute('aria-current', 'page');
  await page.keyboard.press('Escape');
  await expect(page).toHaveURL(`${daemon.origin}/`);
  await expect(destination.getByRole('button', { name: 'Change…' })).toBeFocused();
  await expect(destination.locator('.path')).toHaveText(/** @type {string} */ (daemon.archive));
  // A section opened directly, as after a reload, shows the same page.
  await page.goto(`${daemon.origin}/settings/display`);
  await expect(displayLink).toHaveAttribute('aria-current', 'page');
  await page.getByRole('button', { name: 'Back to Downloads' }).click();
  await expect(page.locator('table.jobs')).toBeVisible();

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
    `${fixture.base}/files/${encodeURIComponent(TRICKY)}`,
  );

  // Items that do not apply say why.
  await row(page, 'job-2').click({ button: 'right' });
  menu = await openMenu(page);
  await expect(menu.getByRole('menuitem', { name: /^Rename/ })).toHaveAttribute('aria-disabled', 'true');
  await expect(menu.getByRole('menuitem', { name: /^Rename/ })).toContainText('Once it has finished');
  await expect(menu.getByRole('menuitem', { name: /^Remove from list/ })).not.toHaveAttribute('aria-disabled');
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
  await expect(menu.getByRole('menuitem', { name: /^Remove from list/ })).toBeFocused();
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
  await menu.getByRole('menuitem', { name: /^Remove from list/ }).click();
  await expect(row(page, job)).toHaveCount(0);
  await expect(page.getByText('Removed renamed.bin from the list. Its file stays in its folder.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'All (53)' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Done (52)' })).toBeVisible();
  expect((await daemon.published('renamed.bin', daemon.archive)).equals(content(0, MIB))).toBe(true);

  // The chosen default outlives a reload.
  await page.reload();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
  await page.getByRole('button', { name: 'Add links' }).click();
  await expect(destination.locator('.path')).toHaveText(/** @type {string} */ (daemon.archive));
  await context.close();
});
