#!/usr/bin/env node
// Flowstate npm wrapper — `npx @seg4lt/flowstate` (and friends).
//
// Subcommands:
//   (no args)            install + launch (npx-friendly default)
//   install              install only (use --no-launch to skip launch)
//   launch               open the installed app
//   update               install the latest tag from GitHub Releases
//   uninstall            remove the installed app
//   --version, -v        print this package's version
//   --help, -h           print usage

'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const pkg = require('../package.json');
const { fetchLatestTag, normalizeTag } = require('../lib/release');
const { APP_BUNDLE_NAME } = require('../lib/paths');

async function main() {
  const argv = process.argv.slice(2);
  const flags = parseFlags(argv);

  if (flags.help) {
    printUsage();
    process.exit(0);
  }
  if (flags.version) {
    process.stdout.write(`@seg4lt/flowstate v${pkg.version}\n`);
    process.exit(0);
  }

  const cmd = flags.positional[0] || 'install';

  switch (cmd) {
    case 'install':
      await runInstall({
        version: flags.versionArg,
        launch: flags.launch,
        quiet: flags.quiet,
      });
      break;
    case 'update':
      await runUpdate({ launch: flags.launch, quiet: flags.quiet });
      break;
    case 'launch':
      runLaunch({ quiet: flags.quiet });
      break;
    case 'uninstall':
      runUninstall({ quiet: flags.quiet });
      break;
    default:
      process.stderr.write(`Unknown subcommand: ${cmd}\n\n`);
      printUsage();
      process.exit(2);
  }
}

async function runInstall({ version, launch, quiet }) {
  // Decoupled-version model: this package's own version (pkg.version)
  // tracks the *wrapper*, not the app. When `--version` is omitted, we
  // resolve "latest" against GitHub Releases. To install a specific
  // app version, pass `--version <tag>` explicitly. This means
  // `npx @seg4lt/flowstate` always grabs the current Flowstate even
  // though the npm package itself is republished only when wrapper
  // code changes.
  if (!version) {
    return runUpdate({ launch, quiet });
  }
  const tag = normalizeTag(version);
  if (tag === 'latest') {
    return runUpdate({ launch, quiet });
  }
  await dispatchPlatform(tag, { launch, quiet });
}

async function runUpdate({ launch, quiet }) {
  if (!quiet) process.stderr.write('==> Resolving latest release\n');
  const tag = await fetchLatestTag();
  if (!quiet) process.stderr.write(`    Latest is ${tag}\n`);
  await dispatchPlatform(tag, { launch, quiet });
}

async function dispatchPlatform(tag, opts) {
  if (process.platform === 'darwin') {
    const { installMac } = require('../lib/install-macos');
    await installMac({ tag, ...opts });
    return;
  }
  if (process.platform === 'win32') {
    const { installWindows } = require('../lib/install-windows');
    await installWindows({ tag, ...opts });
    return;
  }
  if (process.platform === 'linux') {
    process.stderr.write(
      'Linux is not currently published. ' +
        'Track https://github.com/seg4lt/flowstate to be notified when it is.\n',
    );
    process.exit(1);
  }
  throw new Error(`Unsupported platform: ${process.platform}`);
}

function runLaunch({ quiet }) {
  if (process.platform === 'darwin') {
    // Try /Applications first, then ~/Applications.
    const candidates = [
      path.join('/Applications', APP_BUNDLE_NAME),
      path.join(require('node:os').homedir(), 'Applications', APP_BUNDLE_NAME),
    ];
    const found = candidates.find((p) => fs.existsSync(p));
    if (!found) {
      process.stderr.write(
        'flowstate.app not found. Run `flowstate install` first.\n',
      );
      process.exit(1);
    }
    spawnSync('open', [found], { stdio: quiet ? 'ignore' : 'inherit' });
    return;
  }
  if (process.platform === 'win32') {
    const { findInstall, resolveMainExe } = require('../lib/windows-registry');
    let entry;
    try {
      entry = findInstall();
    } catch (err) {
      process.stderr.write(`Registry lookup failed: ${err.message}\n`);
      process.exit(1);
    }
    if (!entry) {
      process.stderr.write(
        'flowstate is not installed (no entry in the Uninstall registry hives). ' +
          'Run `flowstate install` first.\n',
      );
      process.exit(1);
    }
    const exe = resolveMainExe(entry);
    if (!exe) {
      process.stderr.write(
        `Found install at ${entry.installLocation || '(unknown)'} but flowstate.exe ` +
          'is missing. Try the Start Menu shortcut.\n',
      );
      process.exit(1);
    }
    const { spawn } = require('node:child_process');
    const child = spawn(exe, [], { detached: true, stdio: 'ignore' });
    child.unref();
    return;
  }
  process.stderr.write(`Cannot launch on ${process.platform}.\n`);
  process.exit(1);
}

function runUninstall({ quiet }) {
  if (process.platform === 'darwin') {
    require('../lib/uninstall-macos').uninstallMac({ quiet });
    return;
  }
  if (process.platform === 'win32') {
    require('../lib/uninstall-windows').uninstallWindows({ quiet });
    return;
  }
  process.stderr.write(`Cannot uninstall on ${process.platform}.\n`);
  process.exit(1);
}

/**
 * Tiny argv parser — no `commander`/`yargs` dep. Handles:
 *   --version <tag>   → versionArg
 *   --no-launch       → launch=false
 *   --quiet           → quiet=true
 *   --help, -h        → help=true
 *   -v, --version (no value, treated as flag if at top level)
 *
 * `--version <tag>` and the package-version flag `--version`/`-v`
 * collide. Resolution: if `--version` is followed by a value AND a
 * subcommand other than the default is present (or `install`/`update`
 * is explicit), treat it as the tag arg. Otherwise treat as the
 * print-package-version flag. This covers:
 *   flowstate install --version 1.2.3   → tag arg
 *   flowstate --version                  → print version
 */
function parseFlags(argv) {
  const out = {
    positional: [],
    versionArg: null,
    launch: true,
    quiet: false,
    help: false,
    version: false,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--help' || a === '-h') {
      out.help = true;
    } else if (a === '--no-launch') {
      out.launch = false;
    } else if (a === '--quiet' || a === '-q') {
      out.quiet = true;
    } else if (a === '--version' || a === '-v') {
      const next = argv[i + 1];
      if (next && !next.startsWith('-')) {
        out.versionArg = next;
        i += 1;
      } else {
        out.version = true;
      }
    } else if (a.startsWith('--version=')) {
      out.versionArg = a.slice('--version='.length);
    } else if (a.startsWith('-')) {
      process.stderr.write(`Unknown flag: ${a}\n`);
      process.exit(2);
    } else {
      out.positional.push(a);
    }
  }
  return out;
}

function printUsage() {
  process.stdout.write(`@seg4lt/flowstate — install Flowstate from the command line

Usage:
  npx @seg4lt/flowstate                   install latest + launch (default)
  flowstate install [--version <tag>]     install a specific release tag
  flowstate update                        install the latest release
  flowstate launch                        open the installed app
  flowstate uninstall                     remove the installed app

Options:
  --no-launch                             don't open the app after install
  --quiet, -q                             suppress progress output
  --version, -v                           print this wrapper's version
  --help, -h                              show this help

The wrapper version (${pkg.version}) is independent of the Flowstate
app version. Without --version, install always pulls the latest
release from github.com/seg4lt/flowstate. To pin, run:

  flowstate install --version 1.2.3
`);
}

main().catch((err) => {
  process.stderr.write(`\nflowstate: ${err.message}\n`);
  process.exit(1);
});
