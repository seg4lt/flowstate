use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const NODE_VERSION: &str = "20.11.1";

fn main() {
    // Only re-run this build script when build.rs itself changes.
    // The Node.js version is hardcoded above, so there are no other
    // inputs that could invalidate the download.
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();

    let tarball_path = out_dir.join("node.tar.gz");
    if tarball_path.exists() {
        // Incremental rebuilds — the tarball is already in OUT_DIR.
        return;
    }

    let (platform, arch) = match detect_platform(&target) {
        Some(p) => p,
        None => {
            // Create an empty marker so `include_bytes!` at least finds
            // a file. The runtime will fail loudly when it tries to use
            // an empty tarball, which is better than a build failure on
            // an unsupported target.
            fs::write(&tarball_path, &[] as &[u8])
                .expect("failed to write empty node.tar.gz marker");
            println!(
                "cargo:warning=Unsupported target {}; zenui-embedded-node will be non-functional",
                target
            );
            return;
        }
    };

    // Use a persistent cache outside of OUT_DIR so the tarball survives
    // `cargo clean` and OUT_DIR hash changes. Only download from the
    // network when the persistent cache is also empty.
    let cache_dir = dirs_for_build::cache_dir()
        .join("zenui")
        .join("node-downloads");
    let cache_filename = format!("node-v{}-{}-{}.tar.gz", NODE_VERSION, platform, arch);
    let cached_tarball = cache_dir.join(&cache_filename);

    if cached_tarball.exists() {
        // Cache hit — just copy to OUT_DIR for include_bytes!.
        fs::copy(&cached_tarball, &tarball_path).expect("failed to copy cached node tarball to OUT_DIR");
        return;
    }

    // Cache miss — download from nodejs.org.
    let filename = format!("node-v{}-{}-{}.tar.gz", NODE_VERSION, platform, arch);
    let url = format!("https://nodejs.org/dist/v{}/{}", NODE_VERSION, filename);

    println!(
        "cargo:warning=Downloading Node.js {} for {}-{}...",
        NODE_VERSION, platform, arch
    );

    let status = Command::new("curl")
        .args([
            "-fsSL",
            "-o",
            &tarball_path.to_string_lossy(),
            &url,
        ])
        .status()
        .expect("failed to invoke curl");
    if !status.success() {
        panic!("failed to download Node.js from {url}");
    }

    // Persist to cache for future builds / cargo clean cycles.
    fs::create_dir_all(&cache_dir).ok();
    fs::copy(&tarball_path, &cached_tarball).ok();
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
