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
    // Node.js v24.15.0 — from https://nodejs.org/dist/v24.15.0/SHASUMS256.txt
    // Bumped from v20.11.1 because GitHub Copilot CLI requires Node ≥ v24.
    (
        "darwin",
        "arm64",
        "tar.gz",
        "372331b969779ab5d15b949884fc6eaf88d5afe87bde8ba881d6400b9100ffc4",
    ),
    (
        "darwin",
        "x64",
        "tar.gz",
        "ffd5ee293467927f3ee731a553eb88fd1f48cf74eebc2d74a6babe4af228673b",
    ),
    (
        "linux",
        "arm64",
        "tar.gz",
        "73afc234d558c24919875f51c2d1ea002a2ada4ea6f83601a383869fefa64eed",
    ),
    (
        "linux",
        "x64",
        "tar.gz",
        "44836872d9aec49f1e6b52a9a922872db9a2b02d235a616a5681b6a85fec8d89",
    ),
    (
        "win",
        "x64",
        "zip",
        "cc5149eabd53779ce1e7bdc5401643622d0c7e6800ade18928a767e940bb0e62",
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
