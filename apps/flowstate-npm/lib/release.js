// Release artifact URL helpers.
//
// Asset filenames are kept in lockstep with `.github/workflows/build.yml`'s
// "Stage release assets" step (lines 284–321). Any rename there must be
// mirrored here.

'use strict';

const https = require('node:https');

const REPO = 'seg4lt/flowstate';
const RELEASE_BASE = `https://github.com/${REPO}/releases/download`;
const API_LATEST = `https://api.github.com/repos/${REPO}/releases/latest`;

/**
 * Normalize a user-supplied version string to a `v`-prefixed tag.
 * Accepts: "1.2.3", "v1.2.3", or "latest" (returned unchanged so callers
 * can branch).
 */
function normalizeTag(version) {
  if (!version || version === 'latest') return 'latest';
  return version.startsWith('v') ? version : `v${version}`;
}

function macAssetName(tag) {
  return `Flowstate-${tag}-macos-arm64.dmg`;
}

function windowsAssetName(tag) {
  return `Flowstate-${tag}-windows-x64-setup.exe`;
}

function macUrl(tag) {
  return `${RELEASE_BASE}/${tag}/${macAssetName(tag)}`;
}

function windowsUrl(tag) {
  return `${RELEASE_BASE}/${tag}/${windowsAssetName(tag)}`;
}

/**
 * Resolve "latest" → concrete `v…` tag via the GitHub API.
 * Used only by `flowstate update`. All other paths use the version pinned
 * in the package's own package.json.
 */
function fetchLatestTag() {
  return new Promise((resolve, reject) => {
    const req = https.get(
      API_LATEST,
      {
        headers: {
          // GitHub API requires a UA. Be honest about who's calling so
          // rate-limit reports are diagnosable.
          'User-Agent': '@seg4lt/flowstate-installer',
          Accept: 'application/vnd.github+json',
        },
      },
      (res) => {
        // Don't follow redirects here — /releases/latest serves JSON directly.
        if (res.statusCode !== 200) {
          res.resume();
          reject(
            new Error(
              `GitHub API returned ${res.statusCode} when resolving latest release`,
            ),
          );
          return;
        }
        let body = '';
        res.setEncoding('utf8');
        res.on('data', (chunk) => {
          body += chunk;
        });
        res.on('end', () => {
          try {
            const json = JSON.parse(body);
            if (typeof json.tag_name !== 'string') {
              reject(new Error('GitHub API response missing tag_name'));
              return;
            }
            resolve(json.tag_name);
          } catch (err) {
            reject(err);
          }
        });
      },
    );
    req.on('error', reject);
  });
}

module.exports = {
  REPO,
  normalizeTag,
  macAssetName,
  windowsAssetName,
  macUrl,
  windowsUrl,
  fetchLatestTag,
};
