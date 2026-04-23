use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

// Pure-data lookup table for the pinned SHA-256 digests. Same file is
// `pub mod node_checksums;`-ed from src/lib.rs at runtime; this
// `#[path]` directive shares the source so the build-time and
// runtime checks can never disagree about what's trusted.
#[path = "src/node_checksums.rs"]
mod node_checksums;

const NODE_VERSION: &str = "20.11.1";

fn main() {
    // Only re-run this build script when build.rs itself changes.
    // The Node.js version is hardcoded above, so there are no other
    // inputs that could invalidate the download.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EMBED");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();

    // Platform detection runs unconditionally — the runtime download path
    // (when `embed` is off) needs the same values and we forward them via
    // `cargo:rustc-env` so the runtime doesn't duplicate the logic.
    let platform_info = detect_platform(&target);
    let (platform, arch) = match platform_info {
        Some(p) => p,
        None => {
            // Emit empty env vars so `env!()` in src/lib.rs still resolves.
            println!("cargo:rustc-env=NODE_TARGET_PLATFORM=");
            println!("cargo:rustc-env=NODE_TARGET_ARCH=");
            println!("cargo:rustc-env=NODE_TARGET_EXT=");
            // When `embed` is on, `include_bytes!` still needs files to
            // resolve to. Write empty stubs so the compile succeeds.
            if feature_enabled("embed") {
                fs::write(out_dir.join("node.tar.gz"), &[] as &[u8])
                    .expect("failed to write empty node.tar.gz marker");
                fs::write(out_dir.join("node.zip"), &[] as &[u8])
                    .expect("failed to write empty node.zip marker");
            }
            println!(
                "cargo:warning=Unsupported target {}; zenui-embedded-node will be non-functional",
                target
            );
            return;
        }
    };

    let is_windows = platform == "win";
    let ext = if is_windows { "zip" } else { "tar.gz" };

    // Forward platform identifiers to the runtime crate. `src/lib.rs`
    // reads these via `env!()` so both the embed and download paths
    // resolve against the same strings baked into the binary.
    println!("cargo:rustc-env=NODE_TARGET_PLATFORM={platform}");
    println!("cargo:rustc-env=NODE_TARGET_ARCH={arch}");
    println!("cargo:rustc-env=NODE_TARGET_EXT={ext}");

    // Feature off → nothing to bake into the binary. The runtime will
    // download on first use.
    if !feature_enabled("embed") {
        return;
    }

    // ------------------------------------------------------------------
    // Embed path: download the Node.js archive at build time and stage
    // it in OUT_DIR so `include_bytes!` in src/lib.rs picks it up.
    // ------------------------------------------------------------------

    let out_filename = format!("node.{ext}");
    let archive_path = out_dir.join(&out_filename);

    // Also write an empty file for the other format so include_bytes!
    // compiles on every platform (the unused one is just empty bytes).
    let other_filename = if is_windows {
        "node.tar.gz"
    } else {
        "node.zip"
    };
    let other_path = out_dir.join(other_filename);
    if !other_path.exists() {
        fs::write(&other_path, &[] as &[u8]).ok();
    }

    if archive_path.exists() {
        // Incremental rebuilds — the archive is already in OUT_DIR.
        return;
    }

    // Use a persistent cache outside of OUT_DIR so the archive survives
    // `cargo clean` and OUT_DIR hash changes. Only download from the
    // network when the persistent cache is also empty.
    let cache_dir = dirs_for_build::cache_dir()
        .join("zenui")
        .join("node-downloads");
    let cache_filename = format!("node-v{}-{}-{}.{}", NODE_VERSION, platform, arch, ext);
    let cached_archive = cache_dir.join(&cache_filename);

    // Look up the pinned digest before touching the network. A stale
    // checksum table is a hard build failure with a clear message —
    // not a silent unverified download.
    let expected_sha = match node_checksums::expected_sha(platform, arch, ext) {
        Some(sha) => sha,
        None => {
            println!(
                "cargo:error=no pinned SHA-256 in src/node_checksums.rs for \
                 {platform}-{arch}.{ext} (Node.js v{NODE_VERSION}); refusing to embed \
                 an unverifiable archive"
            );
            std::process::exit(1);
        }
    };

    if cached_archive.exists() {
        // Cache hit — verify before copying. A previous build that
        // pre-dated checksum-pinning, or a tampered cache, must not be
        // trusted just because a file with the right name exists.
        if let Err(err) = verify_file_sha256(&cached_archive, expected_sha) {
            println!(
                "cargo:warning=cached Node.js archive at {} failed SHA-256 \
                 verification ({err}); deleting and re-downloading",
                cached_archive.display()
            );
            fs::remove_file(&cached_archive).ok();
        } else {
            fs::copy(&cached_archive, &archive_path)
                .expect("failed to copy cached node archive to OUT_DIR");
            return;
        }
    }

    // Cache miss (or evicted by a failed checksum) — download from
    // nodejs.org using a pure-Rust HTTP client. Avoids the dependency
    // on system `curl` (missing on bare Windows CI runners,
    // inconsistent across Linux containers).
    let filename = format!("node-v{}-{}-{}.{}", NODE_VERSION, platform, arch, ext);
    let url = format!("https://nodejs.org/dist/v{}/{}", NODE_VERSION, filename);

    println!(
        "cargo:warning=Downloading Node.js {} for {}-{}...",
        NODE_VERSION, platform, arch
    );

    match download_to(&url, &archive_path) {
        Ok(()) => {}
        Err(err) => {
            // Cargo turns `cargo:error=` lines into a clean build error
            // without dumping a backtrace the way `panic!` does.
            println!("cargo:error=failed to download Node.js from {url}: {err}");
            std::process::exit(1);
        }
    }

    // Verify the freshly-downloaded archive before publishing it to
    // either OUT_DIR (where include_bytes! picks it up) or the
    // persistent cache. A mismatch deletes the partial file and aborts
    // the build with a copy-pasteable error.
    if let Err(err) = verify_file_sha256(&archive_path, expected_sha) {
        fs::remove_file(&archive_path).ok();
        println!(
            "cargo:error=downloaded Node.js archive from {url} failed \
             SHA-256 verification: {err}"
        );
        std::process::exit(1);
    }

    // Persist to cache for future builds / cargo clean cycles.
    fs::create_dir_all(&cache_dir).ok();
    fs::copy(&archive_path, &cached_archive).ok();
}

