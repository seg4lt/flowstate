use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// Copilot bridge build — mirror of provider-claude-sdk/build.rs.
// Keep them in sync when changing staging semantics.

/// Cross-platform program name for pnpm. Mirrors the helper in
/// provider-claude-sdk/build.rs — keep in sync. On Windows pnpm is
/// installed as `pnpm.cmd`, and Rust's `Command::new` doesn't
/// auto-resolve `.cmd` shims through PATHEXT, so a bare "pnpm" panics
/// with "program not found" even when pnpm is on PATH.
fn pnpm_program() -> &'static str {
    if cfg!(windows) { "pnpm.cmd" } else { "pnpm" }
}

/// Same Windows-shim caveat as `pnpm_program`.
fn npm_program() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bridge_dir = PathBuf::from("bridge");

    println!("cargo:rerun-if-changed=bridge/src/index.ts");
    println!("cargo:rerun-if-changed=bridge/package.json");
    println!("cargo:rerun-if-changed=bridge/package-lock.json");
    println!("cargo:rerun-if-changed=bridge/pnpm-lock.yaml");
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

    // -----------------------------------------------------------------
    // Fast-path: skip the entire pnpm/tsc pipeline when nothing has
    // changed. Mirror of provider-claude-sdk/build.rs — see that file
    // for the full rationale. Short version: invoking pnpm rewrites
    // `bridge/node_modules/.package-lock.json`, which under
    // `tauri dev` triggers a watcher rebuild, which reruns this build
    // script, which reinvokes pnpm, ad infinitum. Skipping pnpm
    // entirely when the staged assets are already fresh breaks the
    // loop.
    let assets_dir = out_dir.join("bridge-assets");
    let staged_index = assets_dir.join("index.js");
    let staged_pkg = assets_dir.join("package.json");
    let glue_fp_path = out_dir.join("bridge-glue-fingerprint.txt");
    let deps_fp_path = out_dir.join("bridge-deps-fingerprint.txt");

    let inputs: Vec<PathBuf> = vec![
        bridge_dir.join("src/index.ts"),
        bridge_dir.join("tsconfig.json"),
        bridge_dir.join("package.json"),
        bridge_dir.join("package-lock.json"),
        bridge_dir.join("pnpm-lock.yaml"),
        PathBuf::from("../embedded-node/src/lib.rs"),
    ];
    let outputs: Vec<&Path> = vec![
        staged_index.as_path(),
        staged_pkg.as_path(),
        glue_fp_path.as_path(),
        deps_fp_path.as_path(),
    ];

    if assets_fresh(&inputs, &outputs) {
        let glue_fp = fs::read_to_string(&glue_fp_path).unwrap_or_default();
        let deps_fp = fs::read_to_string(&deps_fp_path).unwrap_or_default();
        println!("cargo:rustc-env=BRIDGE_GLUE_FINGERPRINT={glue_fp}");
        println!("cargo:rustc-env=BRIDGE_DEPS_FINGERPRINT={deps_fp}");
        println!("cargo:rerun-if-changed={}", glue_fp_path.display());
        println!("cargo:rerun-if-changed={}", deps_fp_path.display());
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
    //
    // Gated by a stamp — see provider-claude-sdk/build.rs for the
    // full rationale. Short version: `npm ci --dry-run` re-syncs
    // `bridge/node_modules/.package-lock.json` as a side effect, which
    // under `tauri dev` triggers an infinite watcher rebuild loop.
    // Skipping the check when lockfiles haven't changed avoids the
    // rewrite without weakening the guarantee.
    let validation_stamp = out_dir.join(".lockfile-validation-stamp");
    let pkg_json_for_stamp = bridge_dir.join("package.json");
    let pkg_lock_for_stamp = bridge_dir.join("package-lock.json");
    if !is_stamp_fresh(
        &validation_stamp,
        &[&pkg_json_for_stamp, &pkg_lock_for_stamp],
    ) {
        validate_lockfile_consistency(&bridge_dir, "Copilot");
        touch_stamp(&validation_stamp);
    }

    // pnpm preflight — see provider-claude-sdk/build.rs::preflight_pnpm
    // for the full rationale. Failing the build now turns the
    // "bridge assets are empty" runtime mystery into a pointed
    // build-time error.
    preflight_pnpm("Copilot");

    // pnpm via mise / corepack is the dev-time package manager. bun
    // intentionally not used: extra cross-platform install with no
    // distinguishing benefit, and not having it on PATH used to
    // silently brick the bridge build. See provider-claude-sdk's
    // build.rs for the matching rationale, including the reason this
    // install runs unconditionally (the build step below needs
    // `node_modules/.bin/tsc`, so non-embed builds were broken when
    // the install was gated on `embed`).
    let pnpm_install_stamp = out_dir.join(".pnpm-install-stamp");
    let pkg_json = bridge_dir.join("package.json");
    let pnpm_lock = bridge_dir.join("pnpm-lock.yaml");
    let npm_lock = bridge_dir.join("package-lock.json");

    if !is_stamp_fresh(
        &pnpm_install_stamp,
        &[&pkg_json, &pnpm_lock, &npm_lock],
    ) {
        let install_args: &[&str] = if pnpm_lock.exists() {
            &["install", "--frozen-lockfile"]
        } else {
            &["install"]
        };
        // See provider-claude-sdk/build.rs for rationale — force
        // dev install so devDependencies (typescript) actually land
        // in node_modules/.bin/.
        // See provider-claude-sdk/build.rs for the env-clear
        // rationale.
        let install_path =
            env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());
        let install_home = env::var("HOME").unwrap_or_default();
        let install = {
            let mut cmd = Command::new(pnpm_program());
            cmd.env_clear()
                .env("PATH", &install_path)
                .env("HOME", &install_home)
                .env("NODE_ENV", "development")
                // Cargo build scripts have no TTY. Without `CI=true`,
                // pnpm aborts whenever it wants to purge `node_modules/`
                // (`ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY`), which
                // is exactly the path build scripts hit after the
                // workspace's lockfile or member set changes. Force CI
                // mode so pnpm proceeds non-interactively.
                .env("CI", "true")
                .env(
                    "MISE_DATA_DIR",
                    env::var("MISE_DATA_DIR").unwrap_or_default(),
                )
                .env("LANG", env::var("LANG").unwrap_or_default());
            // Windows-essential env vars stripped by `env_clear` —
            // see provider-claude-sdk/build.rs for full rationale.
            // Without TEMP/TMP, node's `os.tmpdir()` returns
            // `undefined`, and pnpm's `temp-dir@2.0.0` dep crashes
            // calling `realpathSync(undefined)` on Windows CI runners.
            // Unconditional copy is a no-op on Unix.
            for var in [
                "TEMP",
                "TMP",
                "USERPROFILE",
                "APPDATA",
                "LOCALAPPDATA",
                "SYSTEMROOT",
                "PATHEXT",
                "COMSPEC",
            ] {
                if let Ok(val) = env::var(var) {
                    cmd.env(var, val);
                }
            }
            cmd.args(install_args)
                .arg("--prod=false")
                .current_dir(&bridge_dir)
                .status()
        };
        match install {
            Ok(s) if s.success() => {
                touch_stamp(&pnpm_install_stamp);
            }
            Ok(s) => panic!(
                "Copilot bridge `pnpm install` exited with {s}. \
                The bridge can't compile without `node_modules/`. \
                Run `pnpm install` in `crates/core/provider-github-copilot/bridge/` \
                manually to see the full error.",
            ),
            Err(e) => panic!(
                "Failed to invoke `pnpm install` for copilot bridge ({e}). \
                Cargo's build-script PATH does not contain `pnpm`. \
                Install pnpm via `corepack enable` or `npm i -g pnpm`, \
                then retry. Current PATH: {}",
                env::var("PATH").unwrap_or_else(|_| "<unset>".to_string()),
            ),
        }
    }

    // Direct tsc call (NOT via `pnpm run build`) — see
    // provider-claude-sdk/build.rs for the full rationale. Short
    // version: pnpm rewrites `node_modules/.package-lock.json` even on
    // no-op script invocations, which under `tauri dev` triggers an
    // infinite watcher rebuild loop. Calling tsc by path skips pnpm
    // entirely, leaving node_modules untouched.
    //
    // Skip tsc when dist is already fresh, AND don't even require tsc
    // to be installed in that case — the bridge artifact is what
    // matters, not the build tooling. Locating tsc itself is delegated
    // to `locate_tsc` which copes with missing .bin shims on Windows
    // by falling back to the pnpm virtual store and `node`.
    let dist_index = bridge_dir.join("dist/index.js");
    let tsconfig = bridge_dir.join("tsconfig.json");
    let src_index = bridge_dir.join("src/index.ts");
    let dist_fresh = match (
        fs::metadata(&dist_index).and_then(|m| m.modified()),
        fs::metadata(&src_index).and_then(|m| m.modified()),
        fs::metadata(&tsconfig).and_then(|m| m.modified()),
    ) {
        (Ok(d), Ok(s), Ok(c)) => d >= s && d >= c,
        _ => false,
    };

    if !dist_fresh {
        let mut tsc_cmd = locate_tsc(&bridge_dir).unwrap_or_else(|| {
            panic!(
                "Copilot bridge: cannot find a runnable tsc anywhere \
                under `node_modules/` (.bin shim, flat layout, or pnpm \
                virtual store). The node_modules tree appears corrupted — \
                most likely from interrupted pnpm installs during `tauri \
                dev` rebuild loops. Repair with: \
                `cd crates/core/provider-github-copilot/bridge && \
                rm -rf node_modules && pnpm install --prod=false`",
            )
        });
        let tsc_status = tsc_cmd
            .env("NODE_ENV", "development")
            .current_dir(&bridge_dir)
            .status();
        match tsc_status {
            Ok(s) if s.success() => {}
            Ok(s) => panic!(
                "Copilot bridge `tsc` exited with {s}. \
                The compile cannot embed an empty bridge — fix the TypeScript \
                error and re-run `cargo build`. Run `pnpm run build` in \
                `crates/core/provider-github-copilot/bridge/` to reproduce.",
            ),
            Err(e) => panic!(
                "Failed to invoke direct tsc for copilot bridge ({e}). \
                Reinstall the bridge dev deps: \
                `cd crates/core/provider-github-copilot/bridge && pnpm install --prod=false`",
            ),
        }
    }

    let assets_dir = out_dir.join("bridge-assets");
    if assets_dir.exists() {
        fs::remove_dir_all(&assets_dir).expect("failed to clean stale bridge-assets");
    }
    fs::create_dir_all(&assets_dir).expect("failed to create bridge-assets dir");

    let bridge_src = PathBuf::from("bridge/dist/index.js");
    if !bridge_src.exists() {
        // See provider-claude-sdk/build.rs for rationale. Reachable
        // only if `pnpm run build` succeeded above but didn't emit
        // dist/index.js — fail loud rather than ship an empty embed.
        panic!(
            "bridge/dist/index.js missing after a successful `pnpm run build`. \
            Check `bridge/tsconfig.json`'s outDir + `bridge/package.json`'s \
            build script. This should never happen.",
        );
    }

    fs::copy(&bridge_src, assets_dir.join("index.js")).expect("failed to copy bridge");
    if pkg_json.exists() {
        fs::copy(&pkg_json, assets_dir.join("package.json"))
            .expect("failed to copy bridge package.json");
    }
    if pnpm_lock.exists() {
        fs::copy(&pnpm_lock, assets_dir.join("pnpm-lock.yaml"))
            .expect("failed to copy pnpm-lock.yaml");
    }
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

