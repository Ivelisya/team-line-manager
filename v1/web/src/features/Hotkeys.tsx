// 全局快捷键：⌘K / Ctrl+K（或 /）命令面板，[ ] 在抽屉里切上一台/下一台，? 快捷键帮助。输入框里不触发。
import * as React from 'react';
import { Search } from 'lucide-react';
import { useMachines } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { modKey, setOverlay, useOverlay } from '@/lib/palette';
import type { ThemePref } from '@/lib/theme';
import { Dialog, DialogContent, DialogDescription, DialogTitle } from '@/components/ui/dialog';

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
  const { id, tab, open } = useDrawer();
  const machines = useMachines('live').data;
  const [loaded, setLoaded] = React.useState(false);
  React.useEffect(() => { if (overlay === 'palette') setLoaded(true); }, [overlay]);

  // 事件处理里读最新值，避免每次变化都重新绑定监听
  const ref = React.useRef({ id, tab, open, machines, overlay });
  ref.current = { id, tab, open, machines, overlay };
  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const c = ref.current;
      if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === 'k') { e.preventDefault(); setOverlay(c.overlay === 'palette' ? null : 'palette'); return; }
      if (e.metaKey || e.ctrlKey || e.altKey || typing(e.target) || c.overlay) return;
      if (document.querySelector('.dialog, .cmd-dialog')) return; // 有对话框开着时不抢键
      if (e.key === '/' ) { e.preventDefault(); setOverlay('palette'); }
      else if (e.key === '?') { e.preventDefault(); setOverlay('help'); }
      else if ((e.key === '[' || e.key === ']') && c.id && c.machines) {
        const i = c.machines.findIndex((m) => m.id === c.id);
        if (i < 0) return;
        const next = c.machines[(i + (e.key === ']' ? 1 : -1) + c.machines.length) % c.machines.length];
        e.preventDefault();
        c.open(next.id, c.tab === 'overview' ? undefined : c.tab);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  return (
    <>
      {loaded && <React.Suspense fallback={null}><CommandPalette theme={theme} /></React.Suspense>}
      <ShortcutsHelp open={overlay === 'help'} onOpenChange={(o) => setOverlay(o ? 'help' : null)} />
    </>
  );
}

const MOD = modKey();
const KEYS: { keys: string[]; text: string }[] = [
  { keys: [MOD, 'K'], text: '打开命令面板：跳转页面、搜索机器、常用动作' },
  { keys: ['/'], text: '同上（不在输入框里时）' },
  { keys: ['['], text: '抽屉打开时，切到上一台机器' },
  { keys: [']'], text: '抽屉打开时，切到下一台机器' },
  { keys: ['?'], text: '打开这份快捷键帮助' },
  { keys: ['Esc'], text: '关闭抽屉、对话框和面板' },
  { keys: ['Enter'], text: '在节点表格里，打开当前行的详情' },
];

function ShortcutsHelp({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <div style={{ display: 'grid', gap: 6 }}><DialogTitle>键盘快捷键</DialogTitle><DialogDescription>在节点卡片或表格行上点右键，可以直接打开菜单：复制 IP、升级 agent、新建静默、退役。</DialogDescription></div>
        <ul className="hk-list">
          {KEYS.map((k) => (
            <li key={k.text}><span className="hk-keys">{k.keys.map((x, i) => <React.Fragment key={x}>{i > 0 && <span className="t-muted">+</span>}<kbd className="kbd">{x}</kbd></React.Fragment>)}</span><span>{k.text}</span></li>
          ))}
        </ul>
      </DialogContent>
    </Dialog>
  );
}
