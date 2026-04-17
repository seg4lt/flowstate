import * as React from "react"

const MOBILE_BREAKPOINT = 768
const DESKTOP_QUERY = `(min-width: ${MOBILE_BREAKPOINT}px)`

// We mirror Tailwind's `md:` breakpoint (`min-width: 768px`) exactly so
// the JS `isMobile` flag is always the strict inverse of the CSS that
// renders the desktop sidebar (`hidden md:block`). Without that, the
// trigger button can route to a state branch (desktop `setOpen`) whose
// element is CSS-hidden, producing a silent click.
//
// We listen to THREE things and read `mql.matches` live in each
// handler. None of these alone is reliable in Tauri's WKWebView:
//
//   1. `window.resize` — most reliable for native window resizes,
//      but never fires on `setZoom()`.
//   2. `ResizeObserver(document.body)` — fires post-reflow when the
//      CSS-pixel viewport changes, which is what `setZoom()` does
//      (the window doesn't resize, only the page's layout viewport).
//      Body, not documentElement: the `<html>` element's box doesn't
//      always re-layout under all layout contexts, but body always
//      reflows when the viewport reflows.
//   3. `MediaQueryList.change` — the "correct" API, but in this
//      WKWebView it's unreliable for both resize AND setZoom. Kept
//      as a cheap third path in case the others miss an edge case.
//
// All three call the same handler, which reads `mql.matches` live, so
// duplicate fires are harmless (setIsMobile bails when the value
// didn't change). The visible bug this prevents: after a single zoom
// or resize past 768px, the desktop sidebar gets CSS-hidden, the
// click handler keeps routing to `setOpen`, and the trigger appears
// dead because the desktop sidebar element is `display: none`.
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

    window.addEventListener("resize", onChange)
    const ro = new ResizeObserver(onChange)
    ro.observe(document.body)
    mql.addEventListener("change", onChange)

    // Initial sync in case the breakpoint crossed between the
    // useState initializer and effect mount.
    onChange()

    return () => {
      window.removeEventListener("resize", onChange)
      ro.disconnect()
      mql.removeEventListener("change", onChange)
    }
  }, [])

  return isMobile
}
