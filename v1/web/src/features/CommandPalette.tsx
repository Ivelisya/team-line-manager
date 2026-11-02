// ⌘K / Ctrl+K 命令面板：跳转页面、搜索机器（名称 / IP / 团队，回车打开抽屉）、团队（跳到机器页的团队筛选）、常用动作。
// 整个文件按需加载（首次打开时才下载 cmdk）。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import { ArrowUpCircle, BellOff, BellRing, Briefcase, CalendarPlus, Clipboard, CloudUpload, Moon, PackageOpen, ShieldCheck, Settings, SquareTerminal, Sun, Users, Waypoints, type LucideIcon } from 'lucide-react';
import { NAV, type NavItem } from '@/lib/nav';
import { useMachineLabels, useTeams, useUpgradeAllAgents } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { copyText } from '@/lib/clipboard';
import { kindLabel } from '@/lib/derive';
import { setOverlay, useOverlay } from '@/lib/palette';
import { matchTier, sv } from '@/lib/match';
import { openEvacuate, openRenew } from '@/lib/nodeActions';
import { openTerminal } from '@/lib/terminal';
import type { ThemePref } from '@/lib/theme';
import { Command, CommandDialog, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { toast } from '@/components/ui/toast';
import { InfoTip } from '@/components/ui/form';
import { ConfirmDialog } from '@/components/app/dialogs';
import { Flag, HealthPill } from '@/components/app/primitives';

// 与侧栏同一份导航；页面里的页签 / 视图单列一条，名字写全路径「页面 › 页签」，关键词加常见叫法（侧栏里没有它们，只有页面）
const SUBS: Record<string, NavItem[]> = {
  '/relays': [{ to: '/relays', label: '线路 › 节点放置', icon: Waypoints, kw: 'placement 放置 矩阵 往返 延迟 换线 建议 换线路机' }],
  '/access': [{ to: '/access/policy', label: '访问监控 › 访问规则', icon: ShieldCheck, kw: 'policy access 访问规则 策略 白名单 黑名单 阻止 域名 allowlist denylist' }],
  '/catalog': [
    { to: '/catalog?section=providers', label: '团队与套餐 › 供应商', icon: Briefcase, kw: 'providers 供应商 厂商' },
    { to: '/catalog?section=plans', label: '团队与套餐 › 套餐', icon: Briefcase, kw: 'plans 套餐 价格' },
    { to: '/catalog?section=due', label: '团队与套餐 › 待付', icon: Briefcase, kw: 'due 待付 续费 到期 付款 未来待付 账单' },
    { to: '/catalog?section=customer', label: '团队与套餐 › 计费台账', icon: Briefcase, kw: 'billing 计费台账 台账 收费 已收 没收 收钱 客户页 按节点数收费 手动计费' },
  ],
  '/incidents': [{ to: '/incidents?tab=uptime', label: '告警 › 月度可用率', icon: BellRing, kw: 'uptime 月度可用率 可用率 断线 成功率 稳定 最不稳定' }],
  '/settings': [
    { to: '/settings?section=retention', label: '设置 › 数据保留', icon: Settings, kw: 'retention 数据保留 保留 清理 磁盘' },
    { to: '/settings?section=fx', label: '设置 › 汇率', icon: Settings, kw: 'fx 汇率 币种 人民币 美元 换算' },
    { to: '/settings?section=probes', label: '设置 › 探测点', icon: Settings, kw: 'probes probe 探测点 探测 香港 主探测来源' },
    { to: '/settings?section=ping', label: '设置 › 三网 PING', icon: Settings, kw: 'ping 三网 三网PING 电信 联通 移动 延迟 丢包' },
    { to: '/settings?section=subs', label: '设置 › 客户端订阅', icon: Settings, kw: 'subs subscription 订阅 客户端订阅 clash openclash v2rayn 链接' },
  ],
};
const PAGES = NAV.flatMap((n) => [n, ...(SUBS[n.to] ?? [])]);
const LIMIT = 8; // 不输入时只列前几台，输入后在全部机器里搜


/** 结果里把输入的字标出来（连续命中的第一处；模糊命中的不标） */
function Hl({ text, q }: { text: string; q: string }) {
  const i = q ? text.toLowerCase().indexOf(q.toLowerCase()) : -1;
  // 包一层 span：.cmd-main 是 flex，裸文本和 mark 会各成一项、被 gap 隔开
  return i < 0 ? <span>{text}</span> : <span>{text.slice(0, i)}<mark>{text.slice(i, i + q.length)}</mark>{text.slice(i + q.length)}</span>;
}

type Theme = { pref: ThemePref; setPref: (p: ThemePref) => void };
type Mode = 'root' | 'copy-ip' | 'evacuate' | 'renew' | 'terminal';
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
        description={<>先升级一台，没问题再继续；失败会自动回滚 <InfoTip text={'顺序：先升一台（有线路机就先选线路机），成功后线路机逐台升，全部成功后节点一起升\n新版本没起来、2 分钟内没有上报，或线路机上的代理不正常，会自动换回旧版本。先行的那台或线路机出问题时，没开始的会全部取消\n已有作业在跑的机器会跳过。升级时 agent 会重启，不影响线路上的连接\n排队中的可以在作业详情里取消'} /></>}
        onConfirm={async () => { const r = await upgradeAll.mutateAsync(); toast.ok(`已提交 ${r.jobs.length} 个升级作业：${r.order}`); }} />
    </>
  );
}

