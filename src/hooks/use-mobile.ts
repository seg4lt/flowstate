import * as React from "react"

const MOBILE_BREAKPOINT = 768

// Initial value must be synchronous — not `undefined → effect → flip` —
// otherwise the very first `toggleSidebar()` call after mount reads
// `isMobile: false` and routes to the wrong state branch (desktop
// `open` instead of mobile `openMobile`). Tauri is not SSR, so reading
// `window` during the initial state callback is safe.
function readInitial(): boolean {
  if (typeof window === "undefined") return false
  return window.innerWidth < MOBILE_BREAKPOINT
}

export function useIsMobile() {
  const [isMobile, setIsMobile] = React.useState<boolean>(readInitial)

  React.useEffect(() => {
    const mql = window.matchMedia(`(max-width: ${MOBILE_BREAKPOINT - 1}px)`)
    const onChange = () => {
      setIsMobile(window.innerWidth < MOBILE_BREAKPOINT)
    }
    mql.addEventListener("change", onChange)
    // Re-sync in case the width crossed the breakpoint between the
    // initial state read and the effect mount (e.g. a window resize
    // fired during the first render).
    setIsMobile(window.innerWidth < MOBILE_BREAKPOINT)
    return () => mql.removeEventListener("change", onChange)
  }, [])

  return isMobile
}
