# apps/ — buildable applications

Top-level, user-facing applications. Each `<app>/` is a self-contained
deployable — its own set of binaries, its own app-specific sub-crates,
its own README.

- **[`zenui/`](./zenui/README.md)** — The ZenUI desktop experience:
  a native `zenui` binary (tao + wry window) plus a `zenui-server`
  daemon binary. Shares everything under `crates/core/` and
  `crates/middleman/`.

## Adding a new app

1. Create `apps/<your-app>/`.
2. Put the main buildable under `apps/<your-app>/main-application/`.
3. Put any app-specific sub-crates under `apps/<your-app>/crate/`.
4. Shared code goes in `crates/core/` or `crates/middleman/`, not under
   your app directory.
5. Add a README for the app as a whole at `apps/<your-app>/README.md`,
   plus one per crate underneath.
