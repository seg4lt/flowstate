//! Runtime HTTP download helper shared between `embedded-node` and the
//! provider-SDK bridge crates. Kept tiny on purpose — we only need a
//! "fetch a URL into a file" primitive that works from a blocking
//! context. Async callers bridge via `tokio::task::spawn_blocking`.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use tracing::info;

/// Fetch `url` into `dest` atomically. Writes to `<dest>.partial` first
/// and renames on success so a crash mid-download never leaves a
/// truncated file that a later launch would mistake for a cache hit.
///
/// Prefer [`fetch_verified`] for any download whose bytes we have a
/// pinned digest for. This unverified variant exists for cases where
/// no upstream-published digest is available (none in-tree today —
/// new callers should default to `fetch_verified`).
pub fn fetch(url: &str, dest: &Path) -> Result<()> {
    fetch_inner(url, dest, None)
}

/// Same as [`fetch`], but the response body is rejected unless its
/// SHA-256 digest matches `expected_sha_hex` (case-insensitive hex,
/// 64 chars). On mismatch the partial file is removed and the
/// download is treated as a hard failure — never a silent fallthrough.
pub fn fetch_verified(url: &str, dest: &Path, expected_sha_hex: &str) -> Result<()> {
    fetch_inner(url, dest, Some(expected_sha_hex))
}

fn fetch_inner(url: &str, dest: &Path, expected_sha_hex: Option<&str>) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let started = std::time::Instant::now();
    info!(
        %url,
        dest = %dest.display(),
        verified = expected_sha_hex.is_some(),
        "downloading runtime asset"
    );

    let response = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;

    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body from {url}"))?;

    if let Some(expected_hex) = expected_sha_hex {
        verify_sha256(&bytes, expected_hex).with_context(|| format!("verify SHA-256 of {url}"))?;
    }

    let tmp = dest.with_extension("partial");
    fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, dest)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;

    info!(
        bytes = bytes.len(),
        duration_ms = started.elapsed().as_millis() as u64,
        dest = %dest.display(),
        "runtime asset downloaded"
    );
    Ok(())
}

/// Verify that `bytes` hash to `expected_hex` (lower- or upper-case
/// hex). Returns a descriptive error on mismatch so the caller's
/// `with_context` produces an actionable message — including both the
/// expected and actual digests so a human can copy-paste-verify
/// against the upstream `SHASUMS256.txt`.
pub fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<()> {
    if expected_hex.len() != 64 {
        bail!(
            "expected SHA-256 hex must be 64 chars; got {} chars: {expected_hex:?}",
            expected_hex.len()
        );
    }
    let expected = hex_decode(expected_hex)
        .ok_or_else(|| anyhow!("expected SHA-256 is not valid hex: {expected_hex:?}"))?;

    let actual = Sha256::digest(bytes);
    if actual.as_slice() != expected.as_slice() {
        let actual_hex = hex_encode(actual.as_slice());
        bail!(
            "SHA-256 mismatch — refusing to use the downloaded archive.\n  \
             expected: {expected_hex}\n  \
             actual:   {actual_hex}\n  \
             ({} bytes; the upstream file may have been tampered with, or \
              the pinned checksum in src/node_checksums.rs is stale)",
            bytes.len()
        );
    }
    Ok(())
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known SHA-256 of the empty input, lower- and upper-case. If
    // either representation stops matching we've regressed the
    // hex-decode path — an easy mistake to make since we hand-roll
    // it to avoid pulling in a hex crate.
    const EMPTY_SHA_LOWER: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const EMPTY_SHA_UPPER: &str =
        "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";

    #[test]
    fn verify_sha256_accepts_matching_digest() {
        verify_sha256(b"", EMPTY_SHA_LOWER).expect("empty input hashes to the known value");
        verify_sha256(b"", EMPTY_SHA_UPPER)
            .expect("uppercase hex should match — hex decode is case-insensitive");
    }

    #[test]
    fn verify_sha256_rejects_mismatch() {
        let wrong = "0".repeat(64);
        let err = verify_sha256(b"not empty", &wrong)
            .expect_err("non-empty input must not hash to all-zeroes");
        let msg = format!("{err:#}");
        assert!(msg.contains("mismatch"), "error should name the mismatch: {msg}");
        assert!(msg.contains(&wrong), "error should echo the expected digest: {msg}");
    }

    #[test]
    fn verify_sha256_rejects_wrong_length() {
        let err = verify_sha256(b"", "deadbeef")
            .expect_err("short hex should be rejected before hashing");
        assert!(format!("{err:#}").contains("64 chars"));
    }

    #[test]
    fn verify_sha256_rejects_non_hex_chars() {
        let bad = "z".repeat(64);
        let err = verify_sha256(b"", &bad).expect_err("non-hex chars must fail parse");
        assert!(format!("{err:#}").contains("not valid hex"));
    }
}
