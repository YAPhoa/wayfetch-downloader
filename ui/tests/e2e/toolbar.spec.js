import assert from 'node:assert/strict';

import { expect, test } from '@playwright/test';

import { Daemon, freePort } from './harness.js';

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