fn verify_file_sha256(path: &Path, expected_hex: &str) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let actual = Sha256::digest(&bytes);
    let actual_hex = hex_encode(actual.as_slice());
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        return Err(format!(
            "SHA-256 mismatch (expected {expected_hex}, got {actual_hex}, {} bytes)",
            bytes.len()
        ));
    }
    Ok(())
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

fn feature_enabled(name: &str) -> bool {
    let key = format!("CARGO_FEATURE_{}", name.to_uppercase().replace('-', "_"));
    env::var(key).is_ok()
}

fn download_to(url: &str, dest: &std::path::Path) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("request failed: {e}"))?;
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read body failed: {e}"))?;
    fs::write(dest, &bytes).map_err(|e| format!("write {} failed: {e}", dest.display()))?;
    Ok(())
}

fn detect_platform(target: &str) -> Option<(&'static str, &'static str)> {
    if target.contains("darwin") {
        if target.contains("aarch64") {
            Some(("darwin", "arm64"))
        } else {
            Some(("darwin", "x64"))
        }
    } else if target.contains("linux") {
        if target.contains("aarch64") {
            Some(("linux", "arm64"))
        } else {
            Some(("linux", "x64"))
        }
    } else if target.contains("windows") {
        // Node.js uses "win" in its distribution filenames.
        Some(("win", "x64"))
    } else {
        None
    }
}

/// Minimal helper to locate the platform cache directory without pulling
/// in the full `dirs` crate as a build-dependency.
mod dirs_for_build {
    use std::path::PathBuf;

    pub fn cache_dir() -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            if let Ok(home) = std::env::var("HOME") {
                return PathBuf::from(home).join("Library/Caches");
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
                return PathBuf::from(xdg);
            }
            if let Ok(home) = std::env::var("HOME") {
                return PathBuf::from(home).join(".cache");
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Ok(local) = std::env::var("LOCALAPPDATA") {
                return PathBuf::from(local);
            }
        }
        // Fallback: use a temp directory.
        std::env::temp_dir()
    }
}
