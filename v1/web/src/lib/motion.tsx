// 动效基础设施：motion（framer-motion 的继任者）只用在两处——分段控件/页签的滑动指示器、首页卡片的重排。
// 其余浮层的进出场仍走 CSS（tokens.css §15 的动效令牌）。
import * as React from 'react';
import { LazyMotion, MotionConfig } from 'motion/react';

// 布局动画（layout / layoutId）需要 domMax 特性包（约 88 KB）；用异步加载放到单独的块里，不占首屏主包，
// 而且等浏览器空闲了再取（不支持 requestIdleCallback 的 Safari 等 3 秒）：登录后首屏的脚本与数据请求不和它抢。
// 特性包到位之前 m.* 元素按静态元素渲染（指示器已在正确位置，只是不带动画），所以不会闪也不会错位；
// 也因此 m.* 上不写 initial / animate 这类进场动画（到位之前会一直停在 initial），卡片的淡入走 CSS。
const idle = () => new Promise<void>((r) => { if ('requestIdleCallback' in window) requestIdleCallback(() => r()); else setTimeout(r, 3000); });
const loadFeatures = () => idle().then(() => import('./motionFeatures')).then((x) => x.default);

/** 根部提供者：reducedMotion="user" 让系统“减少动态效果”时位移/布局动画直接跳到终点 */
export function MotionProvider({ children }: { children: React.ReactNode }) {
  return <MotionConfig reducedMotion="user"><LazyMotion features={loadFeatures} strict>{children}</LazyMotion></MotionConfig>;
}

/** 指示器与卡片重排共用的弹簧：轻微回弹、约 0.35s */
export const SPRING = { type: 'spring', bounce: 0.18, duration: 0.35 } as const;

/** 读动效时长令牌（tokens.css §15），换算成毫秒。生产构建压缩 CSS 时会把 450ms 写成 .45s（更短就换单位），不能直接 parseFloat 当毫秒用 */
export function durMs(token: string): number {
  const v = getComputedStyle(document.documentElement).getPropertyValue(token).trim();
  const n = parseFloat(v);
  return v.endsWith('ms') ? n : v.endsWith('s') ? n * 1000 : n;
}
