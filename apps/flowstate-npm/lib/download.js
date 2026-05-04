// Streaming HTTPS download with redirect handling and a tiny progress bar.
// No external deps — node:https is enough for the GitHub Releases CDN.

'use strict';

const fs = require('node:fs');
const https = require('node:https');
const { URL } = require('node:url');

const MAX_REDIRECTS = 5;

/**
 * Download `url` to `destPath`. Resolves with `destPath` on success.
 *
 * Follows up to 5 redirects (GitHub Releases serves a 302 to the actual
 * blob CDN). Streams to disk so memory stays flat regardless of artifact
 * size. Renders a single-line percent-progress bar to stderr when stdout
 * is a TTY and `quiet` is false; falls back to a one-shot
 * "Downloading…" / "Done." pair otherwise.
 */
function download(url, destPath, { quiet = false } = {}) {
  return new Promise((resolve, reject) => {
    const tmpPath = `${destPath}.part`;
    const file = fs.createWriteStream(tmpPath);
    let cleanup = () => {
      try {
        file.close();
      } catch {}
      try {
        fs.unlinkSync(tmpPath);
      } catch {}
    };

    let redirects = 0;
    const fetch = (currentUrl) => {
      const req = https.get(
        currentUrl,
        {
          headers: { 'User-Agent': '@seg4lt/flowstate-installer' },
        },
        (res) => {
          // Redirect.
          if (
            res.statusCode &&
            res.statusCode >= 300 &&
            res.statusCode < 400 &&
            res.headers.location
          ) {
            res.resume();
            redirects += 1;
            if (redirects > MAX_REDIRECTS) {
              cleanup();
              reject(new Error(`Too many redirects fetching ${url}`));
              return;
            }
            const next = new URL(res.headers.location, currentUrl).toString();
            fetch(next);
            return;
          }

          if (res.statusCode !== 200) {
            res.resume();
            cleanup();
            reject(
              new Error(
                `Download failed: HTTP ${res.statusCode} for ${currentUrl}`,
              ),
            );
            return;
          }

          const total = Number(res.headers['content-length']) || 0;
          let received = 0;
          let lastPct = -1;
          const isTty = !quiet && process.stderr.isTTY && total > 0;

          if (!quiet && !isTty) {
            process.stderr.write(`Downloading ${url}\n`);
          }

          res.on('data', (chunk) => {
            received += chunk.length;
            if (isTty) {
              const pct = Math.floor((received / total) * 100);
              if (pct !== lastPct) {
                lastPct = pct;
                const mb = (received / 1024 / 1024).toFixed(1);
                const totalMb = (total / 1024 / 1024).toFixed(1);
                process.stderr.write(
                  `\r  ${pct.toString().padStart(3)}%  ${mb} / ${totalMb} MB`,
                );
              }
            }
          });
          res.pipe(file);

          file.on('finish', () => {
            file.close((closeErr) => {
              if (closeErr) {
                cleanup();
                reject(closeErr);
                return;
              }
              if (isTty) process.stderr.write('\n');
              try {
                fs.renameSync(tmpPath, destPath);
              } catch (renameErr) {
                cleanup = () => {};
                reject(renameErr);
                return;
              }
              cleanup = () => {};
              resolve(destPath);
            });
          });

          res.on('error', (err) => {
            cleanup();
            reject(err);
          });
        },
      );

      req.on('error', (err) => {
        cleanup();
        reject(err);
      });
    };

    fetch(url);
  });
}

module.exports = { download };
