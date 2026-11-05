import { useEffect, useRef } from "react";

/**
 * Clear the hover tooltip whenever the page or any scroller (a drawer body,
 * via the capturing window listener) scrolls, however the tooltip was shown
 * (mouse move, touch scrub, or the emulated mouse move after a tap). The
 * tooltip follows the pointer, so when content scrolls under a still pointer it
 * would otherwise stay where it was. Calls `clear` directly: no synthetic DOM
 * events, so nothing else in the React tree sees a fake mouseout.
 */
export function useClearTooltipOnScroll(clear: () => void) {
  const clearRef = useRef(clear);
  clearRef.current = clear;
  useEffect(() => {
    const onScroll = () => clearRef.current();
    window.addEventListener("scroll", onScroll, { capture: true, passive: true });
    return () => window.removeEventListener("scroll", onScroll, { capture: true });
  }, []);
}
