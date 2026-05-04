// Build the `flow` CLI for the host target and stage it where Tauri's
// externalBin config expects to find it.
//
// Why this exists:
//   tauri.windows.conf.json declares `externalBin: ["binaries/flow"]`,
//   which makes the flowstate crate's tauri-build script look up
//   `binaries/flow-<host-triple>.exe` (or unsuffixed on Unix) before
//   compiling. Without a staged binary, the whole Rust build aborts
//   with `resource path 'binaries\flow-...' doesn't exist`. The CI
//   `build-windows` recipe handles this manually, but local
//   `pnpm tauri dev` had no equivalent step — this script fills that
//   gap and is wired into `package.json`'s `dev` / `build` scripts so
//   it runs automatically before Vite + Tauri start.
//
// Why a Node script (not a justfile recipe or shell snippet):
//   tauri.conf.json's beforeDevCommand / beforeBuildCommand are the
//   only hooks guaranteed to run no matter how dev/build is launched
//   (CLI, IDE button, npm/pnpm wrapper, etc.). Those hooks invoke
//   `pnpm dev` / `pnpm build`, so chaining staging into those scripts
//   means every entry point — `just dev`, `pnpm tauri dev`, the Tauri
//   VS Code extension — picks it up. A Node implementation keeps the
//   whole thing cross-platform without depending on bash/coreutils
//   being on PATH on Windows.

import { execSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tauriDir = resolve(here, "..");          // apps/flowstate
const repoRoot = resolve(tauriDir, "..", ".."); // <repo>

// Detect host target triple from rustc. tauri's externalBin lookup
// uses this exact string, so we mirror it here. `rustc -vV` prints
// a `host: <triple>` line we can parse.
const rustcVV = execSync("rustc -vV", { encoding: "utf8" });
const hostMatch = rustcVV.match(/^host:\s*(.+)$/m);
if (!hostMatch) {
  console.error("[stage-flow-cli] could not parse host triple from `rustc -vV`:");
  console.error(rustcVV);
  process.exit(1);
}
const hostTriple = hostMatch[1].trim();
const exeSuffix = hostTriple.includes("windows") ? ".exe" : "";

// Build the `flow` crate against the host's default profile (debug).
// Release-profile staging for `pnpm tauri build` is handled by the
// existing `just build-windows` recipe today; if/when a host-target
// release build path is needed, this script can read an env var
// (e.g. STAGE_FLOW_PROFILE=release) and adjust both the cargo flag
// and the source path.
console.log(`[stage-flow-cli] cargo build -p flow (host: ${hostTriple})`);
execSync("cargo build -p flow", { stdio: "inherit", cwd: repoRoot });

const binariesDir = join(tauriDir, "src-tauri", "binaries");
mkdirSync(binariesDir, { recursive: true });

const src = join(repoRoot, "target", "debug", `flow${exeSuffix}`);
const dst = join(binariesDir, `flow-${hostTriple}${exeSuffix}`);
copyFileSync(src, dst);

console.log(`[stage-flow-cli] staged ${dst}`);
