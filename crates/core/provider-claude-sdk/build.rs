use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Install bridge deps and compile its TypeScript. Without this,
    // node_modules/ + dist/index.js don't exist and rust-embed finds
    // an empty staging directory.
    let bridge_dir = PathBuf::from("bridge");
    if bridge_dir.join("src/index.ts").exists() {
        let install_args: &[&str] = if bridge_dir.join("bun.lock").exists() {
            &["install", "--frozen-lockfile"]
        } else {
            &["install"]
        };
        let install = Command::new("bun")
            .args(install_args)
            .current_dir(&bridge_dir)
            .status();
        match install {
            Ok(s) if s.success() => {}
            Ok(s) => println!(
                "cargo:warning=Claude SDK bridge `bun install` exited with {s}; build may fail"
            ),
            Err(e) => println!(
                "cargo:warning=Failed to invoke `bun install` for claude bridge ({e}); \
                build may fail"
            ),
        }

        let tsc_status = Command::new("bun")
            .args(["run", "build"])
            .current_dir(&bridge_dir)
            .status();
        match tsc_status {
            Ok(s) if s.success() => {
                println!("cargo:warning=Claude SDK bridge TS compiled");
            }
            Ok(s) => {
                println!(
                    "cargo:warning=Claude SDK bridge `bun run build` exited with {s}; \
                    using existing dist/index.js if present"
                );
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to invoke `bun run build` for claude bridge ({e}); \
                    using existing dist/index.js if present"
                );
            }
        }
    }

    // Stage the bridge's runtime assets into a dedicated subdir of OUT_DIR
    // so rust-embed can pick them up as a self-contained tree. Anything
    // the bridge needs at runtime goes in here: compiled JS, package
    // manifest, and the node_modules/ directory.
    let assets_dir = out_dir.join("bridge-assets");
    if assets_dir.exists() {
        fs::remove_dir_all(&assets_dir).expect("failed to clear stale bridge-assets dir");
    }
    fs::create_dir_all(&assets_dir).expect("failed to create bridge-assets dir");

    let bridge_src = PathBuf::from("bridge/dist/index.js");
    if !bridge_src.exists() {
        // A blank staging dir means the bridge can't be loaded at
        // runtime; let the warning on the call site in src/lib.rs tell
        // the user. We deliberately don't panic so `cargo check` in a
        // fresh clone (without node_modules) still produces a binary.
        println!(
            "cargo:warning=bridge/dist/index.js missing; Claude SDK bridge will not be embedded"
        );
        return;
    }

    fs::copy(&bridge_src, assets_dir.join("index.js")).expect("failed to copy bridge");

    let pkg_src = PathBuf::from("bridge/package.json");
    if pkg_src.exists() {
        fs::copy(&pkg_src, assets_dir.join("package.json"))
            .expect("failed to copy bridge package.json");
    }

    let node_modules = PathBuf::from("bridge/node_modules");
    if node_modules.exists() {
        println!("cargo:warning=Copying claude-sdk node_modules into bridge-assets...");
        copy_dir_all(&node_modules, &assets_dir.join("node_modules"))
            .expect("failed to copy node_modules");
    }

    println!("cargo:rerun-if-changed=bridge/dist/index.js");
    println!("cargo:rerun-if-changed=bridge/src/index.ts");
    println!("cargo:rerun-if-changed=bridge/package.json");
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
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
