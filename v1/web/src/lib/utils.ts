import { clsx } from 'clsx';

// 组件的基础类都是自定义类（btn、menu-item…），调用处只补少量宽度类，不会和基础类冲突：用不着 tailwind-merge
export const cn = clsx;

/** 程序触发的滚动（scrollTo / scrollIntoView）：系统要求减少动态效果时直接跳，否则平滑（MotionConfig 管不到原生滚动） */
export const scrollBehavior = (): ScrollBehavior => (matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth');
