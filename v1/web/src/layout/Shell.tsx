// 外壳：56px 顶栏 + 240px 侧栏 + 内容区。选中项是软色胶囊。≤760px 侧栏收成抽屉。
import * as React from 'react';
import { NavLink, Navigate, Outlet, useLocation, useNavigationType } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { Boxes, LogOut, Menu, Monitor, Moon, Palette, RotateCw, Sun } from 'lucide-react';
import { NAV } from '@/lib/nav';
import { ApiError, AUTH_EVENT, post } from '@/lib/api';
import { useIncidents, useMe } from '@/lib/queries';
import { useStream } from '@/lib/stream';
import { useTheme, type ThemePref } from '@/lib/theme';
import { closeEvacuate, closeMove, closeRestore, closeRetire, useEvacuateTarget, useMoveTarget, useRestoreTarget, useRetireTarget } from '@/lib/nodeActions';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger, Tip, TipHost } from '@/components/ui/overlays';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { Toaster } from '@/components/ui/toast';
import { useDrawer } from '@/lib/drawer';
import { Hotkeys, PaletteTrigger } from '@/features/Hotkeys';
import { JobsIndicator } from '@/features/JobsIndicator';
import { ErrorBoundary } from '@/components/app/ErrorBoundary';
import { Note, Pill } from '@/components/app/primitives';
import { MotionProvider } from '@/lib/motion';

// 抽屉（含各页签、图表、对话框）和生命周期对话框（换线路机、搬空、恢复、退役）只在第一次用到时才下载，登录页和首屏不背这份体积。
// 加载失败（发版后旧块 404）由外面的 silent ErrorBoundary 接住并提示刷新。
const NodeDrawer = React.lazy(() => import('@/features/NodeDrawer').then((m) => ({ default: m.NodeDrawer })));
const LifecycleDialogsHost = React.lazy(() => import('@/components/app/LifecycleDialogs').then((m) => ({ default: m.LifecycleDialogsHost })));

/** 一旦为真就一直为真：懒加载的浮层第一次需要时挂载，之后保持挂载（关闭动画、再次打开都不用重来） */
function useEver(v: boolean) {
  const r = React.useRef(false);
  if (v) r.current = true;
  return r.current;
}

/** 懒加载页面下载期间的占位：页头骨架，外壳不闪 */
function PageFallback() {
  return <div className="stack" role="status" aria-label="加载中"><Skeleton w={160} h={28} /><Skeleton h={96} /><Skeleton h={220} /></div>;
}

const closeLifecycle = () => { closeMove(); closeEvacuate(); closeRestore(); closeRetire(); };

function DrawerHost() {
  const { id } = useDrawer();
  const ever = useEver(!!id);
  return ever ? <React.Suspense fallback={null}><NodeDrawer /></React.Suspense> : null;
}

function LifecycleHost() {
  // 四个 hook 都要无条件调用：写成 a() || b() 会短路，hook 数量随渲染变化，React 报错
  const move = useMoveTarget(), evacuate = useEvacuateTarget(), restore = useRestoreTarget(), retire = useRetireTarget();
  const ever = useEver(!!(move || evacuate || restore || retire));
  return ever ? <React.Suspense fallback={null}><LifecycleDialogsHost /></React.Suspense> : null;
}

