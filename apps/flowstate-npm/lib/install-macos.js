// macOS DMG install flow — Node port of apps/flowstate/scripts/install-macos.command.
//
//   1. Download the .dmg
//   2. hdiutil attach (read-only, no Finder window) and parse the plist
//      to find the mount point reliably
//   3. cp -R the .app to the chosen install dir
//   4. xattr -cr to clear the Gatekeeper quarantine flag — the whole
//      reason the script (and now this package) exists, since we ship
//      ad-hoc-signed builds without Apple notarization
//   5. hdiutil detach (best-effort)
//   6. Optionally launch via `open`
//   7. Delete the temp DMG

'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const { download } = require('./download');
const {
  APP_BUNDLE_NAME,
  pickMacInstallDir,
  tempArtifactPath,
} = require('./paths');
const { macAssetName, macUrl } = require('./release');

async function installMac({ tag, launch = true, quiet = false } = {}) {
  if (!tag) throw new Error('installMac: tag is required');

  if (process.platform !== 'darwin') {
    throw new Error(
      `installMac called on non-darwin platform: ${process.platform}`,
    );
  }

  if (process.arch !== 'arm64') {
    // Only the arm64 DMG is published today. Intel Macs run it under
    // Rosetta 2, which is automatic for users who've run any other
    // arm64-only app. Worst case, macOS prompts to install Rosetta the
    // first time the app launches.
    log(
      quiet,
      'note: only an arm64 build is published. On Intel Macs, Rosetta 2 will run it.',
    );
  }

  const dmgUrl = macUrl(tag);
  const dmgPath = tempArtifactPath(macAssetName(tag));

  log(quiet, `==> Downloading ${dmgUrl}`);
  await download(dmgUrl, dmgPath, { quiet });

  let mountPoint;
  try {
    mountPoint = attachDmg(dmgPath);
    log(quiet, `==> Mounted at ${mountPoint}`);

    const appSrc = findAppInMount(mountPoint);
    const { dir: installDir, scope } = pickMacInstallDir();
    const appDest = path.join(installDir, APP_BUNDLE_NAME);

    log(
      quiet,
      `==> Installing to ${installDir}${scope === 'user' ? ' (user scope; /Applications was not writable)' : ''}`,
    );

    if (fs.existsSync(appDest)) {
      log(quiet, '    Removing previous installation');
      fs.rmSync(appDest, { recursive: true, force: true });
    }

    runOrThrow('cp', ['-R', appSrc, appDest]);

    log(quiet, '==> Clearing quarantine attributes');
    runOrThrow('xattr', ['-cr', appDest]);

    if (launch) {
      log(quiet, '==> Launching Flowstate');
      // Don't `runOrThrow` here — `open` can return non-zero in odd
      // edge cases (e.g. Launch Services pending registration) but the
      // app still launches. Treat launch as best-effort so install
      // success is reported even if `open` is fussy.
      spawnSync('open', [appDest], { stdio: quiet ? 'ignore' : 'inherit' });
    }

    return { installedAt: appDest, scope };
  } finally {
    if (mountPoint) {
      // Detach is best-effort. A still-mounted DMG is harmless and the
      // user can `hdiutil detach` it later or just reboot.
      spawnSync('hdiutil', ['detach', mountPoint, '-quiet'], {
        stdio: 'ignore',
      });
    }
    try {
      fs.unlinkSync(dmgPath);
    } catch {}
  }
}

/**
 * Attach the DMG and return the mount point.
 *
 * Using `-plist` instead of parsing the human-readable two-column
 * `hdiutil attach` output: the plist's `system-entities` array always
 * contains the mount point under the `mount-point` key for the
 * filesystem entry, regardless of localized output strings or volume
 * naming.
 */
function attachDmg(dmgPath) {
  const result = spawnSync(
    'hdiutil',
    ['attach', dmgPath, '-nobrowse', '-readonly', '-plist'],
    { encoding: 'utf8' },
  );
  if (result.status !== 0) {
    throw new Error(
      `hdiutil attach failed (exit ${result.status}): ${result.stderr || result.stdout}`,
    );
  }
  // Cheap plist scrape — we only need the mount-point string. Avoids
  // pulling in a plist parser dep. Matches lines like:
  //   <key>mount-point</key>
  //   <string>/Volumes/Flowstate</string>
  const match = result.stdout.match(
    /<key>mount-point<\/key>\s*<string>([^<]+)<\/string>/,
  );
  if (!match) {
    throw new Error('hdiutil attach succeeded but no mount-point in plist');
  }
  return match[1];
}

function findAppInMount(mountPoint) {
  const entries = fs.readdirSync(mountPoint);
  const appName = entries.find(
    (name) => name.toLowerCase() === APP_BUNDLE_NAME.toLowerCase(),
  );
  if (!appName) {
    throw new Error(
      `No ${APP_BUNDLE_NAME} found at mount point ${mountPoint}. Contents: ${entries.join(', ')}`,
    );
  }
  return path.join(mountPoint, appName);
}

function runOrThrow(cmd, args) {
  const result = spawnSync(cmd, args, { stdio: 'inherit' });
  if (result.status !== 0) {
    throw new Error(`${cmd} ${args.join(' ')} exited with ${result.status}`);
  }
}

function log(quiet, msg) {
  if (!quiet) process.stderr.write(`${msg}\n`);
}

module.exports = { installMac };
