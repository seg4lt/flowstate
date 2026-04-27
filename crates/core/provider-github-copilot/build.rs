use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// Copilot bridge build — mirror of provider-claude-sdk/build.rs.
// Keep them in sync when changing staging semantics.

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bridge_dir = PathBuf::from("bridge");

    println!("cargo:rerun-if-changed=bridge/src/index.ts");
    println!("cargo:rerun-if-changed=bridge/package.json");
    println!("cargo:rerun-if-changed=bridge/package-lock.json");
    println!("cargo:rerun-if-changed=bridge/bun.lock");
    println!("cargo:rerun-if-changed=bridge/tsconfig.json");
    // Embedded Node version is part of DEPS_FP (a node major bump can
    // invalidate native modules in `node_modules/`), so re-run when
    // it changes. Path is relative to this crate's manifest dir.
    println!("cargo:rerun-if-changed=../embedded-node/src/lib.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EMBED");

    let embed = env::var("CARGO_FEATURE_EMBED").is_ok();

    if !bridge_dir.join("src/index.ts").exists() {
        write_fingerprints(&out_dir, "0000000000000000", "0000000000000000");
        fs::create_dir_all(out_dir.join("bridge-assets")).ok();
        return;
    }

    // Fail-fast guard: bridge/package-lock.json MUST satisfy
    // bridge/package.json's declared deps. Drift between the two
    // makes `npm ci` refuse to install at end-user first launch — the
    // exact bug that bricked the Copilot bridge in commit history.
    // Validating here means a developer who bumps a dep version but
    // forgets to regenerate the lockfile gets a build-time error
    // pointing at the fix, not a shipped binary that explodes for
    // every user who lazy-hydrates the bridge.
    validate_lockfile_consistency(&bridge_dir, "Copilot");

    let bun_install_stamp = out_dir.join(".bun-install-stamp");
    let pkg_json = bridge_dir.join("package.json");
    let bun_lock = bridge_dir.join("bun.lock");

    if embed && !is_stamp_fresh(&bun_install_stamp, &[&pkg_json, &bun_lock]) {
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
                "cargo:warning=Copilot bridge `bun install` exited with {s}; build may fail"
            ),
            Err(e) => println!(
                "cargo:warning=Failed to invoke `bun install` for copilot bridge ({e}); build may fail"
            ),
        }
    }

    let tsc_status = Command::new("bun")
        .args(["run", "build"])
        .current_dir(&bridge_dir)
        .status();
    match tsc_status {
        Ok(s) if s.success() => {}
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

    let assets_dir = out_dir.join("bridge-assets");
    if assets_dir.exists() {
        fs::remove_dir_all(&assets_dir).expect("failed to clean stale bridge-assets");
    }
    fs::create_dir_all(&assets_dir).expect("failed to create bridge-assets dir");

    let bridge_src = PathBuf::from("bridge/dist/index.js");
    if !bridge_src.exists() {
        println!("cargo:warning=bridge/dist/index.js missing; Copilot bridge will not be embedded");
        write_fingerprints(&out_dir, "0000000000000000", "0000000000000000");
        return;
    }

    fs::copy(&bridge_src, assets_dir.join("index.js")).expect("failed to copy bridge");
    if pkg_json.exists() {
        fs::copy(&pkg_json, assets_dir.join("package.json"))
            .expect("failed to copy bridge package.json");
    }
    if bun_lock.exists() {
        fs::copy(&bun_lock, assets_dir.join("bun.lock")).expect("failed to copy bun.lock");
    }
    let npm_lock = bridge_dir.join("package-lock.json");
    if npm_lock.exists() {
        fs::copy(&npm_lock, assets_dir.join("package-lock.json"))
            .expect("failed to copy package-lock.json");
    }

    if embed {
        let node_modules = PathBuf::from("bridge/node_modules");
        if node_modules.exists() {
            let dest_nm = assets_dir.join("node_modules");
            copy_dir_all(&node_modules, &dest_nm).expect("failed to copy node_modules");
        } else {
            println!(
                "cargo:warning=bridge/node_modules missing; Copilot bridge offline embed will be incomplete"
            );
        }
    }

    // Two-axis fingerprinting: the runtime caches `node_modules/`
    // under a directory named by DEPS_FP, and rewrites the small
    // glue files (index.js + package.json) in place when GLUE_FP
    // changes. This means a code-only edit to `bridge/src/index.ts`
    // no longer triggers a fresh `npm ci` (and a ~100 MB download)
    // on every end user's next launch.
    //
    // GLUE_FP: hash of bridge entry-point bytes + package.json bytes.
    //   These two are tiny and rewritten in place if they change.
    // DEPS_FP: hash of package-lock.json bytes + embedded Node version.
    //   These determine `node_modules/` content; changing them is
    //   the ONLY reason to re-hydrate.
    let glue_fp = fingerprint_files(&[
        &assets_dir.join("index.js"),
        &assets_dir.join("package.json"),
    ]);
    let node_version = read_embedded_node_version();
    let deps_fp = fingerprint_files_with_extra(
        &[&assets_dir.join("package-lock.json")],
        node_version.as_bytes(),
    );
    write_fingerprints(&out_dir, &glue_fp, &deps_fp);
}

/// Hash a fixed list of files by their content (FNV-1a over the file
/// bytes, mixed with the basename so reordering files would still
/// produce a different hash). Missing files contribute as empty so
/// the build degrades gracefully when a stage was skipped.
fn fingerprint_files(paths: &[&Path]) -> String {
    fingerprint_files_with_extra(paths, &[])
}

