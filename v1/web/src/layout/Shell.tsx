// 外壳：56px 顶栏 + 240px 侧栏 + 内容区。选中项是软色胶囊。≤760px 侧栏收成抽屉。
import * as React from 'react';
import { NavLink, Navigate, Outlet, useLocation } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { BellRing, Boxes, Briefcase, CloudUpload, LayoutGrid, ListChecks, LogOut, Menu, Monitor, Moon, Palette, Radar, RotateCw, Server, Settings, Sun, Waypoints } from 'lucide-react';
import { ApiError, AUTH_EVENT, post } from '@/lib/api';
import { useIncidents, useMachinePreview, useMe } from '@/lib/queries';
import { useStream } from '@/lib/stream';
import { useTheme, type ThemePref } from '@/lib/theme';
import { closeRetire, useRetireTarget } from '@/lib/nodeActions';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger, Tip } from '@/components/ui/overlays';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { Toaster } from '@/components/ui/toast';
import { NodeDrawer } from '@/features/NodeDrawer';
import { Hotkeys, PaletteTrigger } from '@/features/Hotkeys';
import { JobsIndicator } from '@/features/JobsIndicator';
import { ErrorBoundary } from '@/components/app/ErrorBoundary';
import { RetireDialog } from '@/components/app/RetireDialog';
import { LifecycleDialogsHost } from '@/components/app/LifecycleDialogs';
import { Note, Pill } from '@/components/app/primitives';

const NAV = [
  { to: '/', label: '首页', icon: LayoutGrid, end: true },
  { to: '/nodes', label: '节点', icon: Server },
  { to: '/relays', label: '线路', icon: Waypoints },
  { to: '/provision', label: '上架', icon: CloudUpload },
  { to: '/jobs', label: '作业', icon: ListChecks },
  { to: '/access', label: '访问监控', icon: Radar },
  { to: '/catalog', label: '团队与套餐', icon: Briefcase },
  { to: '/incidents', label: '告警', icon: BellRing, badge: true },
  { to: '/settings', label: '设置', icon: Settings },
] as const;

/** 懒加载页面下载期间的占位：页头骨架，外壳不闪 */
function PageFallback() {
  return <div className="stack" role="status" aria-label="加载中"><Skeleton w={160} h={28} /><Skeleton h={96} /><Skeleton h={220} /></div>;
}

/** 卡片/表格菜单里“退役…”要弹的确认框：全站只渲染这一份 */
function NodeActionsHost() {
  const id = useRetireTarget();
  const q = useMachinePreview(id);
  if (!id || !q.data) return null;
  return <RetireDialog m={q.data} open onOpenChange={(o) => { if (!o) closeRetire(); }} onDone={closeRetire} />;
}