function PaletteBody({ theme, askUpgrade }: { theme: Theme; askUpgrade: () => void }) {
  const nav = useNavigate();
  const { open: openDrawer } = useDrawer();
  // 只取面板用到的字段：开着面板时实时推送只改读数，不会让整表重渲染
  const machines = useMachineLabels().data ?? [];
  const teams = useTeams().data ?? [];
  const [search, setSearch] = React.useState('');
  const [mode, setMode] = React.useState<Mode>('root');
  const go = (to: string) => { close(); nav(to); };
  const dark = document.documentElement.dataset.theme === 'dark';
  const pick = (m: Mode) => { setSearch(''); setMode(m); };
  // 动作：可搜索文本 = 显示的字 + 别名（kw），按看到的字就能搜到
  const actions: { label: string; kw: string; icon: LucideIcon; run: () => void }[] = [
    { label: '上架机器', kw: 'provision 添加机器 VPS 节点 家宽代理 线路机', icon: CloudUpload, run: () => go('/provision') },
    { label: '新建静默', kw: 'silence 维护窗口 维护', icon: BellOff, run: () => go('/incidents?silence=new') },
    { label: '复制某台机器的 IP…', kw: 'copy ip 复制 IP', icon: Clipboard, run: () => pick('copy-ip') },
    { label: '登记续费…', kw: 'renew 续费 续期 到期', icon: CalendarPlus, run: () => pick('renew') },
    { label: '搬空线路机…', kw: 'evacuate 换线路机 退役 下线', icon: PackageOpen, run: () => pick('evacuate') },
    { label: '打开终端…', kw: 'terminal ssh shell 命令行 登录机器', icon: SquareTerminal, run: () => pick('terminal') },
    { label: '升级全部 agent', kw: 'upgrade 升级', icon: ArrowUpCircle, run: () => { close(); askUpgrade(); } },
    { label: `切换到${dark ? '浅色' : '深色'}主题`, kw: '切换主题 深色 浅色 theme dark light 外观', icon: dark ? Sun : Moon, run: () => { close(); theme.setPref(dark ? 'light' : 'dark'); } },
  ];

  // 终端：在用的线路机和节点（家宽代理没有主机可登录）
  const pool = mode === 'evacuate' ? machines.filter((m) => m.kind === 'relay') : mode === 'terminal' ? machines.filter((m) => m.kind !== 'proxy' && m.lifecycle === 'active') : machines;
  const q = search.trim();
  const list = q || mode === 'evacuate' ? pool : pool.slice(0, LIMIT);
  const value = (m: (typeof list)[number]) => sv(m.name, m.ip, `${m.team?.name ?? ''} ${kindLabel(m.kind)}`);
  // cmdk 按分数排序、同分保留上一次排完的 DOM 顺序：加一个小于档差的位次分，同档始终按默认机器顺序
  const order = new Map(list.map((m, i) => [value(m), i]));
  const filter = (v: string, s: string, kw?: string[]) => {
    const t = matchTier(kw?.length ? `${v} ${kw.join(' ')}` : v, s.trim().toLowerCase());
    const i = order.get(v);
    return t === 0 ? 0 : t + (i === undefined ? 0 : 0.5 * (1 - i / list.length));
  };
  // 输入变了：cmdk 选中排在最前的一项，列表滚回顶部，选中项不会停在视口外（回车打开的正是看得见的那一项）
  const listRef = React.useRef<HTMLDivElement>(null);
  React.useLayoutEffect(() => { listRef.current?.scrollTo({ top: 0 }); }, [search, mode]);
  const machineItems = list.map((m) => (
    <CommandItem key={m.id} value={value(m)} keywords={[m.id]}
      onSelect={() => { close(); if (mode === 'copy-ip') void copyText(m.ip, ' IP', true); else if (mode === 'evacuate') openEvacuate(m.id); else if (mode === 'renew') openRenew(m.id); else if (mode === 'terminal') openTerminal({ kind: 'machine', id: m.id, name: m.name }); else openDrawer(m.id); }}>
      <Flag country={m.country} manual={!!m.country_set} ip={m.ip} />
      <span className="cmd-main"><b className="ellipsis"><Hl text={m.name} q={q} /></b>{m.health.status !== 'ok' && <HealthPill m={m} />}</span>
      <span className="cmd-aside">{mode === 'copy-ip' ? <>复制 <span className="num">{m.ip}</span></> : mode === 'evacuate' ? <span>搬空…</span> : mode === 'renew' ? <span>登记续费…</span> : mode === 'terminal' ? <span>打开终端</span> : <span className="num">{m.ip}</span>}</span>
    </CommandItem>
  ));

  return (
    <Command label="命令面板" loop filter={filter}
      onKeyDown={(e) => { if (e.key === 'Backspace' && !search && mode !== 'root') { e.preventDefault(); setMode('root'); } }}>
      <CommandInput autoFocus value={search} onValueChange={setSearch} placeholder={mode === 'copy-ip' ? '选择要复制 IP 的机器…' : mode === 'evacuate' ? '选择要搬空的线路机…' : mode === 'renew' ? '选择要登记续费的机器…' : mode === 'terminal' ? '选择要打开终端的机器…' : '搜索机器、团队、页面或动作…'} />
      <CommandList ref={listRef}>
        <CommandEmpty>没有匹配的结果{search ? `：${search}` : ''}</CommandEmpty>
        {mode === 'root' && (
          <>
            <CommandGroup heading="跳转页面">
              {PAGES.map((p) => (
                <CommandItem key={p.label} value={sv(p.label, '', p.kw)} onSelect={() => go(p.to)}><p.icon /><span className="cmd-main"><Hl text={p.label} q={q} /></span></CommandItem>
              ))}
            </CommandGroup>
            <CommandGroup heading="动作">
              {actions.map((a) => (
                <CommandItem key={a.kw} value={sv(a.label.replace(/…$/, ''), '', a.kw)} onSelect={a.run}><a.icon /><span className="cmd-main"><Hl text={a.label} q={q} /></span></CommandItem>
              ))}
            </CommandGroup>
            {/* 团队：只在输入后出现，跳到机器页的团队筛选 */}
            {q && teams.length > 0 && (
              <CommandGroup heading="团队">
                {teams.map((t) => (
                  <CommandItem key={t.id} value={sv(t.name, '', 'team 团队')} onSelect={() => go(`/nodes?team=${t.id}`)}>
                    <Users /><span className="cmd-main"><Hl text={t.name} q={q} /></span><span className="cmd-aside"><span className="num">{t.node_count}</span> 台</span>
                  </CommandItem>
                ))}
              </CommandGroup>
            )}
          </>
        )}
        {machineItems.length > 0 && (
          <CommandGroup heading={mode === 'copy-ip' ? '复制 IP' : mode === 'evacuate' ? '线路机' : mode === 'renew' ? '登记续费' : mode === 'terminal' ? '打开终端' : '机器'}>
            {machineItems}
            {!q && mode !== 'evacuate' && pool.length > LIMIT && <div className="cmd-more">还有 {pool.length - LIMIT} 台，输入关键字可搜索</div>}
          </CommandGroup>
        )}
      </CommandList>
      <div className="cmd-foot">
        <span><kbd className="kbd">↑</kbd><kbd className="kbd">↓</kbd>选择</span>
        <span><kbd className="kbd">↵</kbd>{mode === 'copy-ip' ? '复制' : mode === 'evacuate' ? '搬空' : mode === 'renew' ? '登记续费' : mode === 'terminal' ? '打开终端' : '打开'}</span>
        {mode !== 'root' && <span><kbd className="kbd">⌫</kbd>返回</span>}
        <span style={{ marginLeft: 'auto' }}><kbd className="kbd">esc</kbd>关闭</span>
      </div>
    </Command>
  );
}
