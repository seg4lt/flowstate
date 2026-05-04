// Windows registry helper — query the NSIS-registered install entry for
// flowstate so the wrapper never has to hardcode an install directory.
//
// NSIS installers (Tauri's default Windows bundler) write a key under
//   HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\<id>   (currentUser mode)
//   HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\<id>   (perMachine)
//   HKLM\Software\WOW6432Node\...\Uninstall\<id>                    (32-bit on 64-bit)
//
// containing `InstallLocation`, `UninstallString`, `DisplayIcon`, etc.
// The registry key name itself is typically the bundleId
// ("com.seg4lt.flowstate") and DisplayName is sometimes blank, so we
// CAN'T rely on filtering by DisplayName alone — the smart match is
// done Node-side, considering key name + DisplayName + Publisher +
// UninstallString path + DisplayIcon path.
//
// PowerShell is the cleanest way to read the registry from Node.
// Every supported Windows ships PowerShell 5.1+ in the box.

'use strict';

const { spawnSync } = require('node:child_process');

// Returns ALL entries from the three Uninstall hives. Filtering happens
// on the Node side so we have full data to fall back on for diagnosis.
//
// Critical bit: `ConvertTo-Json -InputObject @($all)` (not pipeline form)
// avoids the empty-array → `[[]]` serialization quirk that older versions
// of this code tripped on. With -InputObject + explicit @() coercion,
// 0 entries → `[]`, 1 entry → `[{...}]`, N entries → `[{...},{...}]`.
const PS_SCRIPT = `
$ErrorActionPreference = 'SilentlyContinue'
$hives = @(
  'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall',
  'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall',
  'HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall'
)
$all = @()
foreach ($hive in $hives) {
  if (Test-Path $hive) {
    Get-ChildItem $hive | ForEach-Object {
      $p = Get-ItemProperty $_.PSPath
      if ($p) {
        $all += [pscustomobject]@{
          KeyName              = $_.PSChildName
          DisplayName          = $p.DisplayName
          InstallLocation      = $p.InstallLocation
          UninstallString      = $p.UninstallString
          QuietUninstallString = $p.QuietUninstallString
          DisplayIcon          = $p.DisplayIcon
          Publisher            = $p.Publisher
          Hive                 = $hive
        }
      }
    }
  }
}
ConvertTo-Json -InputObject @($all) -Compress -Depth 4
`;

/**
 * Patterns that count as "this is our install" — case-insensitive.
 * Any one matching across {KeyName, DisplayName, Publisher,
 * UninstallString, DisplayIcon, InstallLocation} is enough.
 */
const FLOWSTATE_PATTERN = /flowstate|seg4lt/i;

/**
 * Read all Uninstall-hive entries. Returns the raw array (possibly empty,
 * never null). Throws on PowerShell invocation failure.
 */
function readAllEntries() {
  if (process.platform !== 'win32') {
    throw new Error(`readAllEntries is win32-only, got ${process.platform}`);
  }

  const result = spawnSync(
    'powershell.exe',
    ['-NoProfile', '-NonInteractive', '-Command', PS_SCRIPT],
    { encoding: 'utf8', windowsHide: true, maxBuffer: 16 * 1024 * 1024 },
  );

  if (result.status !== 0) {
    throw new Error(
      `PowerShell registry query failed (exit ${result.status}): ${
        result.stderr || result.stdout
      }`,
    );
  }

  const stdout = (result.stdout || '').trim();
  if (!stdout) return [];

  let parsed;
  try {
    parsed = JSON.parse(stdout);
  } catch (err) {
    throw new Error(
      `Failed to parse registry query JSON: ${err.message}\nRaw: ${stdout.slice(0, 500)}`,
    );
  }

  if (!Array.isArray(parsed)) parsed = [parsed];
  return parsed.map(normalizeEntry);
}

