use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    // Get the output directory
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Find the bridge source
    let bridge_src = PathBuf::from("bridge/dist/index.js");

    if bridge_src.exists() {
        // Copy bridge to output directory
        let bridge_dest = out_dir.join("copilot-bridge.js");
        fs::copy(&bridge_src, &bridge_dest).expect("Failed to copy bridge to output directory");

        println!("cargo:rerun-if-changed=bridge/dist/index.js");
        println!("cargo:rerun-if-changed=bridge/src/index.ts");
    } else {
        // Bridge not built yet, warn but don't fail
        println!("cargo:warning=Bridge not found at bridge/dist/index.js - run 'bunx tsc' in bridge/ directory");
    }
}
