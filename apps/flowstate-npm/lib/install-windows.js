// Windows install flow — fetch the NSIS setup.exe and run it silently.
//
// NSIS supports `/S` for silent install. The installer is per-user
// (Tauri's default installMode = "currentUser"), so UAC may or may not
// prompt depending on the user's account-control settings; either way
// the result is the same install dir under %LOCALAPPDATA%.

'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');

const { download } = require('./download');
const { defaultWindowsInstallExe, tempArtifactPath } = require('./paths');
const { windowsAssetName, windowsUrl } = require('./release');

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

  const installedExe = defaultWindowsInstallExe();
  if (!fs.existsSync(installedExe)) {
    log(
      quiet,
      `note: expected ${installedExe} after install, but it was not found. ` +
        'Try launching from the Start Menu instead.',
    );
  } else if (launch) {
    log(quiet, '==> Launching Flowstate');
    // detached + unref so we don't keep the npm process alive waiting
    // on a GUI app to exit.
    const { spawn } = require('node:child_process');
    const child = spawn(installedExe, [], { detached: true, stdio: 'ignore' });
    child.unref();
  }

  return { installedAt: installedExe };
}

function log(quiet, msg) {
  if (!quiet) process.stderr.write(`${msg}\n`);
}

module.exports = { installWindows };
