use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir missing"));
    // From crates/middleman/transport-http → apps/zenui/frontend is three
    // levels up and then into apps/zenui/frontend.
    //   ..           = crates/middleman
    //   ../..        = crates
    //   ../../..     = repo root
    //   ../../../apps/zenui/frontend
    let frontend_dir = manifest_dir
        .join("../../../apps/zenui/frontend")
        .canonicalize()
        .unwrap_or_else(|err| {
            panic!(
                "failed to canonicalize frontend dir from {}: {err}",
                manifest_dir.display()
            )
        });

    watch_path(&frontend_dir.join("package.json"));
    watch_path(&frontend_dir.join("bun.lock"));
    watch_path(&frontend_dir.join("index.html"));
    watch_path(&frontend_dir.join("tsconfig.json"));
    watch_path(&frontend_dir.join("tsconfig.app.json"));
    watch_path(&frontend_dir.join("tsconfig.node.json"));
    watch_path(&frontend_dir.join("vite.config.ts"));
    watch_dir(&frontend_dir.join("src"));

    if env::var_os("ZENUI_SKIP_FRONTEND_BUILD").is_some() {
        println!(
            "cargo:warning=Skipping ZenUI frontend build because ZENUI_SKIP_FRONTEND_BUILD is set"
        );
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR missing"));
    let bun_install_stamp = out_dir.join(".bun-install-stamp");
    let pkg_json = frontend_dir.join("package.json");
    let bun_lock = frontend_dir.join("bun.lock");

    if !is_stamp_fresh(&bun_install_stamp, &[&pkg_json, &bun_lock]) {
        let install_args: &[&str] = if bun_lock.exists() {
            &["install", "--frozen-lockfile"]
        } else {
            &["install"]
        };
        run_bun(&frontend_dir, install_args);
        touch_stamp(&bun_install_stamp);
    }

    run_bun(&frontend_dir, &["run", "build"]);
}

fn watch_dir(path: &Path) {
    if !path.exists() {
        return;
    }

    println!("cargo:rerun-if-changed={}", path.display());

    let entries = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("failed to read frontend path {}: {error}", path.display()));

    for entry in entries {
        let entry = entry.unwrap_or_else(|error| {
            panic!(
                "failed to read directory entry in {}: {error}",
                path.display()
            )
        });
        let entry_path = entry.path();
        if entry_path.is_dir() {
            watch_dir(&entry_path);
        } else {
            watch_path(&entry_path);
        }
    }
}

fn watch_path(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn run_bun(frontend_dir: &Path, args: &[&str]) {
    let status = Command::new("bun")
        .args(args)
        .current_dir(frontend_dir)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "failed to launch Bun in {} with args {:?}: {error}",
                frontend_dir.display(),
                args
            )
        });

    if !status.success() {
        panic!(
            "Bun command failed in {} with args {:?} and status {}",
            frontend_dir.display(),
            args,
            status
        );
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
