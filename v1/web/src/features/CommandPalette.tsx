// ⌘K / Ctrl+K 命令面板：跳转页面、搜索机器（名称 / IP / 团队模糊匹配，回车打开抽屉）、常用动作。
// 整个文件按需加载（首次打开时才下载 cmdk）。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import { ArrowUpCircle, BellOff, Clipboard, CloudUpload, Moon, PackageOpen, ShieldCheck, Sun } from 'lucide-react';
import { NAV, type NavItem } from '@/lib/nav';
import { useMachineLabels, useUpgradeAllAgents } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { copyText } from '@/lib/clipboard';
import { KIND_LABEL } from '@/lib/sort';
import { setOverlay, useOverlay } from '@/lib/palette';
import { openEvacuate } from '@/lib/nodeActions';
import type { ThemePref } from '@/lib/theme';
import { Command, CommandDialog, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog } from '@/components/app/dialogs';
import { HealthPill, KindIcon } from '@/components/app/primitives';

// 与侧栏同一份导航，在「访问监控」后加上「访问规则」（侧栏里它是访问监控页头的视图切换）
const POLICY: NavItem = { to: '/access/policy', label: '访问规则', icon: ShieldCheck, kw: 'policy access 访问规则 策略 白名单 黑名单 阻止 域名 allowlist denylist' };
const PAGES = NAV.flatMap((n) => (n.to === '/access' ? [n, POLICY] : [n]));
const LIMIT = 8; // 不输入时只列前几台，输入后在全部机器里搜

/** 编号去掉前导零：团队1-04 → 团队1-4，让「1-4」也能按名称命中它 */
const unpad = (s: string) => s.replace(/\d+/g, (d) => d.replace(/^0+(?=\d)/, ''));
const subseq = (s: string, q: string) => { const t = [...q]; let i = 0; for (const c of s) if (c === t[i]) i++; return i >= t.length; };
/** 匹配档位（大的在前）：名称相同 > 名称前缀 > 名称包含 > 去前导零后名称包含 > IP 等其它字段包含 > 名称模糊。
 *  模糊（子序列）只对名称：IP、UUID 里数字多，短数字串对它们做子序列几乎条条命中。
 *  条目 value 写成「名称\t其它字段」；没有 \t 的（页面、动作）整串当名称 */
function matchTier(value: string, q: string): number {
  const [name, rest = ''] = value.toLowerCase().split('\t');
  if (name === q) return 7;
  if (name.startsWith(q)) return 6;
  if (name.includes(q)) return 5;
  if (unpad(name).includes(unpad(q))) return 4;
  if (rest.includes(q)) return 3;
  return subseq(name, q) ? 2 : 0;
}

type Theme = { pref: ThemePref; setPref: (p: ThemePref) => void };
const close = () => setOverlay(null);

export default function CommandPalette({ theme }: { theme: Theme }) {
  const open = useOverlay() === 'palette';
  const upgradeAll = useUpgradeAllAgents();
  const [askUpgrade, setAskUpgrade] = React.useState(false);
  return (
    <>
      {/* 面板内容只在打开时挂载：关着时不订阅机器列表，实时推送不会让它重排 */}
      <CommandDialog open={open} onOpenChange={(o) => setOverlay(o ? 'palette' : null)} title="命令面板">
        <PaletteBody theme={theme} askUpgrade={() => setAskUpgrade(true)} />
      </CommandDialog>

      <ConfirmDialog open={askUpgrade} onOpenChange={setAskUpgrade} title="升级全部 agent？" confirmLabel="全部升级"
        description="先升一台做金丝雀（有线路机先挑线路机），它成功后线路机逐台升级，线路机都成功后落地机并发；金丝雀或任何一台线路机失败，没开始的都会取消。新版本起不来或 2 分钟内没有上报，会自动换回旧版本。已有进行中作业的机器会跳过；升级时 agent 会重启，不影响线路上的连接。"
        onConfirm={async () => { const r = await upgradeAll.mutateAsync(); toast.ok(`已提交 ${r.jobs.length} 个升级作业：${r.order}`); }} />
    </>
  );
}

