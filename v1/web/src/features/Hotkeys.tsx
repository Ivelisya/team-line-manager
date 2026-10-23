// 全局快捷键：⌘K / Ctrl+K（或 /）命令面板，[ ] 在抽屉里切上一台/下一台，? 快捷键帮助。输入框里不触发。
import * as React from 'react';
import { Search, X } from 'lucide-react';
import { useQueryClient } from '@tanstack/react-query';
import { drawerSiblings, useDrawer } from '@/lib/drawer';
import { modKey, setOverlay, useOverlay } from '@/lib/palette';
import type { ThemePref } from '@/lib/theme';
import { Dialog, DialogContent, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { IconButton } from '@/components/ui/button';

const CommandPalette = React.lazy(() => import('./CommandPalette'));

const typing = (t: EventTarget | null) => {
  const el = t as HTMLElement | null;
  return !!el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT' || el.isContentEditable);
};

/** 顶栏的搜索入口：“搜索…  ⌘K” */
export function PaletteTrigger() {
  // 鼠标移上来就预热面板代码，点开时不用等
  const warm = () => { void import('./CommandPalette'); };
  return (
    <button type="button" className="cmd-trigger" aria-label="打开命令面板" aria-keyshortcuts="Control+K Meta+K" onClick={() => setOverlay('palette')} onPointerEnter={warm} onFocus={warm}>
      <Search size={14} /><span className="cmd-trigger-t">搜索…</span><span className="cmd-trigger-k"><kbd className="kbd">{modKey()}</kbd><kbd className="kbd">K</kbd></span>
    </button>
  );
}

export function Hotkeys({ theme }: { theme: { pref: ThemePref; setPref: (p: ThemePref) => void } }) {
  const overlay = useOverlay();
  const { id, switchTo } = useDrawer();
  // 不订阅机器列表（订阅会让每个页面都跟着取 /machines）：按 [ ] 时才读缓存
  const qc = useQueryClient();
  const [loaded, setLoaded] = React.useState(false);
  React.useEffect(() => { if (overlay === 'palette') setLoaded(true); }, [overlay]);

  // 事件处理里读最新值，避免每次变化都重新绑定监听
  const ref = React.useRef({ id, switchTo, overlay });
  ref.current = { id, switchTo, overlay };
  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const c = ref.current;
      if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === 'k') { e.preventDefault(); setOverlay(c.overlay === 'palette' ? null : 'palette'); return; }
      if (e.metaKey || e.ctrlKey || e.altKey || typing(e.target) || c.overlay) return;
      if (document.querySelector('.dialog, .cmd-dialog')) return; // 有对话框开着时不抢键
      if (e.key === '/' ) { e.preventDefault(); setOverlay('palette'); }
      else if (e.key === '?') { e.preventDefault(); setOverlay('help'); }
      else if ((e.key === '[' || e.key === ']') && c.id) {
        // 到头不绕回：同抽屉头部到头时禁用的上一台 / 下一台
        const s = drawerSiblings(qc, c.id);
        const to = s && (e.key === ']' ? s.next : s.prev);
        if (!to) return;
        e.preventDefault();
        c.switchTo(to);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [qc]);

  return (
    <>
      {loaded && <React.Suspense fallback={null}><CommandPalette theme={theme} /></React.Suspense>}
      <ShortcutsHelp open={overlay === 'help'} onOpenChange={(o) => setOverlay(o ? 'help' : null)} />
    </>
  );
}

const MOD = modKey();
// 说明压成短词；在哪儿生效（抽屉里、表格里）放悬停
// keys：按法，写多组的是等价的几种（用「或」连起来）；每组里用 + 连
const KEYS: { keys: string[][]; text: string; tip?: string }[] = [
  { keys: [[MOD, 'K'], ['/']], text: '命令面板', tip: '跳转页面、搜机器、执行常用操作。不在输入框里时，也可以按 /' },
  { keys: [['[']], text: '上一台', tip: '抽屉打开时' },
  { keys: [[']']], text: '下一台', tip: '抽屉打开时' },
  { keys: [['?']], text: '快捷键帮助' },
  { keys: [['Esc']], text: '关闭浮层', tip: '抽屉、对话框、面板' },
  { keys: [['Enter']], text: '打开当前行', tip: '机器表格里' },
  { keys: [['右键']], text: '更多操作', tip: '卡片、表格行' },
];

function ShortcutsHelp({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent aria-describedby={undefined}>
        <DialogHeader className="hk-head"><DialogTitle>键盘快捷键</DialogTitle>
          <IconButton label="关闭" tip={false} onClick={() => onOpenChange(false)}><X size={16} /></IconButton></DialogHeader>
        <ul className="hk-list">
          {KEYS.map((k) => (
            <li key={k.text} data-tip={k.tip}>
              <span className="hk-keys">{k.keys.map((combo, n) => (
                <React.Fragment key={combo.join()}>
                  {n > 0 && <span className="t-muted">或</span>}
                  {combo.map((x, i) => <React.Fragment key={x}>{i > 0 && <span className="t-muted">+</span>}<kbd className="kbd">{x}</kbd></React.Fragment>)}
                </React.Fragment>
              ))}</span><span>{k.text}</span>
            </li>
          ))}
        </ul>
      </DialogContent>
    </Dialog>
  );
}
