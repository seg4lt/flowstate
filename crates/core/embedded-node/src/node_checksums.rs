//! Expected SHA-256 digests for the Node.js archives we fetch from
//! `nodejs.org/dist/v<NODE_VERSION>/`. Pinned in source so both the
//! build-time embed path (`build.rs`) and the runtime download path
//! (`src/lib.rs` → `download::fetch_verified`) can reject an archive
//! that doesn't match what a trusted human checked in.
//!
//! Bumping [`super::NODE_VERSION`] requires updating every row here —
//! copy the values straight from
//! `https://nodejs.org/dist/v<version>/SHASUMS256.txt` (the file is
//! GPG-signed; fetch it from a trusted machine and cross-check before
//! committing). CI will fail at build time (embed mode) or first
//! launch (download mode) if a row is stale.
//!
//! This module is `#[path = "src/node_checksums.rs"] mod node_checksums;`
//! from `build.rs` so there's one source of truth for both paths — do
//! NOT add any `use` statements or external-crate references here.

/// `(platform, arch, ext, sha256_hex)` tuples. Match the identifiers
/// that `detect_platform` in `build.rs` / `src/lib.rs` emits, not
/// arbitrary strings — they're compared byte-for-byte.
pub const NODE_CHECKSUMS: &[(&str, &str, &str, &str)] = &[
    // Node.js v20.11.1 — from https://nodejs.org/dist/v20.11.1/SHASUMS256.txt
    (
        "darwin",
        "arm64",
        "tar.gz",
        "e0065c61f340e85106a99c4b54746c5cee09d59b08c5712f67f99e92aa44995d",
    ),
    (
        "darwin",
        "x64",
        "tar.gz",
        "c52e7fb0709dbe63a4cbe08ac8af3479188692937a7bd8e776e0eedfa33bb848",
    ),
    (
        "linux",
        "arm64",
        "tar.gz",
        "e34ab2fc2726b4abd896bcbff0250e9b2da737cbd9d24267518a802ed0606f3b",
    ),
    (
        "linux",
        "x64",
        "tar.gz",
        "bf3a779bef19452da90fb88358ec2c57e0d2f882839b20dc6afc297b6aafc0d7",
    ),
    (
        "win",
        "x64",
        "zip",
        "bc032628d77d206ffa7f133518a6225a9c5d6d9210ead30d67e294ff37044bda",
    ),
];

/// Lookup the pinned SHA-256 for a given (platform, arch, ext) triple.
/// Returns `None` for any combination we don't publish a checksum for
/// — callers should treat that as a hard failure (never fall back to
/// "download unverified") so a typo in platform detection can't
/// silently skip verification.
pub fn expected_sha(platform: &str, arch: &str, ext: &str) -> Option<&'static str> {
    NODE_CHECKSUMS
        .iter()
        .find(|(p, a, e, _)| *p == platform && *a == arch && *e == ext)
        .map(|(_, _, _, sha)| *sha)
}