function PaletteBody({ theme, askUpgrade }: { theme: Theme; askUpgrade: () => void }) {
  const nav = useNavigate();
  const { open: openDrawer } = useDrawer();
  // 只取面板用到的字段：开着面板时实时推送只改读数，不会让整表重渲染
  const machines = useMachineLabels().data ?? [];
  const [search, setSearch] = React.useState('');
  const [mode, setMode] = React.useState<'root' | 'copy-ip' | 'evacuate'>('root');
  const go = (to: string) => { close(); nav(to); };
  const dark = document.documentElement.dataset.theme === 'dark';

  const pool = mode === 'evacuate' ? machines.filter((m) => m.kind === 'relay') : machines;
  const list = search.trim() || mode === 'evacuate' ? pool : pool.slice(0, LIMIT);
  const value = (m: (typeof list)[number]) => `${m.name}\t${m.ip} ${m.team?.name ?? ''} ${KIND_LABEL[m.kind]}`;
  // cmdk 按分数排序、同分保留上一次排完的 DOM 顺序：加一个小于档差的位次分，同档始终按默认机器顺序
  const order = new Map(list.map((m, i) => [value(m), i]));
  const filter = (v: string, s: string, kw?: string[]) => {
    const t = matchTier(kw?.length ? `${v} ${kw.join(' ')}` : v, s.trim().toLowerCase());
    const i = order.get(v);
    return t === 0 ? 0 : t + (i === undefined ? 0 : 0.5 * (1 - i / list.length));
  };
  const machineItems = list.map((m) => (
    <CommandItem key={m.id} value={value(m)} keywords={[m.id]}
      onSelect={() => { close(); if (mode === 'copy-ip') void copyText(m.ip, ' IP'); else if (mode === 'evacuate') openEvacuate(m.id); else openDrawer(m.id); }}>
      <KindIcon kind={m.kind} />
      <span className="cmd-main"><b className="ellipsis">{m.name}</b><HealthPill m={m} /></span>
      <span className="cmd-aside">{mode === 'copy-ip' ? <>复制 <span className="num">{m.ip}</span></> : mode === 'evacuate' ? <span>搬空…</span> : <span className="num">{m.ip}</span>}</span>
    </CommandItem>
  ));

  return (
    <Command label="命令面板" loop filter={filter}
      onKeyDown={(e) => { if (e.key === 'Backspace' && !search && mode !== 'root') { e.preventDefault(); setMode('root'); } }}>
      <CommandInput autoFocus value={search} onValueChange={setSearch} placeholder={mode === 'copy-ip' ? '选择要复制 IP 的机器…' : mode === 'evacuate' ? '选择要搬空的线路机…' : '搜索机器、页面或动作…'} />
      <CommandList>
        <CommandEmpty>没有匹配的结果{search ? `：${search}` : ''}</CommandEmpty>
        {mode === 'root' && (
          <>
            <CommandGroup heading="跳转页面">
              {PAGES.map((p) => (
                <CommandItem key={p.to} value={`${p.label}\t${p.kw}`} onSelect={() => go(p.to)}><p.icon /><span className="cmd-main">{p.label}</span></CommandItem>
              ))}
            </CommandGroup>
            <CommandGroup heading="动作">
              <CommandItem value="上架机器 provision 添加机器 落地机 家宽代理 线路机" onSelect={() => go('/provision')}><CloudUpload /><span className="cmd-main">上架机器</span></CommandItem>
              <CommandItem value="新建静默 silence 维护窗口" onSelect={() => go('/incidents?silence=new')}><BellOff /><span className="cmd-main">新建静默</span></CommandItem>
              <CommandItem value="复制某机器 IP copy ip" onSelect={() => { setSearch(''); setMode('copy-ip'); }}><Clipboard /><span className="cmd-main">复制某台机器的 IP…</span></CommandItem>
              <CommandItem value="搬空线路机 evacuate 换线路机 退役 下线" onSelect={() => { setSearch(''); setMode('evacuate'); }}><PackageOpen /><span className="cmd-main">搬空线路机…</span></CommandItem>
              <CommandItem value="升级全部 agent upgrade" onSelect={() => { close(); askUpgrade(); }}><ArrowUpCircle /><span className="cmd-main">升级全部 agent</span></CommandItem>
              <CommandItem value="切换主题 深色 浅色 theme dark light" onSelect={() => { close(); theme.setPref(dark ? 'light' : 'dark'); }}>
                {dark ? <Sun /> : <Moon />}<span className="cmd-main">切换到{dark ? '浅色' : '深色'}主题</span>
              </CommandItem>
            </CommandGroup>
          </>
        )}
        {machineItems.length > 0 && (
          <CommandGroup heading={mode === 'copy-ip' ? '复制 IP' : mode === 'evacuate' ? '线路机' : '机器'}>
            {machineItems}
            {!search.trim() && mode !== 'evacuate' && machines.length > LIMIT && <div className="cmd-more">还有 {machines.length - LIMIT} 台，输入名称、IP 或团队来搜索</div>}
          </CommandGroup>
        )}
      </CommandList>
      <div className="cmd-foot">
        <span><kbd className="kbd">↑</kbd><kbd className="kbd">↓</kbd>选择</span>
        <span><kbd className="kbd">↵</kbd>{mode === 'copy-ip' ? '复制' : mode === 'evacuate' ? '搬空' : '打开'}</span>
        {mode !== 'root' && <span><kbd className="kbd">⌫</kbd>返回</span>}
        <span style={{ marginLeft: 'auto' }}><kbd className="kbd">esc</kbd>关闭</span>
      </div>
    </Command>
  );
}
