// macOS uninstall — remove flowstate.app from whichever Applications
// dir holds it. Mirrors the install-side fallback (system → user).

'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { APP_BUNDLE_NAME } = require('./paths');

function uninstallMac({ quiet = false } = {}) {
  const candidates = [
    path.join('/Applications', APP_BUNDLE_NAME),
    path.join(os.homedir(), 'Applications', APP_BUNDLE_NAME),
  ];

  let removed = 0;
  for (const target of candidates) {
    if (fs.existsSync(target)) {
      if (!quiet) process.stderr.write(`==> Removing ${target}\n`);
      fs.rmSync(target, { recursive: true, force: true });
      removed += 1;
    }
  }

  if (removed === 0 && !quiet) {
    process.stderr.write(
      `No flowstate.app found in /Applications or ~/Applications. Nothing to do.\n`,
    );
  }
  return { removed };
}

module.exports = { uninstallMac };
