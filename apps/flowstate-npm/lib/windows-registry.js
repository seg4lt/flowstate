// Windows registry helper — query the NSIS-registered install entry for
// flowstate so the wrapper never has to hardcode an install directory.
//
// NSIS installers (Tauri's default Windows bundler) write a key under
//   HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\<id>   (currentUser mode)
//   HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\<id>   (perMachine)
//   HKLM\Software\WOW6432Node\...\Uninstall\<id>                    (32-bit on 64-bit)
//
// containing `InstallLocation`, `UninstallString`, `DisplayIcon`,
// `DisplayName`, etc. We walk all three hives, filter by DisplayName
// matching /flowstate/i, and return the first hit.
//
// PowerShell is the cleanest way to do this from Node — `reg query`'s
// output is hostile to parse. Every supported Windows ships PowerShell
// 5.1+ in the box, no install needed.

'use strict';

const { spawnSync } = require('node:child_process');

const PS_SCRIPT = `
$ErrorActionPreference = 'SilentlyContinue'
$hives = @(
  'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall',
  'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall',
  'HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall'
)
$out = @()
foreach ($hive in $hives) {
  if (Test-Path $hive) {
    Get-ChildItem $hive | ForEach-Object {
      $p = Get-ItemProperty $_.PSPath
      if ($p -and $p.DisplayName -and ($p.DisplayName -match 'flowstate')) {
        $out += [pscustomobject]@{
          DisplayName     = $p.DisplayName
          InstallLocation = $p.InstallLocation
          UninstallString = $p.UninstallString
          QuietUninstallString = $p.QuietUninstallString
          DisplayIcon     = $p.DisplayIcon
          Publisher       = $p.Publisher
          Hive            = $hive
        }
      }
    }
  }
}
# Always emit a JSON array, even for 0 or 1 results, so Node-side parsing
# is uniform. ConvertTo-Json on a single object yields an object, not an
# array, hence the explicit @() coercion.
,@($out) | ConvertTo-Json -Compress -Depth 4
`;

/**
 * Query the registry for our install entry.
 *
 * Returns the first match (HKCU preferred, since that's Tauri's default
 * install mode), or null if nothing matches. Never throws on "no
 * match" — only on PowerShell invocation failure.
 *
 * Shape:
 *   {
 *     displayName, installLocation, uninstallString,
 *     quietUninstallString, displayIcon, publisher, hive
 *   }
 */
function findInstall() {
  if (process.platform !== 'win32') {
    throw new Error(`findInstall is win32-only, got ${process.platform}`);
  }

  // -NoProfile so user PS profile slowness / errors don't bleed in.
  // -NonInteractive so any prompt would fail-fast instead of hanging.
  const result = spawnSync(
    'powershell.exe',
    ['-NoProfile', '-NonInteractive', '-Command', PS_SCRIPT],
    { encoding: 'utf8', windowsHide: true },
  );

  if (result.status !== 0) {
    throw new Error(
      `PowerShell registry query failed (exit ${result.status}): ${
        result.stderr || result.stdout
      }`,
    );
  }

  const stdout = (result.stdout || '').trim();
  if (!stdout) return null;

  let parsed;
  try {
    parsed = JSON.parse(stdout);
  } catch (err) {
    throw new Error(
      `Failed to parse registry query JSON: ${err.message}\nRaw: ${stdout}`,
    );
  }

  // ConvertTo-Json with `,@($out)` always wraps in an array.
  const entries = Array.isArray(parsed) ? parsed : [parsed];
  if (entries.length === 0) return null;

  const first = entries[0];
  return {
    displayName: first.DisplayName || null,
    installLocation: trimQuotes(first.InstallLocation),
    uninstallString: trimQuotes(first.UninstallString),
    quietUninstallString: trimQuotes(first.QuietUninstallString),
    displayIcon: trimQuotes(first.DisplayIcon),
    publisher: first.Publisher || null,
    hive: first.Hive || null,
  };
}

/**
 * Resolve the main flowstate.exe path from a registry entry. Tries, in
 * order:
 *   1. DisplayIcon (NSIS often points this directly at the main exe)
 *   2. <InstallLocation>\flowstate.exe
 *   3. dirname(UninstallString)\flowstate.exe
 *      — Tauri's NSIS template doesn't always write InstallLocation,
 *      but UninstallString is mandatory and the uninstaller lives
 *      in the install dir.
 *   4. Scan dirname(UninstallString) for any flowstate*.exe (case-
 *      insensitive) as a last-ditch fallback for renamed binaries.
 * Returns null if nothing matches or the entry is null.
 */
function resolveMainExe(
  entry,
  fs = require('node:fs'),
  path = require('node:path'),
) {
  if (!entry) return null;

  // 1. DisplayIcon may include a `,0` icon-index suffix — strip it.
  if (entry.displayIcon) {
    const cleaned = entry.displayIcon.replace(/,\d+$/, '');
    if (cleaned.toLowerCase().endsWith('.exe') && fs.existsSync(cleaned)) {
      return cleaned;
    }
  }

  // 2. InstallLocation when present.
  if (entry.installLocation) {
    const guess = path.join(entry.installLocation, 'flowstate.exe');
    if (fs.existsSync(guess)) return guess;
  }

  // 3. & 4. Derive install dir from the uninstaller path.
  const uninstallerPath = extractUninstallerPath(
    entry.uninstallString || entry.quietUninstallString,
  );
  if (uninstallerPath) {
    const dir = path.dirname(uninstallerPath);

    const guess = path.join(dir, 'flowstate.exe');
    if (fs.existsSync(guess)) return guess;

    // Last-ditch: any flowstate*.exe in the install dir, case-insensitive.
    try {
      const candidates = fs
        .readdirSync(dir)
        .filter(
          (f) =>
            f.toLowerCase().startsWith('flowstate') &&
            f.toLowerCase().endsWith('.exe'),
        );
      if (candidates.length > 0) {
        return path.join(dir, candidates[0]);
      }
    } catch {
      // unreadable dir — fall through to null
    }
  }

  return null;
}

/**
 * Pull the executable path out of an UninstallString. NSIS writes it
 * either bare (`C:\X\unins.exe`) or quoted (`"C:\X\unins.exe" /S`).
 * Returns the unquoted path, or null if the string is empty.
 */
function extractUninstallerPath(uninstallString) {
  if (!uninstallString) return null;
  const s = uninstallString.trim();
  if (s.startsWith('"')) {
    const end = s.indexOf('"', 1);
    if (end === -1) return s.slice(1);
    return s.slice(1, end);
  }
  // Unquoted: take everything up to the first space (NSIS doesn't put
  // unquoted paths-with-spaces in the registry).
  const space = s.indexOf(' ');
  return space === -1 ? s : s.slice(0, space);
}

/**
 * Strip surrounding double-quotes from a registry value. NSIS often
 * writes `UninstallString` as `"C:\Path With Spaces\uninstall.exe"`
 * (quoted) but `InstallLocation` typically isn't quoted. Handle both.
 */
function trimQuotes(s) {
  if (!s || typeof s !== 'string') return null;
  const t = s.trim();
  if (t.length >= 2 && t.startsWith('"') && t.endsWith('"')) {
    return t.slice(1, -1);
  }
  return t;
}

module.exports = { findInstall, resolveMainExe, extractUninstallerPath };
