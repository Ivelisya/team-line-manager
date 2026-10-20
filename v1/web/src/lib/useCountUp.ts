import { useEffect, useRef, useState } from 'react';

/** 数值变化时平滑过渡（KPI 用）。首次渲染直接显示目标值，不从 0 数起；减少动效偏好下直接跳变。 */
export function useCountUp(target: number, ms = 450): number {
  const [shown, setShown] = useState(target);
  const from = useRef(target);
  const raf = useRef(0);
  useEffect(() => {
    if (!Number.isFinite(target)) { setShown(target); from.current = target; return; }
    if (from.current === target) return;
    const reduce = typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;
    if (reduce) { from.current = target; setShown(target); return; }
    const a = from.current; const t0 = performance.now();
    const step = (t: number) => {
      const p = Math.min(1, (t - t0) / ms);
      const eased = 1 - (1 - p) ** 3;
      const cur = a + (target - a) * eased;
      from.current = cur; setShown(cur);
      if (p < 1) raf.current = requestAnimationFrame(step); else from.current = target;
    };
    raf.current = requestAnimationFrame(step);
    return () => cancelAnimationFrame(raf.current);
  }, [target, ms]);
  return shown;
}
