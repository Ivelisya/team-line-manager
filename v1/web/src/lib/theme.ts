// 主题（light/dark/system）与配色（plum 粉紫默认 / indigo 原版靛蓝）。落在 <html data-theme data-palette>，存 localStorage。
import { useCallback, useEffect, useState } from 'react';

export type ThemePref = 'light' | 'dark' | 'system';
export type Palette = 'plum' | 'indigo';
const read = (k: string, d: string) => { try { return localStorage.getItem(k) ?? d; } catch { return d; } };

export function useTheme() {
  const [pref, setPref] = useState<ThemePref>(() => read('fleet:theme', 'system') as ThemePref);
  const [palette, setPalette] = useState<Palette>(() => read('fleet:palette', 'plum') as Palette);
  const apply = useCallback((p: ThemePref) => {
    const dark = p === 'dark' || (p === 'system' && matchMedia('(prefers-color-scheme: dark)').matches);
    document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  }, []);
  useEffect(() => {
    apply(pref);
    try { localStorage.setItem('fleet:theme', pref); } catch { /* 隐私模式 */ }
  }, [pref, apply]);
  useEffect(() => {
    document.documentElement.dataset.palette = palette;
    try { localStorage.setItem('fleet:palette', palette); } catch { /* 隐私模式 */ }
  }, [palette]);
  return { pref, setPref, palette, setPalette };
}
