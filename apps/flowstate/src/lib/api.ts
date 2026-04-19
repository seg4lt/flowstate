// Barrel re-export so existing `@/lib/api` imports keep working. The
// per-domain modules live in `./api/{rpc,git,pty,display,usage,fs}.ts`;
// new code should import from those directly, but every symbol below
// is still available here to avoid a mechanical rewrite of the 20+
// call sites.

export * from "./api/rpc";
export * from "./api/git";
export * from "./api/pty";
export * from "./api/display";
export * from "./api/usage";
export * from "./api/fs";