export function Shell() {
  const me = useMe();
  const qc = useQueryClient();
  const loc = useLocation();
  const [navOpen, setNavOpen] = React.useState(false);
  const theme = useTheme();
  const stream = useStream(!!me.data);
  const firing = useIncidents('firing').data?.total ?? 0;

  React.useEffect(() => { setNavOpen(false); }, [loc.pathname]);
  // 窄屏侧栏（左侧抽屉）开着时 Esc 收起，焦点回到汉堡按钮；上面叠着对话框、命令面板或节点抽屉时 Esc 归它们
  const navBtn = React.useRef<HTMLButtonElement>(null);
  React.useEffect(() => {
    if (!navOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || document.querySelector('.dialog, .cmd-dialog, .drawer')) return;
      setNavOpen(false); navBtn.current?.focus();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [navOpen]);
  // 换页（不是前进/后退）回到顶部：窗口滚动是全站共用的，不重置的话新页面会停在上一页的滚动位置
  const section = loc.pathname.split('/')[1] ?? '';
  const navType = useNavigationType();
  React.useEffect(() => { if (navType !== 'POP') window.scrollTo(0, 0); }, [section]); // eslint-disable-line react-hooks/exhaustive-deps
  // 登录后首屏稳定下来再悄悄预取抽屉和命令面板的代码：不占首屏带宽，第一次打开时也不用等
  // （面板没预取的话，按 Ctrl+K 后紧接着打的字会在代码下载期间丢掉）
  const authed = !!me.data;
  React.useEffect(() => {
    if (!authed) return;
    const t = setTimeout(() => { for (const p of [import('@/features/NodeDrawer'), import('@/features/CommandPalette')]) p.catch(() => { /* 预取失败不要紧，真用到时会再试 */ }); }, 3000);
    return () => clearTimeout(t);
  }, [authed]);
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

  // 动效提供者只包已登录的外壳：LazyMotion 挂载后（等浏览器空闲）去下载 domMax 特性包，登录页和上面的错误 / 加载态都用不到 m.*
  return (
    <MotionProvider>
      <div className="app" data-nav={navOpen ? 'open' : 'closed'}>
        <button className="skip-link" onClick={() => document.getElementById('main')?.focus()}>跳到内容</button>
        <header className="topbar">
          <button ref={navBtn} className="top-btn nav-toggle" aria-label="导航菜单" aria-expanded={navOpen} aria-controls="sidebar" onClick={() => setNavOpen((o) => !o)}><Menu size={18} /></button>
          <NavLink to="/" className="brand"><span className="brand-mark"><Boxes size={14} /></span>团队线路管理</NavLink>
          <PaletteTrigger />
          <div className="topbar-right">
            {firing > 0 && <NavLink to="/incidents" className="top-alert" aria-label={`${firing} 条告警`}><Pill tone="bad">{firing}<span className="hide-sm"> 条告警</span></Pill></NavLink>}
            <JobsIndicator />
            {stream === 'error'
              ? <Pill tone="warn" dot="pulse" role="status" tip="页面数据暂停更新，连上后自动补齐"><span>实时已断开<span className="hide-sm">，正在重连…</span></span></Pill>
              : <Tip text={stream === 'open' ? '实时连接正常' : '正在连接…'} side="bottom">
                <span className="dot" data-tone={stream === 'open' ? undefined : 'neutral'} tabIndex={0} role="img" aria-label={stream === 'open' ? '实时连接正常' : '正在连接'} />
              </Tip>}
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
  
        <aside className="sidebar" id="sidebar" aria-label="主导航">
          <nav><ul className="nav-list">
            {NAV.map((n) => (
              <li key={n.to}>
                <NavLink to={n.to} end={n.end} className="nav-item">
                  {() => <>
                    <n.icon size={16} /><span className="nav-label">{n.label}</span>
                    {n.badge && firing > 0 && <span className="nav-badge">{firing}</span>}
                  </>}
                </NavLink>
              </li>
            ))}
          </ul></nav>
          <div className="nav-foot">已登录：{me.data.username}<br />按 <kbd className="kbd">?</kbd> 查看快捷键</div>
        </aside>
        <div className="nav-scrim" onClick={() => setNavOpen(false)} />
  
        <main className="main" id="main" tabIndex={-1}>
          <div className="content"><ErrorBoundary resetKey={loc.pathname}><React.Suspense fallback={<PageFallback />}><div key={section} className="page-in"><Outlet /></div></React.Suspense></ErrorBoundary></div>
        </main>
        {/* 浮层宿主各包一层边界：它们出错（图表、懒加载块失败等）只收起自己，不拖垮整页 */}
        <ErrorBoundary silent resetKey={loc.search}><DrawerHost /></ErrorBoundary>
        <ErrorBoundary silent resetKey={loc.pathname} onError={closeLifecycle}><LifecycleHost /></ErrorBoundary>
        <Hotkeys theme={theme} />
        <TipHost />
        <Toaster />
      </div>
    </MotionProvider>
  );
}