export function Shell() {
  const me = useMe();
  const qc = useQueryClient();
  const loc = useLocation();
  const [navOpen, setNavOpen] = React.useState(false);
  const theme = useTheme();
  const stream = useStream(!!me.data);
  const firing = useIncidents('firing').data?.length ?? 0;

  React.useEffect(() => { setNavOpen(false); }, [loc.pathname]);
  React.useEffect(() => {
    // 会话过期：把 me 缓存清成未登录状态（resetQueries 会丢掉旧数据并重取，拿到 401 后跳登录页）。
    // 只清缓存不重取的话，Login 页会看到旧的 me.data 又跳回来，形成重定向循环。1 秒内的重复通知合并。
    let last = 0;
    const f = () => {
      if (Date.now() - last < 1000) return;
      last = Date.now();
      void qc.resetQueries({ queryKey: ['me'] });
    };
    window.addEventListener(AUTH_EVENT, f);
    return () => window.removeEventListener(AUTH_EVENT, f);
  }, [qc]);

  // 只有确认是 401 才去登录页；网络错误 / 5xx 显示可重试的错误态，不要把人踢走
  if (me.isError && me.error instanceof ApiError && me.error.status === 401) return <Navigate to="/login" replace state={{ from: loc.pathname + loc.search }} />;
  if (me.isError && !me.data) {
    return (
      <div className="login-screen">
        <div className="login-card" role="alert">
          <div className="login-brand"><span className="brand-mark"><Boxes size={14} /></span>团队线路管理</div>
          <Note tone="bad">{me.error instanceof Error ? me.error.message : '无法连接到中控'}</Note>
          <div className="hint">这不是登录问题：中控暂时连不上或返回了错误，已登录的会话仍然有效。</div>
          <Button variant="primary" loading={me.isFetching} onClick={() => void me.refetch()}><RotateCw />重试</Button>
        </div>
      </div>
    );
  }
  if (!me.data) return <div className="login-screen"><span className="skel" style={{ width: 160, height: 16 }} /></div>;

  const ThemeIcon = theme.pref === 'dark' ? Moon : theme.pref === 'light' ? Sun : Monitor;
  const logout = async () => { try { await post('/auth/logout'); } finally { qc.clear(); window.location.assign('/login'); } };

  return (
    <div className="app" data-nav={navOpen ? 'open' : 'closed'}>
      <button className="skip-link" onClick={() => document.getElementById('main')?.focus()}>跳到内容</button>
      <header className="topbar">
        <button className="top-btn nav-toggle is-muted" aria-label="打开导航" onClick={() => setNavOpen((o) => !o)} style={{ background: 'transparent', color: 'var(--fg-2)' }}><Menu size={18} /></button>
        <NavLink to="/" className="brand"><span className="brand-mark"><Boxes size={14} /></span>团队线路管理</NavLink>
        <PaletteTrigger />
        <div className="topbar-right">
          {firing > 0 && <NavLink to="/incidents" style={{ textDecoration: 'none' }} aria-label={`${firing} 条告警`}><Pill tone="bad">{firing}<span className="hide-sm"> 条告警</span></Pill></NavLink>}
          <JobsIndicator />
          <Tip text={stream === 'open' ? '实时连接正常' : stream === 'error' ? '实时连接中断，正在自动重连，期间每 30 秒轮询一次' : '正在连接…'} side="bottom">
            <span className="dot" data-tone={stream === 'open' ? undefined : stream === 'error' ? 'bad' : 'neutral'} tabIndex={0} role="img" aria-label={stream === 'open' ? '实时连接正常' : stream === 'error' ? '实时连接中断' : '正在连接'} />
          </Tip>
          <Tip text={`配色：${theme.palette === 'plum' ? '粉紫' : '靛蓝'}（点击切换）`} side="bottom">
            <button className="top-btn" data-top="palette" aria-label="切换配色（粉紫 / 靛蓝）" onClick={() => theme.setPalette(theme.palette === 'plum' ? 'indigo' : 'plum')}><Palette size={16} /></button>
          </Tip>
          <DropdownMenu>
            <DropdownMenuTrigger asChild><button className="top-btn" aria-label="外观"><ThemeIcon size={16} /></button></DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {([['light', '浅色', Sun], ['dark', '深色', Moon], ['system', '跟随系统', Monitor]] as [ThemePref, string, typeof Sun][]).map(([v, l, I]) => (
                <DropdownMenuItem key={v} onSelect={() => theme.setPref(v)}><I />{l}{theme.pref === v && <span style={{ marginLeft: 'auto', color: 'var(--accent)' }}>✓</span>}</DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
          <Tip text={`${me.data.username} · 退出登录`} side="bottom">
            <button className="top-btn" aria-label={`退出登录（${me.data.username}）`} onClick={logout}><LogOut size={16} /></button>
          </Tip>
        </div>
      </header>

      <aside className="sidebar" aria-label="主导航">
        <nav><ul className="nav-list">
          {NAV.map((n) => (
            <li key={n.to}>
              <NavLink to={n.to} end={'end' in n ? n.end : false} className="nav-item">
                {() => <>
                  <n.icon size={16} /><span className="nav-label">{n.label}</span>
                  {'badge' in n && firing > 0 && <span className="nav-badge">{firing}</span>}
                </>}
              </NavLink>
            </li>
          ))}
        </ul></nav>
        <div className="nav-foot">已登录：{me.data.username}<br />按 <kbd className="kbd">?</kbd> 查看快捷键</div>
      </aside>
      <div className="nav-scrim" onClick={() => setNavOpen(false)} />

      <main className="main" id="main" tabIndex={-1}>
        <div className="content"><ErrorBoundary resetKey={loc.pathname}><React.Suspense fallback={<PageFallback />}><Outlet /></React.Suspense></ErrorBoundary></div>
      </main>
      <NodeDrawer />
      <NodeActionsHost />
      <LifecycleDialogsHost />
      <Hotkeys theme={theme} />
      <Toaster />
    </div>
  );
}
