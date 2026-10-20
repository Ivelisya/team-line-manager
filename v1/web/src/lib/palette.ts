// 命令面板 / 快捷键帮助的开关：顶栏按钮、键盘快捷键、面板内都能控制，所以放在模块级的小仓库里。
import { useSyncExternalStore } from 'react';

type Overlay = 'palette' | 'help' | null;
let current: Overlay = null;
const subs = new Set<() => void>();
const subscribe = (f: () => void) => { subs.add(f); return () => { subs.delete(f); }; };
export const setOverlay = (o: Overlay) => { if (o !== current) { current = o; subs.forEach((f) => f()); } };
export const useOverlay = () => useSyncExternalStore(subscribe, () => current, () => null);

export const isMac = () => typeof navigator !== 'undefined' && /Mac|iPhone|iPad/i.test(navigator.platform || navigator.userAgent);
export const modKey = () => (isMac() ? '⌘' : 'Ctrl');
