import assert from 'node:assert/strict';

import { expect, test } from '@playwright/test';

import { Daemon, Fixture, MIB, freePort } from './harness.js';

test('Downloads and Links keep toolbar widths as the window narrows', async ({ page }) => {
  const daemon = await new Daemon(await freePort()).init();
  try {
    await daemon.start();
    await page.goto(daemon.origin);
    await page.getByLabel('Setup secret').fill((await daemon.setupSecret()).trim());
    await page.getByLabel('Owner name').fill('owner');
    await page.getByLabel('Password', { exact: true }).fill('correct horse battery staple');
    await page.getByLabel('Repeat password').fill('correct horse battery staple');
    await page.getByRole('button', { name: 'Create owner' }).click();
    await expect(page.locator('.statusbar .status')).toHaveText('Live');
    await page.getByRole('button', { name: 'Add links' }).click();
    await page.getByLabel('Links, text with links in it, or a page’s HTML').fill('https://example.test/queued.bin');
    await page.getByRole('button', { name: 'Collect links' }).click();
    await expect(page.locator('.links-page .views .count')).toHaveText('1');
    await page.locator('.links-page [data-key="view-downloads"]').click();
    await page.evaluate(() => document.fonts.ready);

    for (const width of [1366, 1280, 1100, 1000, 900, 721, 720, 640, 360]) {
      await page.setViewportSize({ width, height: 720 });
      const downloads = page.locator('.downloads-view .toolbar');
      const before = await Promise.all([
        downloads.locator('.search').boundingBox(),
        downloads.locator('.tool-group').boundingBox(),
        downloads.locator('.views').boundingBox(),
      ]);
      await downloads.getByRole('link', { name: 'Links (1)', exact: true }).click();
      const links = page.locator('.links-page .toolbar');
      await expect(links).toBeVisible();
      await page.evaluate(() => document.fonts.ready);
      const after = await Promise.all([
        links.locator('.search').boundingBox(),
        links.locator('.tool-group').boundingBox(),
        links.locator('.views').boundingBox(),
      ]);
      for (const [index, box] of before.entries()) {
        const next = after[index];
        assert(box && next);
        for (const dimension of /** @type {const} */ (['x', 'width'])) {
          expect(Math.abs(next[dimension] - box[dimension]), `${width}px toolbar ${index} ${dimension}`).toBeLessThanOrEqual(0.01);
        }
      }
      for (const button of await links.locator('.tool-group button').all()) {
        const box = await button.boundingBox();
        assert(box && after[1]);
        expect(box.x).toBeGreaterThanOrEqual(after[1].x);
        expect(box.x + box.width).toBeLessThanOrEqual(after[1].x + after[1].width);
      }
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
      if (width >= 1100) {
        assert(before[0] && before[1] && after[1]);
        expect(after[0]?.y).toBeCloseTo(before[0].y, 0);
        expect(after[1].height).toBeCloseTo(before[1].height, 0);
        const brand = await links.locator('.brand').boundingBox();
        assert(after[0] && brand);
        expect(after[0].y).toBeLessThan(brand.y + brand.height);
      }
      await links.getByRole('link', { name: 'Downloads', exact: true }).click();
      await expect(downloads).toBeVisible();
    }

    // A list just too long for a short window scrolls inside the table:
    // the pager stays above the status bar, which stays at the bottom.
    await page.getByRole('button', { name: 'Add links' }).click();
    const more = Array.from({ length: 8 }, (_, index) => `https://example.test/more-${index}.bin`);
    await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(more.join('\n'));
    await page.getByRole('button', { name: 'Collect links' }).click();
    await expect(page.locator('.links-page .views .count')).toHaveText('9');
    for (const [width, height] of [[1000, 500], [1000, 720], [360, 720]]) {
      await page.setViewportSize({ width, height });
      const [pager, status] = await Promise.all([
        page.locator('.links-page .pager').boundingBox(),
        page.locator('.statusbar').boundingBox(),
      ]);
      assert(pager && status);
      expect(pager.y + pager.height, `${width}x${height} pager`).toBeLessThanOrEqual(status.y + 0.5);
      if (width > 720) {
        // Wide windows fit the page to the window; narrow ones scroll it.
        expect(await page.evaluate(() => document.documentElement.scrollHeight)).toBe(height);
        expect(status.y + status.height).toBeCloseTo(height, 0);
      }
    }
  } finally {
    await daemon.remove();
  }
});

