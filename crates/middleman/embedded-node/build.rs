use std::env;
use std::path::PathBuf;
use std::process::Command;

const NODE_VERSION: &str = "20.11.1";

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();

    let tarball_path = out_dir.join("node.tar.gz");
    if tarball_path.exists() {
        // Incremental rebuilds — the tarball is already there.
        return;
    }

    let (platform, arch) = match detect_platform(&target) {
        Some(p) => p,
        None => {
            // Create an empty marker so `include_bytes!` at least finds
            // a file. The runtime will fail loudly when it tries to use
            // an empty tarball, which is better than a build failure on
            // an unsupported target.
            std::fs::write(&tarball_path, &[] as &[u8])
                .expect("failed to write empty node.tar.gz marker");
            println!(
                "cargo:warning=Unsupported target {}; zenui-embedded-node will be non-functional",
                target
            );
            return;
        }
    };

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

    println!(
        "cargo:warning=Node.js tarball staged at {}",
        tarball_path.display()
    );
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
