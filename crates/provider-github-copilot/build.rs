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

    // Compile the bridge TypeScript to JS before copying. Without this,
    // cargo would silently copy the stale dist/index.js compiled from a
    // prior source revision — we've been bitten by that twice now.
    let bridge_dir = PathBuf::from("bridge");
    if bridge_dir.join("src/index.ts").exists() {
        let tsc_status = Command::new("bun")
            .args(["run", "build"])
            .current_dir(&bridge_dir)
            .status();
        match tsc_status {
            Ok(s) if s.success() => {
                println!("cargo:warning=Copilot bridge TS compiled");
            }
            Ok(s) => {
                println!(
                    "cargo:warning=Copilot bridge `bun run build` exited with {s}; \
                    using existing dist/index.js if present"
                );
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to invoke `bun run build` for copilot bridge ({e}); \
                    using existing dist/index.js if present"
                );
            }
        }
    }

    // Copy bridge and node_modules to output
    let bridge_src = PathBuf::from("bridge/dist/index.js");
    let node_modules = PathBuf::from("bridge/node_modules");

    if bridge_src.exists() {
        // Copy bridge
        let bridge_dest = out_dir.join("copilot-bridge.js");
        fs::copy(&bridge_src, &bridge_dest).expect("Failed to copy bridge");

        // Copy package.json
        let pkg_src = PathBuf::from("bridge/package.json");
        if pkg_src.exists() {
            fs::copy(&pkg_src, out_dir.join("package.json")).expect("Failed to copy package.json");
        }

        // Copy node_modules if they exist
        if node_modules.exists() {
            println!("cargo:warning=Copying node_modules (this may take a moment)...");
            copy_dir_all(&node_modules, &out_dir.join("node_modules"))
                .expect("Failed to copy node_modules");
        }

        println!("cargo:rerun-if-changed=bridge/dist/index.js");
        println!("cargo:rerun-if-changed=bridge/src/index.ts");
        println!("cargo:rerun-if-changed=bridge/package.json");
    }
}

fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_all(&path, &dest_path)?;
        } else {
            fs::copy(&path, &dest_path)?;
        }
    }
    Ok(())
}
