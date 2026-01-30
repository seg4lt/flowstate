export const AUTO_SCROLL_BOTTOM_THRESHOLD_PX = 64;

export function isScrollContainerNearBottom(
  element: HTMLElement | null,
  thresholdPx: number = AUTO_SCROLL_BOTTOM_THRESHOLD_PX,
): boolean {
  if (!element) return true;
  const distance = element.scrollHeight - element.scrollTop - element.clientHeight;
  return distance <= thresholdPx;
}
