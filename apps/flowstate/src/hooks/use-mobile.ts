// `isMobile` is intentionally hardcoded to `false`. The sidebar used to
// fork between an overlay (Sheet) and a push/slide layout based on a
// 768px media query, but that boundary was fragile under zoom — the
// breakpoint flipped mid-toggle and the trigger button routed to a
// state branch whose element was CSS-hidden, producing a silent click.
// We now always render the push/slide sidebar regardless of viewport,
// so this hook is a constant. Kept as a hook (rather than inlined
// `false`) so all existing call sites keep working without churn.
export function useIsMobile() {
  return false
}
