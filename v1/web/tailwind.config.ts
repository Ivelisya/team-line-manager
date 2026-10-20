import type { Config } from 'tailwindcss';

// 颜色、圆角、字号全部指向 tokens.css 里的 CSS 变量（移植自原版）
const v = (n: string) => `var(--${n})`;

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
        accent: v('accent'), 'accent-solid': v('accent-solid'), 'accent-soft': v('accent-soft'),
        'accent-soft-fg': v('accent-soft-fg'), 'on-accent': v('on-accent'),
        ok: v('ok'), 'ok-text': v('ok-text'), 'ok-bg': v('ok-bg'),
        warn: v('warn'), 'warn-text': v('warn-text'), 'warn-bg': v('warn-bg'),
        bad: v('bad'), 'bad-text': v('bad-text'), 'bad-bg': v('bad-bg'),
        info: v('info'), 'info-text': v('info-text'), 'info-bg': v('info-bg'),
        neutral: v('neutral'), 'neutral-text': v('neutral-text'), 'neutral-bg': v('neutral-bg'),
        // shadcn 约定名
        background: v('canvas'), foreground: v('fg'), card: v('surface'), border: v('line'),
        input: v('line-strong'), ring: v('focus'), muted: v('sunken'), 'muted-foreground': v('fg-2'),
        primary: v('accent-solid'), 'primary-foreground': v('on-accent'),
        destructive: v('bad'), popover: v('raised'),
      },
      borderRadius: { card: v('r-card'), float: v('r-float'), control: v('r-control'), pill: v('r-pill'), xs: v('r-xs') },
      fontFamily: { sans: [v('font-sans')], num: [v('font-num')], mono: [v('font-mono')] },
      fontSize: {
        micro: ['11px', '16px'], label: ['12px', '18px'], body: ['13px', '20px'],
        emph: ['15px', '22px'], title: ['16px', '22px'], page: ['24px', '32px'], kpi: ['27px', '32px'],
      },
      spacing: { 'card-pad': v('card-pad'), gutter: v('gutter') },
      height: { chip: v('h-chip'), sm: v('h-sm'), md: v('h-md'), lg: v('h-lg'), bar: v('h-bar') },
      boxShadow: { float: v('shadow-float'), thumb: v('shadow-thumb'), btn: v('shadow-btn') },
      zIndex: { sticky: '10', pagehead: '20', topbar: '30', drawer: '40', popover: '50', dialog: '60', toast: '70', tooltip: '80' },
      transitionDuration: { fast: '120ms', base: '180ms', panel: '220ms' },
    },
  },
  plugins: [],
} satisfies Config;