/// Mirror of provider-claude-sdk/build.rs::locate_tsc — keep in sync.
/// See that copy for the full rationale (Windows symlink failures in
/// pnpm produce node_modules trees with no `.bin/tsc.cmd`; we walk
/// alternative locations and fall back to `node <script>` on Windows).
fn locate_tsc(bridge_dir: &Path) -> Option<Command> {
    let bin_dir = bridge_dir.join("node_modules").join(".bin");
    let shim_name = if cfg!(windows) { "tsc.cmd" } else { "tsc" };
    let shim = bin_dir.join(shim_name);
    if shim.exists() {
        return Some(Command::new(shim));
    }

    let mut script: Option<PathBuf> = None;
    let flat = bridge_dir
        .join("node_modules")
        .join("typescript")
        .join("bin")
        .join("tsc");
    if flat.exists() {
        script = Some(flat);
    } else {
        let pnpm_store = bridge_dir.join("node_modules").join(".pnpm");
        if let Ok(entries) = fs::read_dir(&pnpm_store) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("typescript@") {
                    let candidate = entry
                        .path()
                        .join("node_modules")
                        .join("typescript")
                        .join("bin")
                        .join("tsc");
                    if candidate.exists() {
                        script = Some(candidate);
                        break;
                    }
                }
            }
        }
    }
    let script = script?;

    // Canonicalize before invoking — see provider-claude-sdk/build.rs
    // for rationale (the caller sets `current_dir(bridge_dir)` which
    // would otherwise cause a relative `script` to resolve under
    // `bridge/bridge/...`). Then strip the `\\?\` UNC prefix that
    // `fs::canonicalize` adds on Windows (node's CJS resolver chokes
    // on verbatim paths with "EISDIR: lstat 'C:'").
    let script = fs::canonicalize(&script).unwrap_or(script);
    let script = strip_unc_prefix(&script);

    if cfg!(windows) {
        let mut cmd = Command::new("node");
        cmd.arg(script);
        Some(cmd)
    } else {
        Some(Command::new(script))
    }
}

