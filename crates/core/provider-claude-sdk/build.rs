use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Cross-platform program name for pnpm. On Windows the
/// `pnpm/action-setup` (and most installers) place pnpm as
/// `pnpm.cmd` in `node_modules/.bin/`. Rust's `Command::new` on
/// Windows does NOT reliably resolve bare names through PATHEXT for
/// `.cmd`/`.bat` shims (CreateProcessW only auto-appends `.exe`),
/// so spawning bare "pnpm" fails with a "program not found" error
/// even when pnpm is on PATH. Spelling the extension explicitly
/// sidesteps the lookup ambiguity.
fn pnpm_program() -> &'static str {
    if cfg!(windows) { "pnpm.cmd" } else { "pnpm" }
}

/// Same Windows-shim caveat as `pnpm_program` — `npm` ships as
/// `npm.cmd` on Windows runners.
fn npm_program() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}

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
    println!("cargo:rerun-if-changed=bridge/pnpm-lock.yaml");
    println!("cargo:rerun-if-changed=bridge/tsconfig.json");
    // Embedded Node version is part of DEPS_FP (a node major bump can
    // invalidate native modules in `node_modules/`), so re-run when
    // it changes. Path is relative to this crate's manifest dir.
    println!("cargo:rerun-if-changed=../embedded-node/src/lib.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EMBED");

    let embed = env::var("CARGO_FEATURE_EMBED").is_ok();

    if !bridge_dir.join("src/index.ts").exists() {
        // No bridge source: emit empty stubs so the crate still
        // compiles (useful for docs-only builds).
        write_fingerprints(&out_dir, "0000000000000000", "0000000000000000");
        fs::create_dir_all(out_dir.join("bridge-assets")).ok();
        return;
    }

    // -----------------------------------------------------------------
    // Fast-path: skip the entire pnpm/tsc pipeline when nothing has
    // changed since the last successful build.
    //
    // Why this exists: invoking `pnpm <anything>` causes pnpm to
    // re-sync `bridge/node_modules/.package-lock.json` even on no-op
    // commands. Under `tauri dev`, the file watcher sees that write
    // and triggers a rebuild — which reruns this build script — which
    // reinvokes pnpm — which rewrites `.package-lock.json` again. The
    // build never settles.
    //
    // The cure is to never invoke pnpm in the first place when the
    // staged `bridge-assets/` already reflects the current source.
    // We compare:
    //   - bridge/src/index.ts        (the only TS source)
    //   - bridge/tsconfig.json       (compiler config)
    //   - bridge/package.json        (deps + build script)
    //   - bridge/package-lock.json   (deps)
    //   - bridge/pnpm-lock.yaml      (deps)
    //   - ../embedded-node/src/lib.rs (NODE_VERSION feeds DEPS_FP)
    // …against the staged outputs in OUT_DIR/bridge-assets/. If every
    // staged output exists and is newer than every source, we have
    // nothing to do — skip straight to fingerprint emission.
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
        // Re-emit the rustc-env vars from the cached fingerprint files
        // so the consumer crate still sees them this build. (Cargo
        // doesn't persist `cargo:rustc-env=` between build-script
        // runs — every invocation must re-emit.)
        let glue_fp = fs::read_to_string(&glue_fp_path).unwrap_or_default();
        let deps_fp = fs::read_to_string(&deps_fp_path).unwrap_or_default();
        println!("cargo:rustc-env=BRIDGE_GLUE_FINGERPRINT={glue_fp}");
        println!("cargo:rustc-env=BRIDGE_DEPS_FINGERPRINT={deps_fp}");
        println!("cargo:rerun-if-changed={}", glue_fp_path.display());
        println!("cargo:rerun-if-changed={}", deps_fp_path.display());
        return;
    }

    // Fail-fast guard — see provider-github-copilot/build.rs for the
    // full rationale. `npm ci` at runtime refuses to install when
    // package.json and package-lock.json disagree on dep versions, so
    // we catch drift at build time before it ships.
    //
    // Gated by a stamp so it runs at most once per lockfile change —
    // not on every cargo build. Why: `npm ci --dry-run`, despite the
    // flag, re-syncs `bridge/node_modules/.package-lock.json` to match
    // the parent `package-lock.json` as a side effect. Under
    // `tauri dev`, that write trips the file watcher into an infinite
    // rebuild loop. Skipping the check when nothing relevant has
    // changed avoids the rewrite without weakening the guarantee:
    // if `package.json`/`package-lock.json` haven't changed since the
    // last successful validation, drift can't have appeared.
    let validation_stamp = out_dir.join(".lockfile-validation-stamp");
    let pkg_json_for_stamp = bridge_dir.join("package.json");
    let pkg_lock_for_stamp = bridge_dir.join("package-lock.json");
    if !is_stamp_fresh(
        &validation_stamp,
        &[&pkg_json_for_stamp, &pkg_lock_for_stamp],
    ) {
        validate_lockfile_consistency(&bridge_dir, "Claude SDK");
        touch_stamp(&validation_stamp);
    }

    // pnpm preflight: confirm the binary is reachable from the
    // build-script's child-process PATH BEFORE we get into stamp
    // logic. Cargo's child env is inherited from the parent, but
    // shell-only PATH modifications (e.g. mise's PATH shims that
    // only fire from interactive shells) won't propagate to a build
    // run from a stripped-PATH context. A failure here used to
    // surface as a runtime "bridge assets are empty" error far
    // removed from the cause; now it's a clear build-time panic.
    preflight_pnpm("Claude SDK");

    // --- pnpm install ----------------------------------------------
    //
    // pnpm is the source of truth for dev tooling (corepack ships
    // with node, every contributor has it). bun is intentionally not
    // used: it's an extra cross-platform install with no
    // distinguishing benefit here, and not having it on PATH used
    // to silently brick the bridge build.
    //
    // Install runs unconditionally (gated by the staleness stamp).
    // It used to be gated on `embed` because only the offline-bundle
    // build path needed `node_modules/` staged into the embed dir —
    // but the *build step below* also needs `node_modules/.bin/tsc`,
    // so non-embed builds were failing with "tsc: command not found"
    // unless install had been run by some other path. The stamp
    // already de-dupes redundant runs.
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
        // Pin a development-friendly environment: cargo's build-
        // script subprocess can inherit `NODE_ENV=production` /
        // `PNPM_PROD=true` / `npm_config_*` flags from the
        // launching shell or a parent tooling layer (cargo test
        // sets some of these). Any one of them makes `pnpm
        // install` silently drop `devDependencies` (typescript /
        // @types/*). Without typescript the next `pnpm run build`
        // step dies with "tsc: command not found" and dist stays
        // empty. Force include-dev with the explicit `--prod=false`
        // CLI flag (which beats env var precedence in pnpm) and
        // clear every env var pnpm/npm checks for production
        // signaling.
        // Cargo's build-script subprocess inherits parent env, plus
        // adds dozens of `CARGO_*` vars and (depending on the
        // Cargo.toml) sometimes forwards `npm_config_*` /
        // `NODE_ENV=production`. Any non-empty value of those is
        // treated as truthy by pnpm's legacy flag-style env
        // handling. `env_remove` doesn't help when the shell
        // already exported them in production mode upstream.
        //
        // The robust fix: clear the env entirely, then re-add only
        // what pnpm strictly needs. PATH (so pnpm finds node/git),
        // HOME (npm's config lookup), and a couple of TMP / locale
        // vars so node doesn't crash on missing locale settings.
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
                // mise/asdf/nvm etc. pnpm respects these to find the
                // right node binary; preserving them keeps the
                // install pinned to the same toolchain the dev shell
                // uses.
                .env(
                    "MISE_DATA_DIR",
                    env::var("MISE_DATA_DIR").unwrap_or_default(),
                )
                .env("LANG", env::var("LANG").unwrap_or_default());
            // Windows-essential env vars stripped by `env_clear` —
            // re-add them or pnpm dies on first call into Node.
            // `os.tmpdir()` reads `TEMP`/`TMP`; without it pnpm's
            // `temp-dir@2.0.0` dep crashes calling
            // `realpathSync(undefined)`. `APPDATA`/`USERPROFILE` are
            // where pnpm/npm look up `.npmrc` and the global store.
            // `LOCALAPPDATA` is where corepack caches package-manager
            // shims. `SYSTEMROOT` is required by some Win32 APIs
            // (`GetSystemDirectory`, DNS lookups) that node calls
            // during startup. Missing any of these used to surface as
            // `ENOENT: no such file or directory, lstat '…\bridge\undefined'`
            // on Windows CI runners (where launching shells don't
            // export these into a stripped env). Unconditional copy
            // here is a no-op on Unix where these vars don't exist.
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
                "Claude SDK bridge `pnpm install` exited with {s}. \
                The bridge can't compile without `node_modules/`. \
                Run `pnpm install` in `crates/core/provider-claude-sdk/bridge/` \
                manually to see the full error.",
            ),
            Err(e) => panic!(
                "Failed to invoke `pnpm install` for claude bridge ({e}). \
                Cargo's build-script PATH does not contain `pnpm`. \
                Install pnpm via `corepack enable` or `npm i -g pnpm`, \
                then retry. Current PATH: {}",
                env::var("PATH").unwrap_or_else(|_| "<unset>".to_string()),
            ),
        }
    }

    // --- tsc (always — the dist output is what we actually ship) -----
    //
    // Run tsc DIRECTLY via the node_modules/.bin shim — NOT via
    // `pnpm run build`. Going through pnpm causes pnpm to re-sync
    // `bridge/node_modules/.package-lock.json` as part of its routine
    // workspace bookkeeping, even on no-op script invocations. Under
    // `tauri dev`, that rewrite trips the file watcher into an
    // infinite rebuild loop (the watcher fires → cargo restarts →
    // build.rs reruns → pnpm rewrites the file → repeat). Calling tsc
    // by path skips pnpm entirely, leaving node_modules untouched.
    //
    // Trade-off: pnpm's `run` semantics (env injection, npm-script
    // lifecycle hooks like prebuild/postbuild) are bypassed. The
    // bridge's `build` script is just `tsc` with no hooks, so this is
    // a faithful equivalent. If you ever add lifecycle hooks to
    // `bridge/package.json`, you'll need a pnpm-equivalent that
    // doesn't touch `.package-lock.json` (e.g. `pnpm exec tsc` may
    // suffice — verify before switching).
    // Skip the tsc invocation when `bridge/dist/index.js` is already
    // newer than every input tsc would consume (`bridge/src/**` and
    // `tsconfig.json`). This shaves seconds off every incremental
    // build AND avoids touching dist/index.js unnecessarily — useful
    // for keeping downstream fingerprint mtime stable. Importantly,
    // when dist is fresh we DON'T require `tsc` to be installed at
    // all — the node_modules tree is allowed to be partial as long as
    // the build artifact itself is valid.
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
                "Claude SDK bridge: cannot find a runnable tsc anywhere \
                under `node_modules/` (.bin shim, flat layout, or pnpm \
                virtual store). The node_modules tree appears corrupted — \
                most likely from interrupted pnpm installs during `tauri \
                dev` rebuild loops. Repair with: \
                `cd crates/core/provider-claude-sdk/bridge && \
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
                "Claude SDK bridge `tsc` exited with {s}. \
                The compile cannot embed an empty bridge — fix the TypeScript \
                error and re-run `cargo build`. Run `pnpm run build` in \
                `crates/core/provider-claude-sdk/bridge/` to reproduce.",
            ),
            Err(e) => panic!(
                "Failed to invoke direct tsc for claude bridge ({e}). \
                Reinstall the bridge dev deps: \
                `cd crates/core/provider-claude-sdk/bridge && pnpm install --prod=false`",
            ),
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
        // Reachable only if `pnpm run build` succeeded (above) but
        // somehow didn't produce dist/index.js — e.g. tsconfig
        // misconfigured, the build script does something custom that
        // skips emit. Fail loud rather than ship an empty embed: the
        // alternative is the runtime "bridge assets are empty" error
        // that bricks every Claude session.
        panic!(
            "bridge/dist/index.js missing after a successful `pnpm run build`. \
            Check `bridge/tsconfig.json`'s outDir + `bridge/package.json`'s \
            build script. This should never happen.",
        );
    }

    // Always include the small files: the bridge entry point, its
    // package.json (the runtime uses it to resolve deps), and any
    // lockfiles (for reproducible runtime hydration via
    // `pnpm install --frozen-lockfile` and the `npm ci` fallback).
    fs::copy(&bridge_src, assets_dir.join("index.js")).expect("failed to copy bridge");
    if pkg_json.exists() {
        fs::copy(&pkg_json, assets_dir.join("package.json"))
            .expect("failed to copy bridge package.json");
    }
    if pnpm_lock.exists() {
        // Preferred lockfile: corepack-pnpm at runtime uses this for
        // `pnpm install --frozen-lockfile`.
        fs::copy(&pnpm_lock, assets_dir.join("pnpm-lock.yaml"))
            .expect("failed to copy pnpm-lock.yaml");
    }
    if npm_lock.exists() {
        // Fallback lockfile for the runtime's `npm ci --omit=dev`
        // path. Both can ship; pnpm picks pnpm-lock.yaml when both
        // are present.
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
        // Mix a separator so name-vs-content boundaries don't smear.
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
/// embedded-node crate's lib.rs. See provider-github-copilot/build.rs
/// for rationale (build-deps would force the embedded-node crate's
/// own download-build to run as part of OUR build script — too
/// expensive). Reading the source file is cheap and stable.
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

/// Build a `Command` that invokes `tsc` from the bridge's node_modules,
/// without going through pnpm.
///
/// pnpm on Windows sometimes installs the typescript package contents
/// into `.pnpm/typescript@<ver>/node_modules/typescript/` but fails to
/// create the `.bin/tsc.cmd` shim — typically because symlink creation
/// requires Developer Mode / admin, or because a previous install was
/// interrupted (which happens a lot when `tauri dev`'s file watcher
/// kills cargo mid-build). To stay robust against half-installed
/// node_modules trees, we search multiple candidate paths.
///
/// Search order (first match wins):
///   1. `.bin/tsc.cmd` (windows) / `.bin/tsc` (unix) — the canonical
///      shim. If present, we run it directly.
///   2. `typescript/bin/tsc` — flat npm-style layout.
///   3. `.pnpm/typescript@*/node_modules/typescript/bin/tsc` — pnpm's
///      content-addressable virtual store.
///
/// For cases 2/3 we get a raw JS file (with a `#!/usr/bin/env node`
/// hashbang). On Unix the hashbang makes it directly executable; on
/// Windows we wrap it as `node <script>` using the `node` on PATH.
/// This means the Windows fallback requires a working node install —
/// which the bridge needs anyway (it's how the embedded bridge runs
/// at runtime), so it's a reasonable assumption.
fn locate_tsc(bridge_dir: &Path) -> Option<Command> {
    let bin_dir = bridge_dir.join("node_modules").join(".bin");
    let shim_name = if cfg!(windows) { "tsc.cmd" } else { "tsc" };
    let shim = bin_dir.join(shim_name);
    if shim.exists() {
        return Some(Command::new(shim));
    }

    // Find the raw tsc JS file. Try the flat layout first, then walk
    // pnpm's virtual store.
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

    // The caller sets `current_dir(bridge_dir)`, and we'll pass `script`
    // as an arg to `node` (or as the program on Unix). If `script` is
    // a relative path, the current_dir change makes it resolve under
    // the bridge's bridge subdir (path doubling). Canonicalize to an
    // absolute path so it resolves correctly regardless of cwd.
    //
    // On Windows, `fs::canonicalize` returns UNC verbatim paths
    // (`\\?\C:\...`), which node's CJS resolver chokes on with
    // "EISDIR: illegal operation on a directory, lstat 'C:'". Strip
    // the prefix so node sees a plain `C:\...` path.
    let script = fs::canonicalize(&script).unwrap_or(script);
    let script = strip_unc_prefix(&script);

    if cfg!(windows) {
        // Windows: shell out to `node` on PATH with the script as
        // arg 0 of tsc.
        let mut cmd = Command::new("node");
        cmd.arg(script);
        Some(cmd)
    } else {
        // Unix: hashbang makes the file directly executable.
        Some(Command::new(script))
    }
}

/// Strip the `\\?\` UNC verbatim prefix that `fs::canonicalize` adds
/// on Windows. Many Win32 APIs and most cross-platform tools (Node.js,
/// Python, …) refuse to operate on verbatim paths because they
/// disable per-component normalization. The plain DOS form (`C:\…`)
/// works everywhere.
///
/// Returns the input unchanged on non-Windows platforms or when the
/// prefix isn't present.
fn strip_unc_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        // Don't strip if it's a true UNC path (`\\?\UNC\server\share\...`) —
        // turning that into `UNC\server\share\...` is wrong. Such
        // paths only occur when canonicalizing a path that started as
        // a network share, which we don't expect under target/.
        if !stripped.starts_with("UNC\\") {
            return PathBuf::from(stripped);
        }
    }
    p.to_path_buf()
}

