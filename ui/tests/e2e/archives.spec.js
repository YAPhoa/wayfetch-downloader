// Archives unpacked by a stand-in 7-Zip: no request leaves loopback. The
// stand-in answers in 7-Zip's shape: its banner, a technical listing, and
// files written into the folder it is given, with the password read from
// its standard input.

import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { expect, test } from '@playwright/test';

import { Daemon, Fixture, freePort } from './harness.js';

const PASSWORD = 'correct horse battery staple';
const ARCHIVE_PASSWORD = 'open sesame';

/**
 * A stand-in for 7-Zip in `dir`: a shell script, since helpers run
 * confined, where only the system's programs can be run. `secret.zip` is
 * protected by `ARCHIVE_PASSWORD`; any other archive holds `a.txt` and
 * `b.txt`.
 * @param {string} dir
 */
async function fakeSevenZip(dir) {
  const path = join(dir, '7zz');
  await writeFile(
    path,
    String.raw`#!/bin/sh
if [ $# -eq 0 ]; then printf '\n7-Zip (z) 26.04 (x64) : Copyright (c) 1999-2026 Igor Pavlov\n'; exit 0; fi
for arg in "$@"; do last=$arg; done
case "$(basename "$last")" in secret.zip) secret=1 ;; *) secret= ;; esac
entry() { printf 'Path = %s\nFolder = -\nSize = %s\nAttributes =  -rw-r--r--\nEncrypted = %s\n\n' "$@"; }
if [ "$1" = l ]; then
  if [ -n "$secret" ]; then entry plans.txt 6 +; else entry a.txt 5 -; entry b.txt 5 -; fi
  exit 0
fi
out= none=
for arg in "$@"; do case "$arg" in -o*) out=$(expr "x$arg" : 'x-o\(.*\)') ;; -p) none=1 ;; esac; done
if [ -z "$secret" ]; then printf hello > "$out/a.txt"; printf world > "$out/b.txt"; exit 0; fi
line=
[ -n "$none" ] || IFS= read -r line
if [ "$line" != '${ARCHIVE_PASSWORD}' ]; then echo 'ERROR: Wrong password : plans.txt' >&2; exit 2; fi
printf secret > "$out/plans.txt"
`,
  );
  await chmod(path, 0o755);
  return path;
}

test('finished archives unpack beside them, and a protected one asks for its password', async ({ page }) => {
  const fixture = await new Fixture().start();
  const plugins = await mkdtemp(join(tmpdir(), 'wayfetch-plugins-'));
  const daemon = await new Daemon(await freePort(), {
    plugins: { sevenZip: await fakeSevenZip(plugins) },
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

    // Settings shows the plugin, and unpacking after download is on.
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await expect(page.locator('[data-key="extract-on"]')).toBeChecked();
    await page.getByRole('link', { name: 'Plugins' }).click();
    const sevenZip = page.locator('tr[data-plugin="7zip"]');
    await expect(sevenZip.locator('td.version')).toHaveText('26.04');
    await expect(sevenZip.locator('td.plugin-status')).toHaveText('Ready');
    await expect(sevenZip.locator('td.use')).toHaveText('Unpacks downloaded archives');
    await page.getByRole('button', { name: 'Back to Downloads' }).click();

    await page.getByRole('button', { name: 'Add links' }).click();
    await page
      .getByLabel('Links, text with links in it, or a page’s HTML')
      .fill([fixture.put('Photos.zip', 1000), fixture.put('secret.zip', 1000)].join('\n'));
    await page.getByRole('button', { name: 'Collect links' }).click();
    await expect(page).toHaveURL(`${daemon.origin}/links`);
    // Both are shown before all are selected, or the second arrives unselected.
    await expect(page.locator('tr[data-link]')).toHaveCount(2);
    await page.getByRole('checkbox', { name: 'Select all links shown' }).check();
    await page.getByRole('button', { name: 'Start selected' }).click();
    await expect(page.getByText('Started 2 downloads.')).toBeVisible();

    const photos = page.locator('tr[data-job]', { hasText: 'Photos.zip' });
    const secret = page.locator('tr[data-job]', { hasText: 'secret.zip' });
    await expect(photos.locator('td[data-state]')).toHaveText('Unpacked');
    expect(await readFile(join(/** @type {string} */ (daemon.root), 'Photos', 'a.txt'), 'utf8')).toBe('hello');
    expect(await daemon.published('Photos.zip')).toHaveLength(1000);
    await expect(secret.locator('td[data-state]')).toHaveText('Needs password');

    // The details say where it went.
    await photos.locator('td.name a').click();
    await expect(page.locator('dd[data-fact="unpacked-to"]')).toHaveText('Photos');

    // A wrong password is said to be wrong; the right one unpacks it.
    await secret.getByRole('button', { name: /^Actions for / }).click();
    await page.getByRole('menuitem', { name: 'Enter password…' }).click();
    const dialog = page.getByRole('dialog', { name: 'Unpack secret.zip' });
    await dialog.getByLabel('Password').fill('nope');
    await dialog.getByRole('button', { name: 'Unpack', exact: true }).click();
    await expect(dialog).toBeHidden();
    await secret.locator('td.name a').click();
    await expect(page.locator('[data-note="unpack"]')).toHaveText('The password is wrong.');
    await secret.getByRole('button', { name: /^Actions for / }).click();
    await page.getByRole('menuitem', { name: 'Enter password…' }).click();
    await dialog.getByLabel('Password').fill(ARCHIVE_PASSWORD);
    await dialog.getByRole('button', { name: 'Unpack', exact: true }).click();
    await expect(secret.locator('td[data-state]')).toHaveText('Unpacked');
    expect(await readFile(join(/** @type {string} */ (daemon.root), 'plans.txt'), 'utf8')).toBe('secret');
    await expect(page.locator('[data-note="unpack"]')).toHaveCount(0);

    // Unpacking again never replaces the first output.
    await photos.getByRole('button', { name: /^Actions for / }).click();
    await page.getByRole('menuitem', { name: 'Unpack again' }).click();
    await photos.locator('td.name a').click();
    await expect(page.locator('dd[data-fact="unpacked-to"]')).toHaveText('Photos (2)');
    expect(await readFile(join(/** @type {string} */ (daemon.root), 'Photos (2)', 'b.txt'), 'utf8')).toBe('world');

    // Turned off, archives stay packed until asked.
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.locator('[data-key="extract-off"]').check();
    await page.locator('[data-key="downloads-save"]').click();
    await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: 'Back to Downloads' }).click();
    await page.getByRole('button', { name: 'Add links' }).click();
    await page.getByLabel('Links, text with links in it, or a page’s HTML').fill(fixture.put('Later.zip', 1000));
    await page.getByRole('button', { name: 'Collect links' }).click();
    await expect(page.locator('tr[data-link]', { hasText: 'Later.zip' })).toBeVisible();
    await page.getByRole('checkbox', { name: 'Select all links shown' }).check();
    await page.getByRole('button', { name: 'Start selected' }).click();
    const later = page.locator('tr[data-job]', { hasText: 'Later.zip' });
    await expect(later.locator('td[data-state]')).toHaveText('Done');
    await later.getByRole('button', { name: /^Actions for / }).click();
    await page.getByRole('menuitem', { name: 'Unpack', exact: true }).click();
    await expect(later.locator('td[data-state]')).toHaveText('Unpacked');
  } finally {
    await daemon.remove();
    await rm(plugins, { recursive: true, force: true });
    fixture.server.close();
  }
});
