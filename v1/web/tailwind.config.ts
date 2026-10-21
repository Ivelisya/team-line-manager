import type { Config } from 'tailwindcss';

// 颜色、圆角、字号全部指向 tokens.css 里的 CSS 变量（移植自原版）
const v = (n: string) => `var(--${n})`;
// shadcn 角色色（变量见 styles/theme.css）：用 color-mix 接 <alpha-value>，bg-primary/90 这类透明度修饰才生效
const s = (n: string) => `color-mix(in srgb, var(--${n}) calc(<alpha-value> * 100%), transparent)`;

export default {
  darkMode: ['selector', '[data-theme="dark"]'],
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        canvas: v('canvas'), surface: v('surface'), raised: v('raised'), sunken: v('sunken'),
        hover: v('hover'), track: v('track'),
        line: v('line'), 'line-strong': v('line-strong'),
        fg: v('fg'), 'fg-2': v('fg-2'), 'fg-3': v('fg-3'),
        // 强调色（--accent）叫 brand：Tailwind 的 accent 留给 shadcn 角色
        brand: v('accent'), 'accent-solid': v('accent-solid'), 'accent-soft': v('accent-soft'),
        'accent-soft-fg': v('accent-soft-fg'), 'on-accent': v('on-accent'),
        ok: v('ok'), 'ok-text': v('ok-text'), 'ok-bg': v('ok-bg'),
        warn: v('warn'), 'warn-text': v('warn-text'), 'warn-bg': v('warn-bg'),
        bad: v('bad'), 'bad-text': v('bad-text'), 'bad-bg': v('bad-bg'),
        info: v('info'), 'info-text': v('info-text'), 'info-bg': v('info-bg'),
        neutral: v('neutral'), 'neutral-text': v('neutral-text'), 'neutral-bg': v('neutral-bg'),
        // shadcn 约定名
        background: s('background'), foreground: s('foreground'), border: s('border'), input: s('input'), ring: s('ring'),
        card: { DEFAULT: s('card'), foreground: s('card-foreground') },
        popover: { DEFAULT: s('popover'), foreground: s('popover-foreground') },
        primary: { DEFAULT: s('primary'), foreground: s('primary-foreground') },
        secondary: { DEFAULT: s('secondary'), foreground: s('secondary-foreground') },
        muted: { DEFAULT: s('muted'), foreground: s('muted-foreground') },
        accent: { DEFAULT: s('ui-accent'), foreground: s('accent-foreground') },
        destructive: { DEFAULT: s('destructive'), foreground: s('destructive-foreground') },
        chart: { 1: v('chart-1'), 2: v('chart-2'), 3: v('chart-3'), 4: v('chart-4'), 5: v('chart-5') },
      },
      borderRadius: {
        card: v('r-card'), float: v('r-float'), control: v('r-control'), pill: v('r-pill'), xs: v('r-xs'),
        lg: v('radius'), md: 'calc(var(--radius) - 2px)', sm: 'calc(var(--radius) - 4px)',
      },
      fontFamily: { sans: [v('font-sans')], num: [v('font-num')], mono: [v('font-mono')] },
      fontSize: {
        micro: ['11px', '16px'], label: ['12px', '18px'], body: ['13px', '20px'],
        emph: ['15px', '22px'], title: ['16px', '22px'], page: ['24px', '32px'], kpi: ['27px', '32px'],
      },
      spacing: { 'card-pad': v('card-pad'), gutter: v('gutter') },
      height: { chip: v('h-chip'), sm: v('h-sm'), md: v('h-md'), lg: v('h-lg'), bar: v('h-bar') },
    },
  },
  plugins: [],
} satisfies Config;
