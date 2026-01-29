use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir missing"));
    // From apps/zenui/crate/server → workspace root requires four levels up,
    // then into frontend/.
    let frontend_dir = manifest_dir.join("../../../../frontend");

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

    println!("cargo:warning=Building ZenUI frontend with Bun");

    let install_args: &[&str] = if frontend_dir.join("bun.lock").exists() {
        &["install", "--frozen-lockfile"]
    } else {
        &["install"]
    };

    run_bun(&frontend_dir, install_args);
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
