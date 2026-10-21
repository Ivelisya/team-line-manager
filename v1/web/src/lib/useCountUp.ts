import { useEffect, useRef, useState } from 'react';
import { durMs } from './motion';

/** 数值变化时平滑过渡（KPI 的台数与流量用；网卡速率这类每 3 秒都变的读数不滚，每条推送都要逐帧重画），时长读动效令牌 --dur-data。首次渲染直接显示目标值，不从 0 数起；
 *  减少动效偏好下令牌为 0，直接跳变。毛玻璃遮罩开着（<html data-overlay>，见 dialog.tsx）时也直接跳变：KPI 在遮罩下面，逐帧滚动会让 GPU 每帧重做全屏模糊 */
export function useCountUp(target: number): number {
  const [shown, setShown] = useState(target);
  const from = useRef(target);
  const raf = useRef(0);
  useEffect(() => {
    if (!Number.isFinite(target)) { setShown(target); from.current = target; return; }
    if (from.current === target) return;
    // 之前没有有效值（数据刚到）：直接显示，不从 0 数起
    if (!Number.isFinite(from.current)) { from.current = target; setShown(target); return; }
    const ms = durMs('--dur-data');
    if (!(ms > 0) || document.documentElement.hasAttribute('data-overlay')) { from.current = target; setShown(target); return; }
    // 起点取第一帧的时间戳：rAF 给的是帧开始时刻，可能早于这里的 performance.now()，用它当起点 p 会为负，缓动后反向冲出离谱值
    const a = from.current; let t0 = -1;
    const step = (t: number) => {
      if (t0 < 0) t0 = t;
      const p = Math.min(1, (t - t0) / ms);
      const eased = 1 - (1 - p) ** 3;
      const cur = a + (target - a) * eased;
      from.current = cur; setShown(cur);
      if (p < 1) raf.current = requestAnimationFrame(step); else from.current = target;
    };
    raf.current = requestAnimationFrame(step);
    return () => cancelAnimationFrame(raf.current);
  }, [target]);
  return shown;
}
