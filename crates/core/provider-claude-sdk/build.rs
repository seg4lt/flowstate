use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bridge_dir = PathBuf::from("bridge");

    // Watched sources: re-run when the bridge source, its package.json,
    // or the lockfile changes. NOT bridge/dist/index.js — that's an
    // output of this script and watching it creates an infinite
    // rebuild loop.
    println!("cargo:rerun-if-changed=bridge/src/index.ts");
    println!("cargo:rerun-if-changed=bridge/package.json");
    println!("cargo:rerun-if-changed=bridge/package-lock.json");
    println!("cargo:rerun-if-changed=bridge/bun.lock");
    println!("cargo:rerun-if-changed=bridge/tsconfig.json");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EMBED");

    let embed = env::var("CARGO_FEATURE_EMBED").is_ok();

    if !bridge_dir.join("src/index.ts").exists() {
        // No bridge source: emit empty stubs so the crate still
        // compiles (useful for docs-only builds).
        write_fingerprint(&out_dir, "0000000000000000");
        fs::create_dir_all(out_dir.join("bridge-assets")).ok();
        return;
    }

    // Fail-fast guard — see provider-github-copilot/build.rs for the
    // full rationale. `npm ci` at runtime refuses to install when
    // package.json and package-lock.json disagree on dep versions, so
    // we catch drift at build time before it ships.
    validate_lockfile_consistency(&bridge_dir, "Claude SDK");

    // --- bun install (only needed when embed=ON so `node_modules/` gets
    //     staged; skip entirely in download mode — npm hydrates at
    //     runtime using the lockfile we bake in) ---------------------
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
                "cargo:warning=Claude SDK bridge `bun install` exited with {s}; build may fail"
            ),
            Err(e) => println!(
                "cargo:warning=Failed to invoke `bun install` for claude bridge ({e}); \
                build may fail"
            ),
        }
    }

    // --- tsc (always — the dist output is what we actually ship) -----
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
    if assets_dir.exists() {
        // Clean out any stale staging — otherwise an old `node_modules`
        // from a previous `embed`-on build would linger here and leak
        // into a subsequent default-mode build's embedded tree.
        fs::remove_dir_all(&assets_dir).expect("failed to clean stale bridge-assets");
    }
    fs::create_dir_all(&assets_dir).expect("failed to create bridge-assets dir");

    let bridge_src = PathBuf::from("bridge/dist/index.js");
    if !bridge_src.exists() {
        println!(
            "cargo:warning=bridge/dist/index.js missing; Claude SDK bridge will not be embedded"
        );
        write_fingerprint(&out_dir, "0000000000000000");
        return;
    }

    // Always include the small files: the bridge entry point, its
    // package.json (npm uses it to resolve deps at runtime), and the
    // lockfile (for `npm ci --omit=dev` reproducibility).
    fs::copy(&bridge_src, assets_dir.join("index.js")).expect("failed to copy bridge");
    if pkg_json.exists() {
        fs::copy(&pkg_json, assets_dir.join("package.json"))
            .expect("failed to copy bridge package.json");
    }
    if bun_lock.exists() {
        // Bun's `bun.lock` is bun's own lockfile format. npm doesn't
        // read it, but we ship it so a `bun install` fallback at
        // runtime (future work) stays reproducible. npm uses the
        // separate `package-lock.json` if present — stage it too when
        // the bridge project committed one.
        fs::copy(&bun_lock, assets_dir.join("bun.lock")).expect("failed to copy bun.lock");
    }
    let npm_lock = bridge_dir.join("package-lock.json");
    if npm_lock.exists() {
        fs::copy(&npm_lock, assets_dir.join("package-lock.json"))
            .expect("failed to copy package-lock.json");
    }

    // --- Bundle node_modules only when `embed` feature is on --------
    if embed {
        let node_modules = PathBuf::from("bridge/node_modules");
        if node_modules.exists() {
            let dest_nm = assets_dir.join("node_modules");
            copy_dir_all(&node_modules, &dest_nm).expect("failed to copy node_modules");
        } else {
            println!(
                "cargo:warning=bridge/node_modules missing; Claude SDK bridge offline embed will be incomplete"
            );
        }
    }

    // Fingerprint the staged tree. Naming the cache dir by fingerprint
    // means an upgrade that changes any file (even just the lockfile)
    // forces a fresh `npm install` into a new cache dir rather than
    // overwriting an in-use one.
    let fingerprint = fingerprint_dir(&assets_dir);
    write_fingerprint(&out_dir, &fingerprint);
}

fn write_fingerprint(out_dir: &Path, fingerprint: &str) {
    let fingerprint_path = out_dir.join("bridge-assets-fingerprint.txt");
    let existing = fs::read_to_string(&fingerprint_path).ok();
    if existing.as_deref() != Some(fingerprint) {
        fs::write(&fingerprint_path, fingerprint).expect("write bridge-assets-fingerprint.txt");
    }
    println!("cargo:rustc-env=BRIDGE_FINGERPRINT={fingerprint}");
    println!("cargo:rerun-if-changed={}", fingerprint_path.display());
}

/// FNV-1a over sorted `(relative-path, file-size)` pairs. Matches the
/// runtime hash shape that used to live in `bridge_runtime.rs`, so
/// cache-dir namespaces stay stable across the refactor.
fn fingerprint_dir(root: &Path) -> String {
    let mut parts: Vec<(String, u64)> = Vec::new();
    collect(root, root, &mut parts);
    parts.sort();

    let mut hash: u64 = 0xcbf29ce484222325;
    for (name, size) in &parts {
        for b in name.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        for b in size.to_le_bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else {
            let rel = path
                .strip_prefix(root)
                .expect("collected path is under root")
                .to_string_lossy()
                .replace('\\', "/");
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push((rel, size));
        }
    }
}

/// Returns `true` if `stamp` exists and is newer than every path in `sources`.
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
/// Mirrors the validation in `provider-github-copilot/build.rs` —
/// keep in sync. Both bridges ship `package-lock.json` to end-user
/// machines and rely on `npm ci` to hydrate `node_modules` on first
/// launch, so a silent lockfile drift bricks the provider for everyone.
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
