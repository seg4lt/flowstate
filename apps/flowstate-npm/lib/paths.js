// Filesystem path helpers — install destinations, temp file naming, and
// access checks. Cross-platform-aware: callers pick the relevant function.

'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const APP_BUNDLE_NAME = 'flowstate.app';

/**
 * On macOS, prefer /Applications (system-wide). Fall back to
 * ~/Applications when /Applications is not writable by the current user
 * (e.g. non-admin account, or locked-down corporate Mac). Creating
 * ~/Applications if missing is fine — Launchpad indexes it just like the
 * system path.
 */
function pickMacInstallDir() {
  const system = '/Applications';
  if (isWritableDir(system)) return { dir: system, scope: 'system' };

  const userDir = path.join(os.homedir(), 'Applications');
  fs.mkdirSync(userDir, { recursive: true });
  return { dir: userDir, scope: 'user' };
}

function isWritableDir(dir) {
  try {
    fs.accessSync(dir, fs.constants.W_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * Default Tauri NSIS install path. Tauri's default `installMode` is
 * "currentUser", which lands here. If the user customized installMode in
 * tauri.conf.json the path would differ, but Flowstate's config uses the
 * default.
 */
function defaultWindowsInstallExe() {
  const localAppData =
    process.env.LOCALAPPDATA ||
    path.join(os.homedir(), 'AppData', 'Local');
  return path.join(localAppData, 'Programs', 'flowstate', 'flowstate.exe');
}

/**
 * Build a unique-per-tag path under the OS temp dir. Including the tag in
 * the filename lets us keep partial downloads for inspection if a run
 * fails halfway, without overwriting a different version's bytes.
 */
function tempArtifactPath(filename) {
  return path.join(os.tmpdir(), filename);
}

module.exports = {
  APP_BUNDLE_NAME,
  pickMacInstallDir,
  isWritableDir,
  defaultWindowsInstallExe,
  tempArtifactPath,
};
