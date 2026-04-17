# apps/ — buildable applications

Top-level, user-facing applications. Each `<app>/` is a self-contained
deployable — its own set of binaries, its own app-specific sub-crates,
its own README.

- **[`flowstate/`](./flowstate/README.md)** — Flowstate desktop app.
  A Tauri shell that consumes the agent runtime and provider adapters
  from `crates/core/` and `crates/middleman/` directly (no separate
  daemon process, no HTTP transport — uses `transport-tauri`).

## Adding a new app

1. Create `apps/<your-app>/`.
2. Put the main buildable under `apps/<your-app>/main-application/`
   (or whatever layout the app framework needs, e.g. Tauri uses
   `src-tauri/`).
3. Put any app-specific sub-crates under `apps/<your-app>/crate/`.
4. Shared code goes in `crates/core/` or `crates/middleman/`, not under
   your app directory.
5. Add a README for the app as a whole at `apps/<your-app>/README.md`,
   plus one per crate underneath.
