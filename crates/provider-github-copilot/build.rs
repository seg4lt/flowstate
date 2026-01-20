use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();

    // Download and embed Node.js binary
    let node_dir = out_dir.join("node");
    fs::create_dir_all(&node_dir).ok();

    let node_bin = node_dir.join("bin/node");

    if !node_bin.exists() {
        println!("cargo:warning=Downloading Node.js for {}...", target);

        // Determine platform
        let (platform, arch) = if target.contains("darwin") {
            if target.contains("aarch64") {
                ("darwin", "arm64")
            } else {
                ("darwin", "x64")
            }
        } else if target.contains("linux") {
            if target.contains("aarch64") {
                ("linux", "arm64")
            } else {
                ("linux", "x64")
            }
        } else {
            println!(
                "cargo:warning=Unsupported target {}, skipping Node.js download",
                target
            );
            return;
        };

        let node_version = "20.11.1";
        let filename = format!("node-v{}-{}-{}.tar.gz", node_version, platform, arch);
        let url = format!("https://nodejs.org/dist/v{}/{}", node_version, filename);

        // Download Node.js
        let status = Command::new("curl")
            .args(&[
                "-L",
                "-o",
                &node_dir.join("node.tar.gz").to_string_lossy(),
                &url,
            ])
            .status();

        if status.is_err() || !status.unwrap().success() {
            println!("cargo:warning=Failed to download Node.js from {}", url);
            return;
        }

        // Extract
        let status = Command::new("tar")
            .args(&["-xzf", "node.tar.gz", "--strip-components=1"])
            .current_dir(&node_dir)
            .status();

        if status.is_err() || !status.unwrap().success() {
            println!("cargo:warning=Failed to extract Node.js");
            return;
        }

        // Clean up
        fs::remove_file(node_dir.join("node.tar.gz")).ok();

        println!("cargo:warning=Node.js downloaded to {:?}", node_bin);
    }

    // Copy bridge and create package.json for ES modules
    let bridge_src = PathBuf::from("bridge/dist/index.js");
    if bridge_src.exists() {
        let bridge_dest = out_dir.join("copilot-bridge.js");
        fs::copy(&bridge_src, &bridge_dest).expect("Failed to copy bridge");

        // Create package.json for ES module support
        let pkg_json = r#"{"type": "module"}"#;
        fs::write(out_dir.join("package.json"), pkg_json).expect("Failed to write package.json");

        println!("cargo:rerun-if-changed=bridge/dist/index.js");
        println!("cargo:rerun-if-changed=bridge/src/index.ts");
    }
}
