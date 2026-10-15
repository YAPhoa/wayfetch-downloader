import assert from 'node:assert/strict';
import { test } from 'node:test';

import { SECTIONS, sectionPath, settingsSection } from '../../src/components/settings.js';

test('Settings paths name their section; others are not Settings', () => {
  assert.equal(settingsSection('/settings'), 'folder');
  for (const { id } of SECTIONS) assert.equal(settingsSection(sectionPath(id)), id);
  assert.equal(settingsSection('/settings/nonsense'), 'folder');
  assert.equal(settingsSection('/'), null);
  assert.equal(settingsSection('/jobs/job-1'), null);
  assert.equal(settingsSection('/settingsx'), null);
});
