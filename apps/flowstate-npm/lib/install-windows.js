// Windows install flow — fetch the NSIS setup.exe and run it silently,
// then locate the installed binary via the Uninstall registry hive
// (NSIS writes our install dir there at install time).
//
// Why registry instead of a hardcoded path: Tauri's NSIS install dir
// has shifted between versions / install modes (`$LOCALAPPDATA\X` vs
// `$LOCALAPPDATA\Programs\X` vs `$PROGRAMFILES\X`), and the user can
// override it. The registry entry is the source of truth that NSIS
// itself wrote — trust it.

'use strict';

const { spawnSync, spawn } = require('node:child_process');
const fs = require('node:fs');

const { download } = require('./download');
const { tempArtifactPath } = require('./paths');
const { windowsAssetName, windowsUrl } = require('./release');
const { findInstall, resolveMainExe } = require('./windows-registry');

async function installWindows({ tag, launch = true, quiet = false } = {}) {
  if (!tag) throw new Error('installWindows: tag is required');

  if (process.platform !== 'win32') {
    throw new Error(
      `installWindows called on non-win32 platform: ${process.platform}`,
    );
  }

  if (process.arch !== 'x64') {
    throw new Error(
      `No ${process.arch} Windows build is published. ` +
        'See https://github.com/seg4lt/flowstate/releases for available downloads.',
    );
  }

  const exeUrl = windowsUrl(tag);
  const exePath = tempArtifactPath(windowsAssetName(tag));

  log(quiet, `==> Downloading ${exeUrl}`);
  await download(exeUrl, exePath, { quiet });

  log(quiet, '==> Running installer (silent)');
  // /S = silent (NSIS convention). Tauri's NSIS template honors it.
  // stdio: 'inherit' so any UAC-related console output / installer
  // errors surface to the user.
  const result = spawnSync(exePath, ['/S'], { stdio: 'inherit' });
  if (result.status !== 0) {
    throw new Error(`Installer exited with ${result.status}`);
  }

  // Best-effort: drop the installer immediately. If this fails (AV scan
  // holding a handle, etc.), it'll get cleaned up by the OS temp sweep.
  try {
    fs.unlinkSync(exePath);
  } catch {}

  // Ask the registry where NSIS actually put the app. This handles
  // currentUser vs perMachine, custom install paths, and any future
  // Tauri install-dir changes without code changes.
  let entry = null;
  try {
    entry = findInstall();
  } catch (err) {
    log(quiet, `note: registry lookup failed: ${err.message}`);
  }

  if (!entry) {
    log(
      quiet,
      'note: install completed but no flowstate registry entry found. ' +
        'The app may still have installed correctly — check the Start Menu.',
    );
    return { installedAt: null };
  }

  const installedExe = resolveMainExe(entry);
  log(quiet, `==> Installed to ${entry.installLocation || '(unknown dir)'}`);

  if (!installedExe) {
    log(
      quiet,
      'note: registry has the install entry but flowstate.exe was not found ' +
        'at the expected location. Launch via the Start Menu shortcut instead.',
    );
    return { installedAt: null, registryEntry: entry };
  }

  if (launch) {
    log(quiet, '==> Launching Flowstate');
    // detached + unref so we don't keep the npm process alive waiting
    // on a GUI app to exit.
    const child = spawn(installedExe, [], {
      detached: true,
      stdio: 'ignore',
    });
    child.unref();
  }

  return { installedAt: installedExe, registryEntry: entry };
}

function log(quiet, msg) {
  if (!quiet) process.stderr.write(`${msg}\n`);
}

module.exports = { installWindows };
