// 命令面板 / 快捷键帮助的开关：顶栏按钮、键盘快捷键、面板内都能控制，所以放在模块级的小仓库里。
import { createStore } from './store';

const overlay = createStore<'palette' | 'help' | null>(null);
export const setOverlay = overlay.set;
export const useOverlay = overlay.use;

const isMac = () => typeof navigator !== 'undefined' && /Mac|iPhone|iPad/i.test(navigator.platform || navigator.userAgent);
export const modKey = () => (isMac() ? '⌘' : 'Ctrl');
