// 外壳：56px 顶栏 + 240px 侧栏 + 内容区。选中项是软色胶囊。≤1023px 侧栏收成抽屉。
import * as React from 'react';
import { NavLink, Navigate, Outlet, useLocation, useNavigationType } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { Boxes, LogOut, Menu, Monitor, Moon, RotateCw, Sun } from 'lucide-react';
import { NAV } from '@/lib/nav';
import { ApiError, AUTH_EVENT, post } from '@/lib/api';
import { useIncidents, useMe, useMeta } from '@/lib/queries';
import { useReachLabel } from '@/lib/probePoints';
import { useStream } from '@/lib/stream';
import { useTheme, type Palette, type ThemePref } from '@/lib/theme';
import { useTitleAlerts } from '@/lib/title';
import { closeEvacuate, closeMaint, closeMove, closeRenew, closeRetire, closeUpgrade, useEvacuateTarget, useMaintTarget, useMoveTarget, useRenewTarget, useRetireTarget, useUpgradeTarget } from '@/lib/nodeActions';
import { resetTerminal, useTerminalTarget } from '@/lib/terminal';
import { DropdownMenu, DropdownMenuContent, DropdownMenuLabel, DropdownMenuRadioGroup, DropdownMenuRadioItem, DropdownMenuSeparator, DropdownMenuTrigger, Tip, TipHost } from '@/components/ui/overlays';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { Toaster } from '@/components/ui/toast';
import { useDrawer } from '@/lib/drawer';
import { setOverlay } from '@/lib/palette';
import { useBootTarget, useDeployTarget, useRemoved, useRouterDrawer, useSwitchTarget, useSyncTarget } from '@/lib/routers';
import { Hotkeys, PaletteTrigger } from '@/features/Hotkeys';
import { AlertsIndicator } from '@/features/AlertsIndicator';
import { JobsIndicator } from '@/features/JobsIndicator';
import { ErrorBoundary } from '@/components/app/ErrorBoundary';
import { Note, Pill } from '@/components/app/primitives';
import { MotionProvider } from '@/lib/motion';

// 抽屉（含各页签、图表、对话框）和生命周期对话框（换线路机、搬空、替换、退役）只在第一次用到时才下载，登录页和首屏不背这份体积。
// 加载失败（发版后旧块 404）由外面的 silent ErrorBoundary 接住并提示刷新。
const NodeDrawer = React.lazy(() => import('@/features/NodeDrawer').then((m) => ({ default: m.NodeDrawer })));
const LifecycleDialogsHost = React.lazy(() => import('@/components/app/LifecycleDialogs').then((m) => ({ default: m.LifecycleDialogsHost })));
// 软路由抽屉（?r=）与它的对话框（接入命令、重新部署、删除后的说明）：同样第一次用到时才下载
const RouterDrawer = React.lazy(() => import('@/features/RouterDrawer').then((m) => ({ default: m.RouterDrawer })));
const RouterDialogsHost = React.lazy(() => import('@/features/RouterDialogs').then((m) => ({ default: m.RouterDialogsHost })));
// 单台机器的维护窗口对话框（卡片菜单、抽屉「管理」）：第一次用到时才下载
const MaintenanceHost = React.lazy(() => import('@/components/app/MaintenanceDialog').then((m) => ({ default: m.MaintenanceHost })));
// 升级 agent 的确认框（卡片菜单、抽屉「管理」）：同样第一次用到时才下载
const UpgradeHost = React.lazy(() => import('@/components/app/UpgradeDialog').then((m) => ({ default: m.UpgradeHost })));
// 网页终端连同 xterm（约 290KB）同理：第一次打开终端时才下载
const TerminalHost = React.lazy(() => import('@/features/Terminal').then((m) => ({ default: m.TerminalHost })));

/** 浏览器是否联网（断网时顶栏写「已断开」而不是「重连中…」） */
const onlineSub = (f: () => void) => { window.addEventListener('online', f); window.addEventListener('offline', f); return () => { window.removeEventListener('online', f); window.removeEventListener('offline', f); }; };
const useOnline = () => React.useSyncExternalStore(onlineSub, () => navigator.onLine);

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

const closeLifecycle = () => { closeMove(); closeEvacuate(); closeRetire(); closeRenew(); };

const THEMES: [ThemePref, string, typeof Sun][] = [['light', '浅色', Sun], ['dark', '深色', Moon], ['system', '跟随系统', Monitor]];
const PALETTES: [Palette, string][] = [['plum', '粉紫'], ['indigo', '靛蓝']];

function DrawerHost() {
  const { id } = useDrawer();
  const ever = useEver(!!id);
  return ever ? <React.Suspense fallback={null}><NodeDrawer /></React.Suspense> : null;
}

function RouterHost() {
  const { id } = useRouterDrawer();
  const boot = useBootTarget(), deploy = useDeployTarget(), removed = useRemoved(), sync = useSyncTarget(), sw = useSwitchTarget();
  const drawer = useEver(!!id);
  const dialogs = useEver(!!(boot || deploy || removed || sync || sw));
  return <>{drawer && <React.Suspense fallback={null}><RouterDrawer /></React.Suspense>}{dialogs && <React.Suspense fallback={null}><RouterDialogsHost /></React.Suspense>}</>;
}

function LifecycleHost() {
  // 四个 hook 都要无条件调用：写成 a() || b() 会短路，hook 数量随渲染变化，React 报错
  const move = useMoveTarget(), evacuate = useEvacuateTarget(), retire = useRetireTarget(), renew = useRenewTarget();
  const ever = useEver(!!(move || evacuate || retire || renew));
  return ever ? <React.Suspense fallback={null}><LifecycleDialogsHost /></React.Suspense> : null;
}

