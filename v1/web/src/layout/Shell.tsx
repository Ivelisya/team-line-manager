// 外壳：56px 顶栏 + 240px 侧栏 + 内容区。选中项是软色胶囊。≤760px 侧栏收成抽屉。
import * as React from 'react';
import { NavLink, Navigate, Outlet, useLocation } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { BellRing, Boxes, Briefcase, CloudUpload, LayoutGrid, ListChecks, LogOut, Menu, Monitor, Moon, Palette, Radar, Server, Sun, Waypoints } from 'lucide-react';
import { AUTH_EVENT, post } from '@/lib/api';
import { useIncidents, useMe } from '@/lib/queries';
import { useStream } from '@/lib/stream';
import { useTheme, type ThemePref } from '@/lib/theme';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/overlays';
import { Toaster } from '@/components/ui/toast';
import { NodeDrawer } from '@/features/NodeDrawer';
import { Pill } from '@/components/app/primitives';

const NAV = [
  { to: '/', label: '首页', icon: LayoutGrid, end: true },
  { to: '/nodes', label: '节点', icon: Server },
  { to: '/relays', label: '线路', icon: Waypoints },
  { to: '/provision', label: '上架', icon: CloudUpload },
  { to: '/jobs', label: '作业', icon: ListChecks },
  { to: '/access', label: '访问监控', icon: Radar },
  { to: '/catalog', label: '团队与套餐', icon: Briefcase },
  { to: '/incidents', label: '告警', icon: BellRing, badge: true },
] as const;

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
    const f = () => qc.invalidateQueries({ queryKey: ['me'] });
    window.addEventListener(AUTH_EVENT, f);
    return () => window.removeEventListener(AUTH_EVENT, f);
  }, [qc]);

  if (me.isError) return <Navigate to="/login" replace state={{ from: loc.pathname + loc.search }} />;
  if (!me.data) return <div className="login-screen"><span className="skel" style={{ width: 160, height: 16 }} /></div>;

  const ThemeIcon = theme.pref === 'dark' ? Moon : theme.pref === 'light' ? Sun : Monitor;
  const logout = async () => { try { await post('/auth/logout'); } finally { qc.clear(); window.location.assign('/login'); } };

  return (
    <div className="app" data-nav={navOpen ? 'open' : 'closed'}>
      <button className="skip-link" onClick={() => document.getElementById('main')?.focus()}>跳到内容</button>
      <header className="topbar">
        <button className="top-btn nav-toggle is-muted" aria-label="打开导航" onClick={() => setNavOpen((o) => !o)} style={{ background: 'transparent', color: 'var(--fg-2)' }}><Menu size={18} /></button>
        <NavLink to="/" className="brand"><span className="brand-mark"><Boxes size={14} /></span>团队线路管理</NavLink>
        <div className="topbar-right">
          {firing > 0 && <NavLink to="/incidents" style={{ textDecoration: 'none' }} aria-label={`${firing} 条告警`}><Pill tone="bad">{firing}<span className="hide-sm"> 条告警</span></Pill></NavLink>}
          <span className="dot" data-tone={stream === 'open' ? undefined : stream === 'error' ? 'bad' : 'neutral'} title={stream === 'open' ? '实时连接正常' : stream === 'error' ? '实时连接中断，每 30 秒轮询兜底' : '正在连接…'} />
          <button className="top-btn" data-top="palette" aria-label="切换配色（粉紫 / 靛蓝）" title={`配色：${theme.palette === 'plum' ? '粉紫' : '靛蓝'}`} onClick={() => theme.setPalette(theme.palette === 'plum' ? 'indigo' : 'plum')}><Palette size={16} /></button>
          <DropdownMenu>
            <DropdownMenuTrigger asChild><button className="top-btn" aria-label="外观"><ThemeIcon size={16} /></button></DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {([['light', '浅色', Sun], ['dark', '深色', Moon], ['system', '跟随系统', Monitor]] as [ThemePref, string, typeof Sun][]).map(([v, l, I]) => (
                <DropdownMenuItem key={v} onSelect={() => theme.setPref(v)}><I />{l}{theme.pref === v && <span style={{ marginLeft: 'auto', color: 'var(--accent)' }}>✓</span>}</DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
          <button className="top-btn" aria-label={`退出登录（${me.data.username}）`} title={`${me.data.username} · 退出登录`} onClick={logout}><LogOut size={16} /></button>
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
        <div className="nav-foot">已登录：{me.data.username}</div>
      </aside>
      <div className="nav-scrim" onClick={() => setNavOpen(false)} />

      <main className="main" id="main" tabIndex={-1}>
        <div className="content"><Outlet /></div>
      </main>
      <NodeDrawer />
      <Toaster />
    </div>
  );
}
