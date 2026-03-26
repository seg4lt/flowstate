//! GUI-launched PATH hydration.
//!
//! When flowzen is launched from Finder / the dock / a .desktop
//! file, the process starts with the bare launchd/systemd env —
//! roughly `PATH=/usr/bin:/bin:/usr/sbin:/sbin` plus locale. None of
//! the user's shell rc-file PATH additions (Homebrew, mise, nvm,
//! asdf, pyenv, cargo, pnpm, bun, …) are visible, so every
//! `Command::spawn` we make from Rust — the integrated terminal's
//! pty shell, `open_in_editor` launching Zed/Cursor/Code, the `git`
//! subcommands used for the diff panel and branch switcher — sees a
//! stripped-down PATH and fails to find anything the user installed
//! through their own tool manager.
//!
//! The canonical fix (same one VS Code, Atom, GitHub Desktop, Fig,
//! and the npm `shell-env` package use) is to spawn the user's
//! login + interactive shell exactly once at startup, run `env`
//! inside it, and copy the resulting variables into our own process
//! env. All subsequent `Command::spawn` calls inherit the enriched
//! env for free — no per-call plumbing. The pty backend also
//! iterates `std::env::vars()` when building its CommandBuilder, so
//! the integrated terminal picks up the same PATH that the user
//! would see in Terminal.app.
//!
//! The probe must run **before** any additional thread starts, both
//! because `std::env::set_var` is unsound in a multithreaded process
//! and because the enrichment needs to be visible to the first
//! Tauri / tokio worker we spawn. `run()` in `lib.rs` calls this
//! right after `init_tracing()` and before `tauri::Builder::default()`.
//!
//! The probe is also expensive — it pays the full cost of sourcing the
//! user's rc files, which ranges from ~20ms on a plain zsh to 300-800ms
//! on an oh-my-zsh + powerlevel10k + plugins setup, all while the
//! window is invisible. Terminal-launched processes (the whole dev
//! loop plus anyone who starts the app from an already-warm shell)
//! inherit a rich PATH for free, so the probe is pure waste in that
//! case. `path_already_rich()` is the portable fast-path check: if
//! PATH contains any entry under the user's home directory, we were
//! launched from a shell (or from a Windows GUI, which always
//! inherits the full user env) and the probe is skipped entirely.
//! Bare launchd/systemd launches never populate PATH with home
//! entries, so the inverse reliably catches "we need to probe".

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

/// Marker printed immediately before `env` inside the probe script.
/// `.zshrc` / `.bashrc` files routinely `echo` greetings, version
/// strings, or cowsay banners on startup — splitting on this
/// delimiter discards all of it so only the real env dump is
/// parsed. Long + improbable on purpose.
#[cfg(unix)]
const DELIMITER: &str = "__FLOWZEN_SHELL_ENV_DELIMITER__";

pub fn hydrate_from_login_shell() {
    if path_already_rich() {
        tracing::debug!(
            path = ?std::env::var_os("PATH"),
            "PATH already contains a home-dir entry; skipping login-shell env probe",
        );
        return;
    }
    #[cfg(unix)]
    hydrate_unix();
    // Windows: no login-shell concept, and a bare-PATH Windows GUI
    // launch is vanishingly rare (explorer.exe always inherits the
    // full user env), so there's nothing to fall back to here.
}

