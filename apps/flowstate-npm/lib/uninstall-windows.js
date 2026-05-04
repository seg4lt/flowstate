// Windows uninstall — invoke the registered NSIS uninstaller silently.
//
// Reads UninstallString from the Uninstall hive (whichever one our
// install registered into) so we don't have to guess where the
// uninstaller lives. NSIS uninstallers accept `/S` for silent uninstall.

'use strict';

const { spawnSync } = require('node:child_process');
const path = require('node:path');

const { findInstall } = require('./windows-registry');

function uninstallWindows({ quiet = false } = {}) {
  let entry = null;
  try {
    entry = findInstall();
  } catch (err) {
    if (!quiet) {
      process.stderr.write(`Registry lookup failed: ${err.message}\n`);
    }
    return { removed: false };
  }

  if (!entry) {
    if (!quiet) {
      process.stderr.write(
        'No flowstate install found in the registry. Nothing to uninstall.\n',
      );
    }
    return { removed: false };
  }

  // Prefer QuietUninstallString when NSIS supplied one — it already
  // includes the silent flag. Otherwise append `/S` to UninstallString.
  let cmd = entry.quietUninstallString || entry.uninstallString;
  if (!cmd) {
    if (!quiet) {
      process.stderr.write(
        `Registry entry "${entry.displayName}" has no UninstallString. ` +
          'Open "Settings → Apps → Installed apps" and remove "flowstate" from there.\n',
      );
    }
    return { removed: false };
  }

  // UninstallString is typically `"C:\path\with spaces\uninstall.exe"`.
  // Splitting on space would break paths with spaces — use a small
  // helper that respects quoted segments.
  const argv = parseRegistryCmd(cmd);
  const exe = argv.shift();
  const args = argv;
  if (!entry.quietUninstallString && !args.includes('/S')) {
    args.push('/S');
  }

  if (!quiet) {
    process.stderr.write(`==> Running ${exe} ${args.join(' ')}\n`);
  }

  // NSIS occasionally has trouble deleting its own uninstaller from
  // within its own dir; running with cwd one level up avoids that
  // edge case on some Windows builds.
  const cwd = exe ? path.dirname(path.dirname(exe)) : undefined;
  const result = spawnSync(exe, args, { stdio: 'inherit', cwd });
  if (result.status !== 0) {
    throw new Error(`Uninstaller exited with ${result.status}`);
  }
  return { removed: true };
}

/**
 * Split a registry-style command string into argv, respecting double
 * quotes around the executable path. Handles:
 *   `"C:\Program Files\app\unins.exe"`              -> ['C:\\…\\unins.exe']
 *   `"C:\Program Files\app\unins.exe" /MODE=quiet`  -> ['C:\\…\\unins.exe', '/MODE=quiet']
 *   `C:\App\unins.exe /S`                           -> ['C:\\App\\unins.exe', '/S']
 */
function parseRegistryCmd(s) {
  const out = [];
  let i = 0;
  const len = s.length;
  while (i < len) {
    while (i < len && s[i] === ' ') i += 1;
    if (i >= len) break;
    if (s[i] === '"') {
      const end = s.indexOf('"', i + 1);
      if (end === -1) {
        out.push(s.slice(i + 1));
        break;
      }
      out.push(s.slice(i + 1, end));
      i = end + 1;
    } else {
      const end = s.indexOf(' ', i);
      if (end === -1) {
        out.push(s.slice(i));
        break;
      }
      out.push(s.slice(i, end));
      i = end + 1;
    }
  }
  return out;
}

module.exports = { uninstallWindows, parseRegistryCmd };
