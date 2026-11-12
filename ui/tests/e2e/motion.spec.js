// The transitions, with motion allowed: everything else runs under
// reduced motion (playwright.config.js), where none of this plays.

import { expect, test } from '@playwright/test';

import { Daemon, Fixture, MIB, freePort } from './harness.js';

test.use({ contextOptions: { reducedMotion: 'no-preference' } });

/** @type {Daemon} */
let daemon;
/** @type {Fixture} */
let fixture;

test.beforeEach(async () => {
  fixture = await new Fixture().start();
  daemon = await new Daemon(await freePort()).init();
  await daemon.start();
});

test.afterEach(async () => {
  await daemon?.stop();
  await daemon?.remove();
  await fixture?.stop();
});

/** @param {import('@playwright/test').Page} page */
async function signUp(page) {
  await page.goto(daemon.origin);
  await page.getByLabel('Setup secret').fill((await daemon.setupSecret()).trim());
  await page.getByLabel('Owner name').fill('owner');
  await page.getByLabel('Password', { exact: true }).fill('correct horse battery staple');
  await page.getByLabel('Repeat password').fill('correct horse battery staple');
  await page.getByRole('button', { name: 'Create owner' }).click();
  await expect(page.locator('.statusbar .status')).toHaveText('Live');
}

/**
 * Where `pill` sits once it stops, and the box of what it marks.
 * @param {import('@playwright/test').Locator} pill
 * @param {import('@playwright/test').Locator} target
 */
async function expectPillOn(pill, target) {
  await expect
    .poll(async () => {
      const [a, b] = await Promise.all([pill.boundingBox(), target.boundingBox()]);
      if (!a || !b) return Infinity;
      return Math.max(Math.abs(a.x - b.x), Math.abs(a.y - b.y), Math.abs(a.width - b.width), Math.abs(a.height - b.height));
    })
    .toBeLessThan(1);
}

test('surfaces open and close with their transitions, and pills slide', async ({ page }) => {
  await signUp(page);

  // The Links count pops in, and the view pill follows the current view.
  await page.getByRole('button', { name: 'Add links' }).click();
  await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(fixture.put('slow.bin', 64 * MIB, MIB));
  await page.getByRole('button', { name: 'Collect links' }).click();
  const links = page.locator('.links-page');
  await expect(links.locator('.views .count')).toHaveText('1');
  await expect(links.locator('.views .count')).toHaveClass(/is-open/);
  await expectPillOn(links.locator('.views .t-tabs-pill'), links.locator('[data-key="view-links"]'));

  // A picker menu grows from its button's corner and ends hidden.
  await page.getByRole('button', { name: 'Settings', exact: true }).first().click();
  const picker = page.locator('[data-key="speed-limited"]');
  await picker.check();
  const unit = page.getByRole('button', { name: /^Speed limit unit/ });
  await unit.click();
  const menu = page.getByRole('menu', { name: 'Unit' });
  await expect(menu).toHaveClass(/is-open/);
  await expect(menu).toHaveAttribute('data-origin', /^(top|bottom)-(left|right)$/);
  await page.keyboard.press('Escape');
  await expect(menu).toHaveClass(/is-closing/);
  await expect(menu).toBeHidden();
  await expect(unit).toBeFocused();

  // A number refused when entered shakes.
  const count = page.locator('[data-key="max-active"]');
  await count.fill('0');
  await count.blur();
  await expect(count).toHaveClass(/t-input/);

  // The Settings pill slides to the section chosen.
  const nav = page.locator('.settings-nav');
  await expectPillOn(nav.locator('.t-tabs-pill'), nav.locator('[data-key="settings-section-downloads"]'));
  await page.getByRole('button', { name: 'Cancel' }).click();
  await nav.locator('[data-key="settings-section-folder"]').click();
  await expectPillOn(nav.locator('.t-tabs-pill'), nav.locator('[data-key="settings-section-folder"]'));

  // A dialog scales in, and on Escape plays its close before it goes.
  await page.getByRole('button', { name: 'Add folder…' }).click();
  const dialog = page.getByRole('dialog', { name: 'Add a server folder' });
  await expect(dialog).toHaveClass(/is-open/);
  await page.keyboard.press('Escape');
  await expect(page.locator('dialog.is-closing')).toHaveCount(1);
  await expect(page.locator('dialog')).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Add folder…' })).toBeFocused();
});

test('the details pane opens with its contents easing in and closes at once', async ({ page }) => {
  await signUp(page);
  await page.getByRole('button', { name: 'Add links' }).click();
  await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(fixture.put('stalled.bin', 64 * MIB, MIB));
  await page.getByRole('button', { name: 'Collect links' }).click();
  await page.getByRole('button', { name: 'Start selected' }).click();
  await expect(page).toHaveURL(`${daemon.origin}/`);
  const row = page.locator('.jobs tbody tr').first();
  await row.locator('td.name a').click();
  const pane = page.locator('.inspector');
  await expect(pane).toHaveAttribute('data-open', 'true');
  // The tab underline slides between the tabs.
  const pill = pane.locator('[role="tablist"] .t-tabs-pill');
  await pane.getByRole('tab', { name: 'Engine details' }).click();
  await expect
    .poll(async () => {
      const [bar, tab] = await Promise.all([pill.boundingBox(), pane.getByRole('tab', { name: 'Engine details' }).boundingBox()]);
      return bar && tab ? Math.abs(bar.x - tab.x) + Math.abs(bar.y + bar.height - (tab.y + tab.height)) : Infinity;
    })
    .toBeLessThan(1);
  // Pausing swaps the state mark in place.
  await row.locator('button.more').click();
  await page.getByRole('menuitem', { name: 'Pause' }).click();
  await expect(row.locator('td.state')).toContainText('Paused');
  // Closing leaves at once.
  await pane.getByRole('button', { name: 'Close details' }).click();
  await expect(pane).toBeHidden();
});
