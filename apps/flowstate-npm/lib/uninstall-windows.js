// Windows uninstall — invoke the NSIS uninstaller silently.
//
// Tauri's NSIS template drops `uninstall.exe` (the per-app uninstaller)
// next to the installed binary. NSIS uninstallers also accept `/S` for
// silent uninstall. If we can't find the uninstaller at the default
// path, we direct the user to "Add or Remove Programs" rather than
// trying to clean up by hand and risk leaving registry/Start Menu cruft.

'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const { defaultWindowsInstallExe } = require('./paths');

function uninstallWindows({ quiet = false } = {}) {
  const installedExe = defaultWindowsInstallExe();
  const uninstaller = path.join(path.dirname(installedExe), 'uninstall.exe');

  if (!fs.existsSync(uninstaller)) {
    if (!quiet) {
      process.stderr.write(
        `Could not locate ${uninstaller}.\n` +
          'Open "Settings → Apps → Installed apps" and remove "Flowstate" from there.\n',
      );
    }
    return { removed: false };
  }

  if (!quiet) process.stderr.write(`==> Running ${uninstaller} /S\n`);
  // NSIS requires the uninstaller to be invoked from outside its own
  // install dir for some _NSIS-internal-copy_ scenarios. Spawning with
  // `cwd` set to the parent dir avoids "uninstall.exe was unable to
  // delete itself" weirdness on some Windows builds.
  const result = spawnSync(uninstaller, ['/S'], {
    stdio: 'inherit',
    cwd: path.dirname(path.dirname(uninstaller)),
  });
  if (result.status !== 0) {
    throw new Error(`Uninstaller exited with ${result.status}`);
  }
  return { removed: true };
}

module.exports = { uninstallWindows };
