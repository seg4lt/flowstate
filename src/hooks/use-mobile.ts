import * as React from "react"

const MOBILE_BREAKPOINT = 768
const DESKTOP_QUERY = `(min-width: ${MOBILE_BREAKPOINT}px)`

// Use a single source of truth — the same MediaQueryList drives the
// initial value AND updates, so they cannot disagree. Mirrors Tailwind's
// `md:` breakpoint (`min-width: 768px`) exactly so the JS `isMobile`
// flag is always the strict inverse of the CSS that renders the desktop
// sidebar (`hidden md:block`).
//
// Previously this hook mixed `window.matchMedia('(max-width: 767px)')`
// with `window.innerWidth < 768`. At fractional CSS-pixel viewport
// widths produced by Tauri's `setZoom()` (Cmd+=), those two checks and
// the CSS `min-width: 768px` rule could all disagree by 1px. The
// breaking case: JS thought "desktop, route toggle to setOpen" while
// CSS still had the desktop sidebar `hidden`, so the trigger silently
// flipped state without anything appearing on screen.
//
// Tauri is not SSR, so reading `window` during the initial state
// callback is safe — and it must be synchronous (not `undefined →
// effect → flip`), otherwise the very first `toggleSidebar()` after
// mount would route to the wrong state branch.
function readInitial(): boolean {
  if (typeof window === "undefined") return false
  return !window.matchMedia(DESKTOP_QUERY).matches
}

export function useIsMobile() {
  const [isMobile, setIsMobile] = React.useState<boolean>(readInitial)

  React.useEffect(() => {
    const mql = window.matchMedia(DESKTOP_QUERY)
    const onChange = () => setIsMobile(!mql.matches)
    mql.addEventListener("change", onChange)
    // Re-sync in case the breakpoint crossed between the initial state
    // read and the effect mount (e.g. a resize or Cmd+= fired during
    // the first render).
    onChange()
    return () => mql.removeEventListener("change", onChange)
  }, [])

  return isMobile
}
