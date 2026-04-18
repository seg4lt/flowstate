use std::env;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

const NODE_VERSION: &str = "20.11.1";

fn main() {
    // Only re-run this build script when build.rs itself changes.
    // The Node.js version is hardcoded above, so there are no other
    // inputs that could invalidate the download.
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();

    let (platform, arch) = match detect_platform(&target) {
        Some(p) => p,
        None => {
            // Create empty markers so `include_bytes!` at least finds a
            // file. The runtime will fail loudly when it tries to use an
            // empty archive, which is better than a build failure on an
            // unsupported target.
            fs::write(out_dir.join("node.tar.gz"), &[] as &[u8])
                .expect("failed to write empty node.tar.gz marker");
            fs::write(out_dir.join("node.zip"), &[] as &[u8])
                .expect("failed to write empty node.zip marker");
            println!(
                "cargo:warning=Unsupported target {}; zenui-embedded-node will be non-functional",
                target
            );
            return;
        }
    };

    let is_windows = platform == "win";
    let ext = if is_windows { "zip" } else { "tar.gz" };
    let out_filename = format!("node.{ext}");
    let archive_path = out_dir.join(&out_filename);

    // Also write an empty file for the other format so include_bytes!
    // compiles on every platform (the unused one is just empty bytes).
    let other_filename = if is_windows { "node.tar.gz" } else { "node.zip" };
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

    if cached_archive.exists() {
        // Cache hit — just copy to OUT_DIR for include_bytes!.
        fs::copy(&cached_archive, &archive_path)
            .expect("failed to copy cached node archive to OUT_DIR");
        return;
    }

    // Cache miss — download from nodejs.org using a pure-Rust HTTP
    // client. Avoids the dependency on system `curl` (missing on bare
    // Windows CI runners, inconsistent across Linux containers).
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

    // Persist to cache for future builds / cargo clean cycles.
    fs::create_dir_all(&cache_dir).ok();
    fs::copy(&archive_path, &cached_archive).ok();
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