/**
 * Walk all Uninstall entries and find the first one whose KeyName,
 * DisplayName, Publisher, UninstallString, DisplayIcon, or
 * InstallLocation matches FLOWSTATE_PATTERN. Returns null on no match.
 *
 * Hive order in PS_SCRIPT puts HKCU first (Tauri's default install
 * mode), so the first match is the right one.
 */
function findInstall() {
  const all = readAllEntries();
  for (const entry of all) {
    if (matchesFlowstate(entry)) return entry;
  }
  return null;
}

function matchesFlowstate(entry) {
  if (!entry) return false;
  const fields = [
    entry.keyName,
    entry.displayName,
    entry.publisher,
    entry.uninstallString,
    entry.quietUninstallString,
    entry.displayIcon,
    entry.installLocation,
  ];
  return fields.some((f) => typeof f === 'string' && FLOWSTATE_PATTERN.test(f));
}

function normalizeEntry(raw) {
  if (!raw || typeof raw !== 'object') return null;
  return {
    keyName: nonEmpty(raw.KeyName),
    displayName: nonEmpty(raw.DisplayName),
    installLocation: trimQuotes(raw.InstallLocation),
    uninstallString: trimQuotes(raw.UninstallString),
    quietUninstallString: trimQuotes(raw.QuietUninstallString),
    displayIcon: trimQuotes(raw.DisplayIcon),
    publisher: nonEmpty(raw.Publisher),
    hive: nonEmpty(raw.Hive),
  };
}

/**
 * For diagnosis when findInstall returns null. Returns up to N entries
 * whose key name or any string field hints at flowstate. If nothing
 * looks remotely related, returns the first N entries verbatim so we
 * can at least see what shape the registry has.
 */
function diagnosticDump(limit = 20) {
  let all;
  try {
    all = readAllEntries();
  } catch (err) {
    return { error: err.message, candidates: [] };
  }
  const candidates = all.filter((e) => matchesFlowstate(e));
  if (candidates.length > 0) {
    return { totalEntries: all.length, candidates: candidates.slice(0, limit) };
  }
  // Nothing matched. Surface the first N entries unfiltered so we can
  // see what the registry looks like at all.
  return {
    totalEntries: all.length,
    note:
      'No entry matched flowstate/seg4lt across any field. First entries shown for shape inspection.',
    candidates: all.slice(0, limit),
  };
}

/**
 * Resolve the main flowstate.exe path from a registry entry. Tries, in
 * order:
 *   1. DisplayIcon (NSIS often points this directly at the main exe)
 *   2. <InstallLocation>\flowstate.exe
 *   3. dirname(UninstallString)\flowstate.exe — Tauri's NSIS doesn't
 *      always write InstallLocation, but UninstallString is mandatory
 *      and the uninstaller lives in the install dir.
 *   4. Scan dirname(UninstallString) for any *.exe whose name starts
 *      with "flowstate" (case-insensitive) — last-ditch for renamed
 *      binaries.
 * Returns null if nothing matches or the entry is null.
 */
function resolveMainExe(
  entry,
  fs = require('node:fs'),
  path = require('node:path'),
) {
  if (!entry) return null;

  // 1. DisplayIcon (strip `,N` icon-index suffix).
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
 */
function extractUninstallerPath(uninstallString) {
  if (!uninstallString) return null;
  const s = uninstallString.trim();
  if (s.startsWith('"')) {
    const end = s.indexOf('"', 1);
    if (end === -1) return s.slice(1);
    return s.slice(1, end);
  }
  const space = s.indexOf(' ');
  return space === -1 ? s : s.slice(0, space);
}

function nonEmpty(s) {
  if (typeof s !== 'string') return null;
  const t = s.trim();
  return t === '' ? null : t;
}

function trimQuotes(s) {
  const t = nonEmpty(s);
  if (!t) return null;
  if (t.length >= 2 && t.startsWith('"') && t.endsWith('"')) {
    return t.slice(1, -1);
  }
  return t;
}

module.exports = {
  findInstall,
  resolveMainExe,
  extractUninstallerPath,
  diagnosticDump,
  // Exposed for tests:
  matchesFlowstate,
  normalizeEntry,
};
