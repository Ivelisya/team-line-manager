import { type RefObject, useEffect } from "react";

/**
 * Hide the hover tooltip while the page or a drawer scrolls. The tooltip follows
 * the pointer; when the content scrolls under a still pointer no mouse event
 * fires and the tooltip would stay where it was, so dispatch a `mouseout` on the
 * chart's interaction layer (the first `<g>` under the `<svg>`) that React reads
 * as "pointer left".
 */
export function useHideTooltipOnScroll(ref: RefObject<HTMLElement | null>) {
  useEffect(() => {
    const el = ref.current;
    if (!el) {
      return;
    }
    let inside = false;
    const enter = () => {
      inside = true;
    };
    const leave = () => {
      inside = false;
    };
    const hide = () => {
      if (!inside) {
        return;
      }
      inside = false;
      el.querySelector("svg > g")?.dispatchEvent(
        new MouseEvent("mouseout", { bubbles: true })
      );
    };
    el.addEventListener("pointermove", enter);
    el.addEventListener("pointerleave", leave);
    window.addEventListener("scroll", hide, { capture: true, passive: true });
    return () => {
      el.removeEventListener("pointermove", enter);
      el.removeEventListener("pointerleave", leave);
      window.removeEventListener("scroll", hide, { capture: true });
    };
  }, [ref]);
}