fn fingerprint_files_with_extra(paths: &[&Path], extra: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for p in paths {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for b in name.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        // Mix a separator so name-vs-content boundaries don't smear
        // (`abc` + `def` should not equal `ab` + `cdef`).
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
        let bytes = fs::read(p).unwrap_or_default();
        for b in &bytes {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xfe;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    for b in extra {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Pull `pub const NODE_VERSION: &str = "<v>";` out of the
/// embedded-node crate's lib.rs. Build-deps would let us depend on
/// the crate cleanly, but that would force the embedded-node crate's
/// own download-build to run as part of OUR build script — too
/// expensive. Reading the source file is cheap and the constant
/// shape is stable.
fn read_embedded_node_version() -> String {
    let lib_rs = "../embedded-node/src/lib.rs";
    let src = match fs::read_to_string(lib_rs) {
        Ok(s) => s,
        Err(e) => {
            println!(
                "cargo:warning=failed to read {lib_rs} ({e}); DEPS_FP will not include node version"
            );
            return String::new();
        }
    };
    let needle = "pub const NODE_VERSION: &str = \"";
    let start = match src.find(needle) {
        Some(i) => i + needle.len(),
        None => {
            println!(
                "cargo:warning=NODE_VERSION constant not found in {lib_rs}; DEPS_FP will not include node version"
            );
            return String::new();
        }
    };
    let end = match src[start..].find('"') {
        Some(i) => start + i,
        None => return String::new(),
    };
    src[start..end].to_string()
}

fn write_fingerprints(out_dir: &Path, glue_fp: &str, deps_fp: &str) {
    // The fingerprint files exist so rustc's dep-info tracks them —
    // when a hash changes, the runtime crate's `include_str!` sees
    // a different byte sequence and the `#[derive(Embed)]` proc-macro
    // re-scans the staged tree.
    let glue_path = out_dir.join("bridge-glue-fingerprint.txt");
    let deps_path = out_dir.join("bridge-deps-fingerprint.txt");
    if fs::read_to_string(&glue_path).ok().as_deref() != Some(glue_fp) {
        fs::write(&glue_path, glue_fp).expect("write bridge-glue-fingerprint.txt");
    }
    if fs::read_to_string(&deps_path).ok().as_deref() != Some(deps_fp) {
        fs::write(&deps_path, deps_fp).expect("write bridge-deps-fingerprint.txt");
    }
    println!("cargo:rustc-env=BRIDGE_GLUE_FINGERPRINT={glue_fp}");
    println!("cargo:rustc-env=BRIDGE_DEPS_FINGERPRINT={deps_fp}");
    println!("cargo:rerun-if-changed={}", glue_path.display());
    println!("cargo:rerun-if-changed={}", deps_path.display());
}

fn is_stamp_fresh(stamp: &Path, sources: &[&Path]) -> bool {
    let stamp_mtime = match fs::metadata(stamp).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    for src in sources {
        match fs::metadata(src).and_then(|m| m.modified()) {
            Ok(src_mtime) if src_mtime > stamp_mtime => return false,
            Err(_) => return false,
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

/// Run the SAME `npm ci` flags that the runtime uses
/// (`bridge_runtime.rs::hydrate_node_modules`), but with `--dry-run`
/// so no `node_modules/` materializes. npm's first pre-flight stage
/// is exactly the lockfile/package.json sync check; if it fails here,
/// it would fail on every end-user's machine on first launch.
///
/// Mirrors the validation in `provider-claude-sdk/build.rs` — keep in
/// sync. Both bridges ship `package-lock.json` to end-user machines and
/// rely on `npm ci` to hydrate `node_modules` on first launch, so a
/// silent lockfile drift bricks the provider for everyone.
///
/// Best-effort: if either file is missing, or `npm` is not on PATH
/// (rare — the bridge build also requires `bun`, so devs reaching this
/// step have a JS toolchain installed), the check skips with a
/// warning rather than failing the build.
fn validate_lockfile_consistency(bridge_dir: &Path, provider: &str) {
    let pkg_path = bridge_dir.join("package.json");
    let lock_path = bridge_dir.join("package-lock.json");
    if !pkg_path.exists() || !lock_path.exists() {
        return;
    }

    // Same flag set as `hydrate_node_modules` so the dry-run's
    // pre-flight matches what end-user `npm ci` will see — no
    // false-negatives from a different flag combination resolving
    // peer-deps differently.
    let output = Command::new("npm")
        .args([
            "ci",
            "--omit=dev",
            "--legacy-peer-deps",
            "--no-audit",
            "--no-fund",
            "--dry-run",
        ])
        .current_dir(bridge_dir)
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            println!(
                "cargo:warning={provider} bridge: skipping lockfile-consistency check ({e}); \
                install npm to enable build-time validation"
            );
            return;
        }
    };

    if output.status.success() {
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Surface the diagnostic prominently in the cargo build log,
    // then panic so the artifact never ships. `cargo:warning=` alone
    // would print but not block the build.
    println!(
        "cargo:warning=========================================================="
    );
    println!(
        "cargo:warning={provider} bridge: package-lock.json is OUT OF SYNC with package.json"
    );
    println!("cargo:warning=`npm ci --dry-run` rejected the lockfile.");
    for line in stderr.lines().chain(stdout.lines()) {
        if line.trim().is_empty() {
            continue;
        }
        println!("cargo:warning=  {line}");
    }
    println!(
        "cargo:warning=Fix: cd {} && npm install --package-lock-only --legacy-peer-deps",
        bridge_dir.display()
    );
    println!(
        "cargo:warning=(Then commit the regenerated package-lock.json. \
        `npm ci` on end-user machines refuses to install when these drift, \
        bricking first launch of the {provider} provider.)"
    );
    println!(
        "cargo:warning=========================================================="
    );
    panic!(
        "{provider} bridge package-lock.json drift detected by `npm ci --dry-run`; \
         see cargo warnings above for the fix"
    );
}