function MaintHost() {
  const ever = useEver(!!useMaintTarget());
  return ever ? <React.Suspense fallback={null}><MaintenanceHost /></React.Suspense> : null;
}

function UpgHost() {
  const ever = useEver(!!useUpgradeTarget());
  return ever ? <React.Suspense fallback={null}><UpgradeHost /></React.Suspense> : null;
}

function TermHost() {
  const ever = useEver(!!useTerminalTarget());
  return ever ? <React.Suspense fallback={null}><TerminalHost /></React.Suspense> : null;
}

export function Shell() {
  const me = useMe();
  // 中文名、阈值等（/meta）到了才渲染外壳与页面：derive 里的 kindLabel 等同步读取它。不等 /auth/me，两个请求并发；
  // 未登录时 /meta 的 401 照常走会话过期通知，/auth/me 的 401 才决定跳登录页
  const meta = useMeta(true);
  useReachLabel(); // 顺手预取「谁连不上」的来源，页面里的卡片挂载时已有缓存
  const qc = useQueryClient();
  const loc = useLocation();
  const [navOpen, setNavOpen] = React.useState(false);
  const theme = useTheme();
  const stream = useStream(!!me.data);
  const online = useOnline();
  // 标签页标题同顶栏告警胶囊：进行中、还没确认的条数
  const inc = useIncidents('firing').data;
  const firing = inc ? inc.total - inc.acked : 0;
  useTitleAlerts(firing);

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
  const metaDenied = meta.error instanceof ApiError && meta.error.status === 401; // 没登录：等 /auth/me 的 401 把人带去登录页，不闪错误态
  const failed = me.isError && !me.data ? me : meta.isError && !meta.data && !metaDenied ? meta : null;
  if (failed) {
    return (
      <div className="login-screen">
        <div className="login-card" role="alert">
          <div className="login-brand"><span className="brand-mark"><Boxes size={14} /></span>团队线路管理</div>
          <Note tone="bad">{failed.error instanceof Error ? failed.error.message : '无法连接到中控'}</Note>
          <Button variant="primary" loading={failed.isFetching} onClick={() => void failed.refetch()}><RotateCw />重试</Button>
        </div>
      </div>
    );
  }
  if (!me.data || !meta.data) return <div className="login-screen"><span className="skel" style={{ width: 160, height: 16 }} /></div>;

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
            <AlertsIndicator />
            <JobsIndicator />
            {/* 实时连接只在出问题时出现（连着、首次连接中都不占位）：断网写「已断开」，其余在退避重连 */}
            {stream === 'error' && <Pill tone="warn" role="status" tabIndex={0} tip="页面数据暂停更新，连上后自动补齐">{online ? '重连中…' : '已断开'}</Pill>}
            {/* 深浅色与配色在同一个菜单里（原来配色是单独一个盲切的按钮，窄屏还被藏起来）；色块就是该配色的强调色 */}
            <DropdownMenu>
              <DropdownMenuTrigger asChild><button className="top-btn" aria-label="外观：深浅色与配色" data-tip="外观"><ThemeIcon size={16} /></button></DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                <DropdownMenuLabel>外观</DropdownMenuLabel>
                <DropdownMenuRadioGroup value={theme.pref} onValueChange={(v) => theme.setPref(v as ThemePref)}>
                  {THEMES.map(([v, l, I]) => <DropdownMenuRadioItem key={v} value={v}><I />{l}</DropdownMenuRadioItem>)}
                </DropdownMenuRadioGroup>
                <DropdownMenuSeparator />
                <DropdownMenuLabel>配色</DropdownMenuLabel>
                <DropdownMenuRadioGroup value={theme.palette} onValueChange={(v) => theme.setPalette(v as Palette)}>
                  {PALETTES.map(([v, l]) => <DropdownMenuRadioItem key={v} value={v}><i className="swatch" data-palette={v} />{l}</DropdownMenuRadioItem>)}
                </DropdownMenuRadioGroup>
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
                    {n.badge && firing > 0 && <span className="count-badge">{firing}</span>}
                  </>}
                </NavLink>
              </li>
            ))}
          </ul></nav>
          <button type="button" className="nav-foot" onClick={() => setOverlay('help')}><kbd className="kbd">?</kbd>快捷键</button>
        </aside>
        <div className="nav-scrim" onClick={() => setNavOpen(false)} />
  
        <main className="main" id="main" tabIndex={-1}>
          <div className="content"><ErrorBoundary resetKey={loc.pathname}><React.Suspense fallback={<PageFallback />}><div key={section} className="page-in"><Outlet /></div></React.Suspense></ErrorBoundary></div>
        </main>
        {/* 浮层宿主各包一层边界：它们出错（图表、懒加载块失败等）只收起自己，不拖垮整页 */}
        {/* 路由抽屉排在机器抽屉前面：从路由的出口换到机器抽屉时，标签页标题由后提交的机器抽屉写（lib/title 只有一格） */}
        <ErrorBoundary silent resetKey={loc.search}><RouterHost /></ErrorBoundary>
        <ErrorBoundary silent resetKey={loc.search}><DrawerHost /></ErrorBoundary>
        <ErrorBoundary silent resetKey={loc.pathname} onError={closeLifecycle}><LifecycleHost /></ErrorBoundary>
        <ErrorBoundary silent resetKey={loc.pathname} onError={resetTerminal}><TermHost /></ErrorBoundary>
        <ErrorBoundary silent resetKey={loc.pathname} onError={closeMaint}><MaintHost /></ErrorBoundary>
        <ErrorBoundary silent resetKey={loc.pathname} onError={closeUpgrade}><UpgHost /></ErrorBoundary>
        <Hotkeys theme={theme} />
        <TipHost />
        <Toaster />
      </div>
    </MotionProvider>
  );
}