/// Mirror of provider-claude-sdk/build.rs::strip_unc_prefix — keep in sync.
fn strip_unc_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        if !stripped.starts_with("UNC\\") {
            return PathBuf::from(stripped);
        }
    }
    p.to_path_buf()
}

/// Mirror of provider-claude-sdk/build.rs::assets_fresh. Returns true
/// when every staged output exists and is newer than every existing
/// input — used by the build-script fast-path to skip pnpm/tsc when
/// nothing has changed. Missing inputs are skipped; missing outputs
/// always count as stale. Keep in sync with the claude-sdk copy.
fn assets_fresh(inputs: &[PathBuf], outputs: &[&Path]) -> bool {
    use std::time::SystemTime;

    let mut newest_input: Option<SystemTime> = None;
    for src in inputs {
        let Ok(meta) = fs::metadata(src) else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        newest_input = Some(match newest_input {
            Some(prev) if prev >= mtime => prev,
            _ => mtime,
        });
    }
    let Some(newest_input) = newest_input else {
        return false;
    };

    for out in outputs {
        let Ok(meta) = fs::metadata(out) else { return false };
        let Ok(mtime) = meta.modified() else { return false };
        if mtime < newest_input {
            return false;
        }
    }
    true
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
/// (rare — the bridge build also requires `pnpm`, so devs reaching
/// this step have a JS toolchain installed), the check skips with a
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
    let output = Command::new(npm_program())
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

/// Mirror of `provider-claude-sdk/build.rs::preflight_pnpm`. Verify
/// `pnpm` is reachable from cargo's build-script subprocess before
/// any install / build step. See that file's doc-comment for the
/// full rationale.
fn preflight_pnpm(provider: &str) {
    let probe = Command::new(pnpm_program()).arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => {}
        Ok(out) => panic!(
            "{provider} bridge preflight: `pnpm --version` exited with {} \
            (stderr: {}). pnpm is on PATH but appears broken.",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        ),
        Err(e) => panic!(
            "{provider} bridge preflight: `pnpm` not found on PATH ({e}). \
            Cargo's build-script subprocess inherits PATH from its launching \
            shell — if you installed pnpm via mise, ensure mise's shims dir \
            is on PATH for non-interactive shells too. Quick fixes: \
            `corepack enable`, `npm i -g pnpm`, or add mise shims to \
            your login PATH (~/.zprofile, ~/.profile). Current PATH: {}",
            env::var("PATH").unwrap_or_else(|_| "<unset>".to_string()),
        ),
    }
}