/// Portable "PATH already looks rich" check.
///
/// A bare launchd / systemd / .desktop launch on Unix gets a PATH
/// like `/usr/bin:/bin:/usr/sbin:/sbin` — exclusively system dirs,
/// never anything under the user's home. Every shell rc file in
/// common use adds at least one `$HOME/...` entry (`.cargo/bin`,
/// `.local/bin`, `.bun/bin`, `.rbenv/shims`, `.nvm/versions/.../bin`,
/// `go/bin`, …), so "PATH contains a home-dir entry" is a reliable
/// one-signal test for "we were launched from a terminal (or a
/// Windows GUI, which always inherits the full user PATH) and the
/// expensive login-shell probe would just rediscover what we
/// already have".
fn path_already_rich() -> bool {
    // `USERPROFILE` is the Windows equivalent of `HOME`. Fall back
    // to it so the same helper works on Windows without a cfg gate.
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return false;
    };
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let home = Path::new(&home);
    // `split_paths` uses the OS-native separator (`;` on Windows,
    // `:` elsewhere) so we don't need a cfg branch. `Path::starts_with`
    // does component-aware prefix matching, so a home of `/home/alice`
    // doesn't false-match an entry under `/home/alicia`.
    std::env::split_paths(&path).any(|entry| entry.starts_with(home))
}

#[cfg(unix)]
fn hydrate_unix() {
    let shell = match std::env::var("SHELL") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            tracing::debug!("$SHELL unset; skipping login-shell env probe");
            return;
        }
    };

    // `-l -i` together source the full chain of rc files:
    //   login  → /etc/zprofile (path_helper on macOS), ~/.zprofile,
    //            /etc/profile, ~/.profile, ~/.bash_profile
    //   inter. → ~/.zshrc, ~/.bashrc, conf.d/*.fish
    // which covers every PATH-extension style in common use. The
    // script prints the delimiter first so any banner/`echo` output
    // the rc files produced can be skipped, then `exec env` hands
    // stdout over to /usr/bin/env without an extra fork. stdin is
    // /dev/null because some interactive shells complain when
    // launched on a non-tty without it.
    let script = format!("printf '\\n%s\\n' '{DELIMITER}'; exec /usr/bin/env");
    let output = match Command::new(&shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            tracing::warn!(
                shell = %shell,
                code = ?out.status.code(),
                "login-shell env probe exited with non-zero status; PATH may be incomplete",
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                shell = %shell,
                error = %err,
                "failed to spawn login shell for env probe",
            );
            return;
        }
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let body = match text.find(DELIMITER) {
        Some(idx) => &text[idx + DELIMITER.len()..],
        None => {
            tracing::warn!("login-shell probe never emitted delimiter; skipping env hydration");
            return;
        }
    };

    // Carry every variable the login shell exposed, minus a short
    // blocklist of vars that describe the shell session itself and
    // would actively confuse the flowzen process if overwritten:
    //   PWD     — our cwd, not the shell's
    //   OLDPWD  — meaningless outside that shell session
    //   SHLVL   — shell nesting depth counter
    //   _       — the last-command variable
    //   PS1/PS2 — shell prompt strings
    const SKIP: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_", "PS1", "PS2"];
    let parsed = parse_env(body);
    let mut hydrated = 0usize;
    for (key, value) in parsed {
        if SKIP.contains(&key.as_str()) {
            continue;
        }
        if std::env::var(&key).ok().as_deref() == Some(value.as_str()) {
            continue;
        }
        // SAFETY: this function's contract is that it runs before
        // any additional threads are spawned — see the module doc
        // and the call site in `run()`. `set_var` is only unsound
        // when concurrent getenv()s can race, and there are none
        // here yet.
        std::env::set_var(&key, &value);
        hydrated += 1;
    }

    tracing::debug!(
        shell = %shell,
        hydrated,
        path = %std::env::var("PATH").unwrap_or_default(),
        "hydrated process env from login shell",
    );
}

#[cfg(unix)]
fn parse_env(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    // `env` emits one KEY=VALUE per line for the overwhelming
    // majority of variables. Values with embedded newlines (rare —
    // set via `export FOO=$'a\nb'`) would technically span lines,
    // but the variables users care about here (PATH, MANPATH,
    // HOMEBREW_*, NVM_DIR, MISE_*, BUN_INSTALL, PNPM_HOME, …) are
    // all single-line, so a plain line split is sufficient.
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if !k.is_empty() {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}