/// Returns `true` when every path in `outputs` exists and is at least
/// as new as the newest path in `inputs`. Missing inputs are skipped
/// (treated as not-an-input) — the bridge tolerates an absent
/// pnpm-lock.yaml or package-lock.json depending on which package
/// manager generated it. Missing outputs always count as stale.
///
/// Used by the build-script fast-path to skip the pnpm/tsc pipeline
/// when nothing relevant has changed. See the call site for the full
/// rationale (avoids an infinite rebuild loop under `tauri dev`).
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
    // No inputs found (unusual — would mean the bridge dir is empty).
    // Be conservative and rebuild.
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
/// (rare — the bridge build also requires `pnpm`, so devs reaching
/// this step have a JS toolchain installed), the check skips with a
/// warning rather than failing the build.
fn validate_lockfile_consistency(bridge_dir: &Path, provider: &str) {
    let pkg_path = bridge_dir.join("package.json");
    let lock_path = bridge_dir.join("package-lock.json");
    if !pkg_path.exists() || !lock_path.exists() {
        return;
    }

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

/// Verify `pnpm` is reachable from the build script's child-process
/// PATH before the install / build steps run. Cargo inherits PATH
/// from its launching shell, but shell-only modifications (mise
/// shims, sourced rc-file additions) sometimes don't propagate to
/// non-interactive subprocesses.
///
/// This is a guard against the silent failure mode that produced
/// the original "Claude SDK bridge assets are empty" runtime error:
/// pnpm wasn't on the build-script PATH, both `pnpm install` and
/// `pnpm run build` exited with errors that were logged as
/// `cargo:warning=…`, the rust-embed proc-macro saw an empty
/// bridge-assets dir, and the binary shipped with no embedded
/// bridge. Failing the build here turns that mystery into a
/// pointed actionable error.
fn preflight_pnpm(provider: &str) {
    let probe = Command::new(pnpm_program()).arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => {
            // pnpm exists on PATH — nothing to do. We deliberately
            // don't enforce a specific version; the bridges' tooling
            // works on any recent pnpm major.
        }
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