test('the details panel and the rows keep their place as downloads start, run, and finish', async ({ page }) => {
  const fixture = await new Fixture().start();
  const daemon = await new Daemon(await freePort()).init();
  try {
    await daemon.start();
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto(daemon.origin);
    await page.getByLabel('Setup secret').fill((await daemon.setupSecret()).trim());
    await page.getByLabel('Owner name').fill('owner');
    await page.getByLabel('Password', { exact: true }).fill('correct horse battery staple');
    await page.getByLabel('Repeat password').fill('correct horse battery staple');
    await page.getByRole('button', { name: 'Create owner' }).click();
    await expect(page.getByText('No downloads yet.')).toBeVisible();
    /** @param {string} url */
    const start = async (url) => {
      await page.getByRole('button', { name: 'Add links' }).click();
      await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(url);
      await page.getByRole('button', { name: 'Collect links' }).click();
      await page.getByRole('button', { name: 'Start selected' }).click();
      await expect(page.getByText('Started 1 download.')).toBeVisible();
    };
    const boxes = async () => {
      const found = await Promise.all(
        ['.table-scroll', '.pager', '.inspector'].map((selector) => page.locator(`.downloads-view ${selector}`).boundingBox()),
      );
      return found.map((box) => box && { y: Math.round(box.y), height: Math.round(box.height) });
    };

    await start(fixture.put('first.bin', 4 * MIB, MIB));
    const first = page.locator('tr[data-job]', { hasText: 'first.bin' });
    await expect(first.locator('td[data-state]')).toHaveText('Downloading');
    await first.locator('td.name a').click();
    await expect(page.locator('dd[data-fact="state"]')).toHaveText('Downloading');
    await page.evaluate(() => document.fonts.ready);
    const expected = await boxes();

    // Neither tab, a notice, a new row, nor finishing moves the panel.
    await page.getByRole('tab', { name: 'Engine details' }).click();
    await expect(page.locator('dd[data-fact="job"]')).toBeVisible();
    expect(await boxes()).toEqual(expected);
    await page.getByRole('tab', { name: 'General' }).click();
    await start(fixture.put('second.bin', MIB));
    await expect(page.locator('tr[data-job]')).toHaveCount(2);
    await page.locator('tr[data-job]', { hasText: 'first.bin' }).locator('td.name a').click();
    expect(await boxes()).toEqual(expected);
    fixture.release('first.bin');
    await expect(page.locator('dd[data-fact="state"]')).toHaveText('Done');
    await expect(page.locator('dd[data-fact="speed"]')).toHaveText('—');
    expect(await boxes()).toEqual(expected);
    // Everything in the panel fits its room at this size.
    for (const tab of ['General', 'Engine details']) {
      await page.getByRole('tab', { name: tab }).click();
      const panel = page.locator('.inspector .panel');
      expect(await panel.evaluate((element) => element.scrollHeight <= element.clientHeight), tab).toBe(true);
    }

    // In a short window the panel scrolls, and updates keep its place.
    await page.setViewportSize({ width: 800, height: 500 });
    await page.getByRole('tab', { name: 'General' }).click();
    const panel = page.locator('.inspector .panel');
    await panel.evaluate((element) => {
      element.scrollTop = element.scrollHeight;
    });
    const scrolled = await panel.evaluate((element) => element.scrollTop);
    expect(scrolled).toBeGreaterThan(0);
    await page.locator('tr[data-job]', { hasText: 'second.bin' }).getByRole('checkbox').check();
    await expect(page.locator('.downloads-view .selection-summary strong')).toHaveText('1 download selected');
    expect(await panel.evaluate((element) => element.scrollTop)).toBe(scrolled);
    await page.getByRole('tab', { name: 'Engine details' }).click();
    expect(await panel.evaluate((element) => element.scrollTop)).toBe(0);
  } finally {
    await daemon.remove();
    await fixture.stop();
  }
});
