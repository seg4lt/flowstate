# Screenshots

App screenshots used by the top-level `README.md`. Captured on macOS
with [`axctl`](https://github.com/) in `app` mode so every window
owned by the process is composited into a single PNG.

## Capture

`axctl` requires **Accessibility** and **Screen Recording**
permissions in *System Settings → Privacy & Security*. Grant both to
your terminal (or to `axctl` itself) before running these commands.

```sh
# Whole window (topmost visible)
axctl screenshot flowstate --mode window \
  --output docs/screenshots/chat.png

# All visible windows owned by the app, composited
axctl screenshot flowstate --mode app \
  --output docs/screenshots/orchestration.png
```

## Shot list

The top-level README references these filenames. Capture them in the
order below so each shot has the right state loaded.

| File | What to show |
|---|---|
| `chat.png` | Default chat view with a Claude session mid-response |
| `orchestration.png` | A turn that called `spawn_and_await`, tool-call card expanded |
| `worktree.png` | Worktree picker / `create_worktree` result card |
| `sidebar.png` | Session sidebar grouped by project, with archived hidden |

Re-capture whenever the UI meaningfully changes. Keep the window at a
consistent size (roughly 1440×900) so the images line up in the
README.
