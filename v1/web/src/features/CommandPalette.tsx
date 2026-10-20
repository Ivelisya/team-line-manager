// ⌘K / Ctrl+K 命令面板：跳转页面、搜索机器（名称 / IP / 团队模糊匹配，回车打开抽屉）、常用动作。
// 整个文件按需加载（首次打开时才下载 cmdk）。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import { ArrowUpCircle, BellOff, BellRing, Briefcase, Clipboard, CloudUpload, Gauge, LayoutGrid, ListChecks, Moon, Server, Settings, Sun, Waypoints } from 'lucide-react';
import { useMachines, useUpgradeAllAgents } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { copyText } from '@/lib/clipboard';
import { KIND_LABEL } from '@/lib/sort';
import { setOverlay, useOverlay } from '@/lib/palette';
import type { ThemePref } from '@/lib/theme';
import { Command, CommandDialog, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog } from '@/components/app/dialogs';
import { HealthPill, KindIcon } from '@/components/app/primitives';

const PAGES = [
  { to: '/', label: '首页', icon: LayoutGrid, kw: 'home dashboard 总览' },
  { to: '/nodes', label: '节点', icon: Server, kw: 'nodes 表格 列表' },
  { to: '/relays', label: '线路机', icon: Waypoints, kw: 'relays 线路 入口' },
  { to: '/access', label: '访问监控', icon: Gauge, kw: 'access 访问 监控' },
  { to: '/jobs', label: '作业', icon: ListChecks, kw: 'jobs 任务' },
  { to: '/incidents', label: '告警', icon: BellRing, kw: 'incidents alerts 静默' },
  { to: '/catalog', label: '目录（团队与套餐）', icon: Briefcase, kw: 'catalog teams plans providers 团队 套餐 供应商' },
  { to: '/provision', label: '上架', icon: CloudUpload, kw: 'provision 添加 新增' },
  { to: '/settings', label: '设置', icon: Settings, kw: 'settings api 密钥 通知 key notify' },
];
const LIMIT = 8; // 不输入时只列前几台，输入后在全部机器里搜

export default function CommandPalette({ theme }: { theme: { pref: ThemePref; setPref: (p: ThemePref) => void } }) {
  const open = useOverlay() === 'palette';
  const nav = useNavigate();
  const { open: openDrawer } = useDrawer();
  const machines = useMachines('live').data ?? [];
  const upgradeAll = useUpgradeAllAgents();
  const [search, setSearch] = React.useState('');
  const [mode, setMode] = React.useState<'root' | 'copy-ip'>('root');
  const [askUpgrade, setAskUpgrade] = React.useState(false);

  React.useEffect(() => { if (!open) { setSearch(''); setMode('root'); } }, [open]);
  const close = () => setOverlay(null);
  const go = (to: string) => { close(); nav(to); };
  const dark = document.documentElement.dataset.theme === 'dark';

  const list = search.trim() ? machines : machines.slice(0, LIMIT);
  const machineItems = list.map((m) => (
    <CommandItem key={m.id} value={`${m.name} ${m.ip} ${m.team?.name ?? ''} ${KIND_LABEL[m.kind]}`} keywords={[m.id]}
      onSelect={() => { close(); if (mode === 'copy-ip') void copyText(m.ip, ' IP'); else openDrawer(m.id); }}>
      <KindIcon kind={m.kind} />
      <span className="cmd-main"><b className="ellipsis">{m.name}</b><HealthPill m={m} /></span>
      <span className="cmd-aside">{mode === 'copy-ip' ? <>复制 <span className="num">{m.ip}</span></> : <span className="num">{m.ip}</span>}</span>
    </CommandItem>
  ));

  return (
    <>
      <CommandDialog open={open} onOpenChange={(o) => setOverlay(o ? 'palette' : null)} title="命令面板">
        <Command label="命令面板" loop
          onKeyDown={(e) => { if (e.key === 'Backspace' && !search && mode !== 'root') { e.preventDefault(); setMode('root'); } }}>
          <CommandInput autoFocus value={search} onValueChange={setSearch} placeholder={mode === 'copy-ip' ? '选择要复制 IP 的机器…' : '搜索机器、页面或动作…'} />
          <CommandList>
            <CommandEmpty>没有匹配的结果{search ? `：${search}` : ''}</CommandEmpty>
            {mode === 'root' && (
              <>
                <CommandGroup heading="跳转页面">
                  {PAGES.map((p) => (
                    <CommandItem key={p.to} value={`${p.label} ${p.kw}`} onSelect={() => go(p.to)}><p.icon /><span className="cmd-main">{p.label}</span></CommandItem>
                  ))}
                </CommandGroup>
                <CommandGroup heading="动作">
                  <CommandItem value="上架节点 provision 添加机器" onSelect={() => go('/provision')}><CloudUpload /><span className="cmd-main">上架节点</span></CommandItem>
                  <CommandItem value="新建静默 silence 维护窗口" onSelect={() => go('/incidents?silence=new')}><BellOff /><span className="cmd-main">新建静默</span></CommandItem>
                  <CommandItem value="复制某机器 IP copy ip" onSelect={() => { setSearch(''); setMode('copy-ip'); }}><Clipboard /><span className="cmd-main">复制某台机器的 IP…</span></CommandItem>
                  <CommandItem value="升级全部 agent upgrade" onSelect={() => { close(); setAskUpgrade(true); }}><ArrowUpCircle /><span className="cmd-main">升级全部 agent</span></CommandItem>
                  <CommandItem value="切换主题 深色 浅色 theme dark light" onSelect={() => { close(); theme.setPref(dark ? 'light' : 'dark'); }}>
                    {dark ? <Sun /> : <Moon />}<span className="cmd-main">切换到{dark ? '浅色' : '深色'}主题</span>
                  </CommandItem>
                </CommandGroup>
              </>
            )}
            {machineItems.length > 0 && (
              <CommandGroup heading={mode === 'copy-ip' ? '复制 IP' : '机器'}>
                {machineItems}
                {!search.trim() && machines.length > LIMIT && <div className="cmd-more">还有 {machines.length - LIMIT} 台，输入名称、IP 或团队来搜索</div>}
              </CommandGroup>
            )}
          </CommandList>
          <div className="cmd-foot">
            <span><kbd className="kbd">↑</kbd><kbd className="kbd">↓</kbd>选择</span>
            <span><kbd className="kbd">↵</kbd>{mode === 'copy-ip' ? '复制' : '打开'}</span>
            {mode !== 'root' && <span><kbd className="kbd">⌫</kbd>返回</span>}
            <span style={{ marginLeft: 'auto' }}><kbd className="kbd">esc</kbd>关闭</span>
          </div>
        </Command>
      </CommandDialog>

      <ConfirmDialog open={askUpgrade} onOpenChange={setAskUpgrade} title="升级全部 agent？" confirmLabel="全部升级"
        description="会给所有在用的线路机和 VPS 节点各建一个升级作业（已有进行中作业的会跳过），升级时 agent 会重启，不影响线路上的连接。"
        onConfirm={async () => { const r = await upgradeAll.mutateAsync(); toast.ok(`已提交 ${r.jobs.length} 个升级作业`); }} />
    </>
  );
}
