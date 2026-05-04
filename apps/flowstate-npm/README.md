# @seg4lt/flowstate

One-line installer for the [Flowstate](https://github.com/seg4lt/flowstate) desktop app.

```bash
npx @seg4lt/flowstate
```

That's it. The command downloads the latest signed-pinned release, installs it to `/Applications` (macOS) or `%LOCALAPPDATA%\Programs\flowstate` (Windows), and launches the app.

## Why an npm package?

Flowstate's macOS builds are not yet Apple-notarized, so a fresh download is normally blocked by Gatekeeper. The hand-install flow is:

1. Download the DMG
2. Open it
3. Double-click "Install Flowstate.command" (which runs `xattr -cr` to clear the quarantine flag)

This package automates all three steps. On Windows it just runs the NSIS installer with `/S` (silent).

## Install modes

```bash
# One-shot install + launch (always pulls the latest Flowstate release)
npx @seg4lt/flowstate

# Pin to a specific Flowstate release
npx @seg4lt/flowstate install --version 1.2.3

# Or install the CLI globally for repeated use
npm install -g @seg4lt/flowstate
flowstate install                   # latest
flowstate install --version 1.2.3   # pinned
flowstate launch
flowstate update                    # pull the latest release
flowstate uninstall
```

> **Note on versions.** This package's npm version (e.g. `1.0.0`) is the
> *wrapper* version, not the app version. Wrapper code rarely changes;
> the app ships often. `npx @seg4lt/flowstate` always resolves "latest"
> against [github.com/seg4lt/flowstate/releases](https://github.com/seg4lt/flowstate/releases),
> so you don't need to wait for an npm publish after every app release.

## Flags

| Flag           | Effect                                            |
| -------------- | ------------------------------------------------- |
| `--version <tag>` | Install a specific release tag (e.g. `1.2.3`)  |
| `--no-launch`  | Don't auto-launch the app after install           |
| `--quiet`, `-q`| Suppress progress output                          |
| `--help`, `-h` | Print usage                                       |

## Platform support

| Platform        | Status                          |
| --------------- | ------------------------------- |
| macOS arm64     | Supported                       |
| macOS x64       | Runs arm64 build via Rosetta 2  |
| Windows x64     | Supported                       |
| Windows arm64   | Not yet published               |
| Linux           | Not yet published               |

## Trust model

The package is a thin Node-only wrapper (zero runtime deps, ~10 KB unpacked). It downloads the official release artifact from `github.com/seg4lt/flowstate/releases` over HTTPS. There is no separate signature check inside this wrapper — verify the package author (`@seg4lt`) on npmjs.com, or download the artifact directly from GitHub Releases if you prefer to verify Tauri's minisign signature manually.

## Maintainer notes

The wrapper is decoupled from the app's release cadence. To publish a wrapper update:

1. Edit code in `apps/flowstate-npm/`.
2. Bump `version` in `apps/flowstate-npm/package.json` (semver).
3. Merge to `main`.
4. Open the **Actions → Publish npm wrapper** workflow and click **Run workflow** (or pass `dry-run: true` first to confirm). The job aborts if the local version already matches what's on npm.

`v*` tag pushes do **not** publish — they only build and release the desktop app. The npm package always resolves the latest release at install time, so a tagged app release ships to npx users with zero extra steps.

The asset filename templates in `lib/release.js` must stay in lockstep with the "Stage release assets" step of `.github/workflows/build.yml`. If you rename a release asset, update both.
