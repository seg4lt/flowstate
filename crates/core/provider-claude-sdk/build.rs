use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bridge_dir = PathBuf::from("bridge");

    // Register watched sources FIRST, before any early returns, so cargo
    // knows exactly which files trigger a re-run of this build script.
    // Notably we do NOT list bridge/dist/index.js — that is an output of
    // this script (produced by tsc), and watching it creates an infinite
    // rebuild loop.
    println!("cargo:rerun-if-changed=bridge/src/index.ts");
    println!("cargo:rerun-if-changed=bridge/package.json");
    println!("cargo:rerun-if-changed=bridge/bun.lock");
    println!("cargo:rerun-if-changed=bridge/tsconfig.json");

    if !bridge_dir.join("src/index.ts").exists() {
        return;
    }

    // --- bun install (skip when deps haven't changed) ----------------
    let bun_install_stamp = out_dir.join(".bun-install-stamp");
    let pkg_json = bridge_dir.join("package.json");
    let bun_lock = bridge_dir.join("bun.lock");

    if !is_stamp_fresh(&bun_install_stamp, &[&pkg_json, &bun_lock]) {
        let install_args: &[&str] = if bun_lock.exists() {
            &["install", "--frozen-lockfile"]
        } else {
            &["install"]
        };
        let install = Command::new("bun")
            .args(install_args)
            .current_dir(&bridge_dir)
            .status();
        match install {
            Ok(s) if s.success() => {
                touch_stamp(&bun_install_stamp);
            }
            Ok(s) => println!(
                "cargo:warning=Claude SDK bridge `bun install` exited with {s}; build may fail"
            ),
            Err(e) => println!(
                "cargo:warning=Failed to invoke `bun install` for claude bridge ({e}); \
                build may fail"
            ),
        }
    }

    // --- tsc (always runs when build.rs is invoked — it's fast) ------
    let tsc_status = Command::new("bun")
        .args(["run", "build"])
        .current_dir(&bridge_dir)
        .status();
    match tsc_status {
        Ok(s) if s.success() => {}
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

    // --- Stage assets for rust-embed ---------------------------------
    let assets_dir = out_dir.join("bridge-assets");
    fs::create_dir_all(&assets_dir).expect("failed to create bridge-assets dir");

    let bridge_src = PathBuf::from("bridge/dist/index.js");
    if !bridge_src.exists() {
        println!(
            "cargo:warning=bridge/dist/index.js missing; Claude SDK bridge will not be embedded"
        );
        return;
    }

    // Always copy small files (index.js ~few KB, package.json ~200 bytes).
    fs::copy(&bridge_src, assets_dir.join("index.js")).expect("failed to copy bridge");

    if pkg_json.exists() {
        fs::copy(&pkg_json, assets_dir.join("package.json"))
            .expect("failed to copy bridge package.json");
    }

    // Only re-copy node_modules when bun install actually ran (i.e. deps changed).
    let node_modules_stamp = out_dir.join(".node-modules-copy-stamp");
    let node_modules = PathBuf::from("bridge/node_modules");
    if !is_stamp_fresh(&node_modules_stamp, &[&bun_install_stamp]) && node_modules.exists() {
        let dest_nm = assets_dir.join("node_modules");
        if dest_nm.exists() {
            fs::remove_dir_all(&dest_nm).expect("failed to remove stale node_modules from assets");
        }
        copy_dir_all(&node_modules, &dest_nm).expect("failed to copy node_modules");
        touch_stamp(&node_modules_stamp);
    }

    // Content fingerprint of the bridge script. rust-embed's proc macro
    // only re-scans `$OUT_DIR/bridge-assets/` when its containing crate
    // recompiles, which normally only happens when a `.rs` file changes.
    // That meant a fresh `bridge/dist/index.js` could be staged into
    // bridge-assets but the binary would still embed the previous build's
    // bytes (baked in at the last macro expansion). To force a recompile
    // when the bridge content changes, we write a stable hex hash of the
    // dist to `bridge-assets-fingerprint.txt` and have `bridge_runtime.rs`
    // `include_str!` it — rustc tracks include_str! inputs in dep-info,
    // so a fingerprint change invalidates the crate and re-expands
    // `#[derive(Embed)]` with fresh bytes.
    //
    // CRITICAL: we only rewrite the fingerprint file when the value
    // actually changes. tsc updates `dist/index.js`'s mtime on every
    // build even when content is identical; blindly touching the
    // fingerprint here would force an unconditional crate recompile on
    // every cargo invocation.
    let dist_bytes = fs::read(&bridge_src).expect("read bridge dist for fingerprint");
    let fingerprint = fnv1a_hex(&dist_bytes);
    let fingerprint_path = out_dir.join("bridge-assets-fingerprint.txt");
    let existing = fs::read_to_string(&fingerprint_path).ok();
    if existing.as_deref() != Some(fingerprint.as_str()) {
        fs::write(&fingerprint_path, &fingerprint)
            .expect("write bridge-assets-fingerprint.txt");
    }
    println!(
        "cargo:rerun-if-changed={}",
        fingerprint_path.display()
    );
}

/// Inline FNV-1a over a byte slice — same family as `assets_fingerprint`
/// in `bridge_runtime.rs` but over content, not file-name/size tuples.
/// No dep, deterministic, adequate for a "did this file change?" signal.
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Returns `true` if `stamp` exists and is newer than every path in `sources`.
fn is_stamp_fresh(stamp: &Path, sources: &[&Path]) -> bool {
    let stamp_mtime = match fs::metadata(stamp).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false, // stamp missing → stale
    };
    for src in sources {
        match fs::metadata(src).and_then(|m| m.modified()) {
            Ok(src_mtime) if src_mtime > stamp_mtime => return false,
            Err(_) => return false, // source missing → conservative: stale
            _ => {}
        }
    }
    true
}

fn touch_stamp(stamp: &Path) {
    fs::write(stamp, b"ok").expect("failed to write stamp file");
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
