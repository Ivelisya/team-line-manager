import { clsx, type ClassValue } from 'clsx';
import { extendTailwindMerge } from 'tailwind-merge';

// tailwind-merge：同组的 Tailwind 工具类后写的盖掉先写的（cn('h-md', 'h-sm') 只留 h-sm），不同组的都留。
// 我们在 styles/theme.css 里登记了自定义字号、圆角、间距、高度、字体：这里同样告诉它，不然 text-body 会被当成颜色、h-md 当成不认识的类。
// 不认识的类名（手写组件类：btn、menu-item、dt…）原样保留，不参与去重。
const twMerge = extendTailwindMerge({
  extend: {
    theme: {
      // text-micro / text-body …（= 令牌字阶）：属于字号组，和 text-fg（颜色）并存
      text: ['micro', 'label', 'body', 'emph', 'title', 'section', 'page', 'kpi'],
      // rounded-card / float / control / pill / inner / section / tag（xs 在 tailwind-merge 里本来就有）
      radius: ['card', 'float', 'control', 'pill', 'inner', 'section', 'tag'],
      // h-chip / h-sm / h-md / h-lg / h-bar、p-card-pad、gap-gutter（--height-*、--spacing-*）
      spacing: ['chip', 'sm', 'md', 'lg', 'bar', 'card-pad', 'gutter'],
      // font-num（theme.css 里的 @utility）
      font: ['num'],
    },
  },
});

export const cn = (...inputs: ClassValue[]) => twMerge(clsx(inputs));

/** 程序触发的滚动（scrollTo / scrollIntoView）：系统要求减少动态效果时直接跳，否则平滑（MotionConfig 管不到原生滚动） */
export const scrollBehavior = (): ScrollBehavior => (matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth');
