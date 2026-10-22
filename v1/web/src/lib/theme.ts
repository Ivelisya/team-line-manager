// 主题（light/dark/system）与配色（plum 粉紫默认 / indigo 原版靛蓝）。落在 <html data-theme data-palette>，存 localStorage。
import { useCallback, useEffect, useState } from 'react';
import { prefs } from './prefs';

export type ThemePref = 'light' | 'dark' | 'system';
export type Palette = 'plum' | 'indigo';
const read = (k: string, d: string) => prefs.get(k) ?? d;
const mq = matchMedia('(prefers-color-scheme: dark)');
/** 手机浏览器的地址栏 / 状态栏跟顶栏同色（<meta name="theme-color">），深浅色一换就跟着换 */
function syncThemeColor() {
  let meta = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]');
  if (!meta) { meta = document.createElement('meta'); meta.name = 'theme-color'; document.head.append(meta); }
  meta.content = getComputedStyle(document.documentElement).getPropertyValue('--surface').trim();
}
const applyTheme = (p: ThemePref) => { document.documentElement.dataset.theme = p === 'dark' || (p === 'system' && mq.matches) ? 'dark' : 'light'; syncThemeColor(); };
const applyPalette = (p: Palette) => { document.documentElement.dataset.palette = p; };

/** 用户切换主题 / 配色（首次挂载不走这里）：切换那一帧给 <html> 加 data-theme-swap 关掉全部过渡与动画（base.css），
 *  强制算一次样式让新颜色在“无过渡”下落定，下一帧再去掉——整页同时换完，不再一块一块地变。
 *  支持 View Transitions 且没开减弱动效时，外面再包一层整页交叉淡入。 */
function swap(change: () => void) {
  const root = document.documentElement;
  const run = () => {
    root.setAttribute('data-theme-swap', '');
    change();
    void root.offsetHeight; // 强制算一次样式：新颜色在关掉过渡的状态下落定，下一帧去掉属性时没有可过渡的变化
    requestAnimationFrame(() => root.removeAttribute('data-theme-swap'));
  };
  if (document.startViewTransition && !matchMedia('(prefers-reduced-motion: reduce)').matches) document.startViewTransition(run);
  else run();
}

// 跟随系统时，系统深浅色变化跟着换（首屏的解析在 index.html 的内联脚本里）。挂在模块上：登录页没有 useTheme 也要跟
mq.addEventListener('change', () => { if (read('fleet:theme', 'system') === 'system') swap(() => applyTheme('system')); });

export function useTheme() {
  const [pref, setPrefState] = useState<ThemePref>(() => read('fleet:theme', 'system') as ThemePref);
  const [palette, setPaletteState] = useState<Palette>(() => read('fleet:palette', 'plum') as Palette);
  // 首次挂载按读到的偏好对齐一次（不走 swap）
  useEffect(() => { applyTheme(pref); applyPalette(palette); }, []); // eslint-disable-line react-hooks/exhaustive-deps
  // 用户切换：<html> 上的属性只在 swap 里改。不能放进依赖 pref 的 effect：点击引起的 effect 会在交叉淡入截取旧画面之前同步跑掉
  const setPref = useCallback((p: ThemePref) => {
    swap(() => applyTheme(p));
    setPrefState(p);
    prefs.set('fleet:theme', p);
  }, []);
  const setPalette = useCallback((p: Palette) => {
    swap(() => applyPalette(p));
    setPaletteState(p);
    prefs.set('fleet:palette', p);
  }, []);
  return { pref, setPref, palette, setPalette };
}
