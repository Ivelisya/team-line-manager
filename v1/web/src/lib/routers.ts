// 软路由（客户现场的 OpenWrt + OpenClash）：类型、查询与写操作、抽屉开关、接入命令对话框的状态。
// 中控经反向隧道每 30 秒在路由上跑一次只读采集，结果随 {"type":"routers"} 事件推来（eventKeys 让 ['routers'] 下的列表与详情一起重取），
// 所以列表与详情 staleTime 都是 Infinity；曲线不推，抽屉开着时按范围轮询（同机器抽屉的曲线）。形状见 r21 设计稿「API」。
import { useCallback, useRef } from 'react';
import { useSearchParams } from 'react-router-dom';
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { del, get, patch, post } from './api';
import { useTeams, useWrite } from './queries';
import { collator } from './sort';
import { createStore } from './store';
import type { SeriesPoint, Team } from './types';
import * as F from './format';

/** pending = 建了记录、路由还没连上过；offline = 连上过、现在隧道不通或采集失败 */
export type RouterState = 'pending' | 'online' | 'offline';
/** 列表里的摘要：最近一次采集成功的快照，从没采集成功过为 null。速率是 bit/s，内存 / 存储是字节 */
export interface RouterSummary {
  uptime_s: number; cpu_pct: number; load1: number; cores: number; mem_used: number; mem_total: number; disk_used: number; disk_total: number;
  conntrack: number; conntrack_max: number; temp_c: number | null;
  wan: { up: boolean; ip: string | null; rx_bps: number; tx_bps: number };
  openclash: { enabled: boolean; running: boolean; version: string | null; mode: string | null; conns: number; up_bps: number; down_bps: number };
  /** managed：server:port 对上了 fleet 入口的出口；drift：其中参数与入口当前参数不一致的 */
  exits: { total: number; alive: number; managed: number; drift: number };
  clients: number;
}
/** 一键部署（中控 SSH 到路由上替你执行接入）的进度：最近一次的，从没部署过为 null。step 是当前步骤（中文） */
export interface RouterDeploy { state: 'running' | 'failed' | 'done'; step: string | null; error: string | null; started_at: string; finished_at: string | null }
export interface RouterView {
  id: string; name: string; team: { id: string; name: string } | null; note: string; tunnel_port: number; created_at: string;
  state: RouterState; last_seen_at: string | null; error: string | null; summary: RouterSummary | null; deploy: RouterDeploy | null;
}
export interface RouterIface { name: string; device: string; up: boolean; proto: string; ip: string | null; uptime_s: number | null; rx_bps: number; tx_bps: number; rx_bytes: number; tx_bytes: number }
/** ok：参数与 fleet 入口一致；drift：server:port 对上了，但 uuid / reality 参数不一致；unmanaged：没对上任何 fleet 入口 */
export type ExitSync = 'ok' | 'drift' | 'unmanaged';
export interface RouterExit {
  name: string; type: string; server: string; port: number;
  entry: { id: string; name: string; node_id: string; relay_id: string } | null; sync: ExitSync;
  /** alive / delay_ms 来自每 5 分钟一次的延迟测试（delay_at 是测试时刻）；还没测过为 null。conns 等是 mihomo 连接的汇总 */
  alive: boolean | null; delay_ms: number | null; delay_at: string | null; conns: number; up_bps: number; down_bps: number; clients: number;
}
export interface RouterClient { ip: string; mac: string | null; hostname: string | null; online: boolean; conns: number; exits: { name: string; conns: number }[] }
export interface RouterDetail {
  system: { os: string; kernel: string; cpu_model: string; cores: number; load: [number, number, number]; procs: number; clock_skew_s: number | null; clash_rss: number | null };
  interfaces: RouterIface[];
  openclash: { enabled: boolean; running: boolean; version: string | null; mode: string | null; config_path: string | null; config_mtime: string | null };
  exits: RouterExit[]; clients: RouterClient[]; collected_at: string;
}
/** 详情：没采集成功过（pending）时 detail 为 null */
export type RouterFull = RouterView & { detail: RouterDetail | null };
export interface Bootstrap { cmd: string; expires_at: string }
/** 曲线：点里的字段 cpu_pct load1 mem_pct disk_pct conntrack_pct temp_c wan_rx_bps wan_tx_bps proxy_up_bps proxy_down_bps conns clients exits_alive */
export interface RouterMetrics { step_s: number; points: SeriesPoint[] }
/** 各出口的曲线：点里 delay_ms（测试失败为 null）alive conns up_bps down_bps */
export interface RouterExitMetrics { step_s: number; exits: { name: string; points: SeriesPoint[] }[] }

// ---- 查询 ----
/** 团队顺序（同机器的默认顺序），再按名称；没分团队的排最后 */
const sortRouters = (teams: Team[]) => {
  const order = new Map(teams.map((t) => [t.id, t.sort_order]));
  return (a: RouterView, b: RouterView) => ((a.team ? order.get(a.team.id) ?? 1e9 : 1e9 + 1) - (b.team ? order.get(b.team.id) ?? 1e9 : 1e9 + 1)) || collator.compare(a.name, b.name);
};
export function useRouters() {
  const teams = useTeams().data;
  const select = useCallback((d: RouterView[]) => [...d].sort(sortRouters(teams ?? [])), [teams]);
  return useQuery({ queryKey: ['routers', 'list'], queryFn: ({ signal }) => get<RouterView[]>('/routers', signal), staleTime: Infinity, select });
}
export const useRouter = (id: string | null) =>
  useQuery({ queryKey: ['routers', id], queryFn: ({ signal }) => get<RouterFull>(`/routers/${id}`, signal), enabled: !!id, staleTime: Infinity });
/** 曲线的轮询间隔随范围放大（同机器抽屉）；换范围时先留着上一段（调用方按 isPlaceholderData 调淡），换了路由不沿用 */
const POLL: Record<string, number> = { '1h': 30_000, '6h': 60_000, '24h': 120_000 };
export const routerPoll = (range: string) => POLL[range] ?? 300_000;
export const useRouterMetrics = (id: string, range: string) =>
  useQuery({ queryKey: ['router-metrics', id, range], queryFn: ({ signal }) => get<RouterMetrics>(`/routers/${id}/metrics?range=${range}`, signal), staleTime: routerPoll(range), refetchInterval: routerPoll(range), refetchOnWindowFocus: false, placeholderData: (prev, pq) => (pq?.queryKey[1] === id ? keepPreviousData(prev) : undefined) });
export const useRouterExitMetrics = (id: string, range: string) =>
  useQuery({ queryKey: ['router-exit-metrics', id, range], queryFn: ({ signal }) => get<RouterExitMetrics>(`/routers/${id}/exits/metrics?range=${range}`, signal), staleTime: routerPoll(range), refetchInterval: routerPoll(range), refetchOnWindowFocus: false, placeholderData: (prev, pq) => (pq?.queryKey[1] === id ? keepPreviousData(prev) : undefined) });

// ---- 写操作（全部写审计） ----
const K = [['routers'], ['audit']];
/** 一键部署的 SSH 登录：password 不填 = 用中控现有的密钥登录；只随这一次请求发出，中控不存、之后也不回显 */
export interface SshLogin { host: string; port: number; user: string; password?: string }
/** 带 ssh：中控马上开始部署，只返回 id（进度在路由的 deploy 上，随 routers 事件更新）；不带：返回在路由上执行的接入命令 */
export const useAddRouter = () => useWrite((b: { name: string; team_id?: string; note?: string; ssh?: SshLogin }) => post<{ id: string; bootstrap?: Bootstrap }>('/routers', b), K);
/** 重新部署（一键部署的重试，或换了地址 / 密码）。已有一次在跑时 409（错误信息直接给用户看） */
export const useDeployRouter = () => useWrite(({ id, ...b }: { id: string } & SshLogin) => post<{ ok: boolean }>(`/routers/${id}/deploy`, b), K);
/** 重新生成接入命令：换隧道密钥，旧隧道立刻失效 */
export const useRegenBootstrap = () => useWrite((id: string) => post<Bootstrap>(`/routers/${id}/bootstrap`), K);
/** team_id 传 null = 不分团队 */
export const usePatchRouter = () => useWrite(({ id, ...b }: { id: string; name?: string; team_id?: string | null; note?: string }) => patch(`/routers/${id}`, b), K);
export const useDeleteRouter = () => useWrite((id: string) => del(`/routers/${id}`), K);

// ---- 抽屉：?r=<路由id>&rtab=<页签>，与机器抽屉（?m=）各管各的 ----
export type RouterTab = 'overview' | 'exits' | 'clients' | 'charts';
export const ROUTER_TABS: { value: RouterTab; label: string }[] = [
  { value: 'overview', label: '概览' }, { value: 'exits', label: '出口' }, { value: 'clients', label: '客户端' }, { value: 'charts', label: '曲线' },
];
export function useRouterDrawer() {
  const [sp, setSp] = useSearchParams();
  const id = sp.get('r');
  const raw = sp.get('rtab');
  const tab: RouterTab = ROUTER_TABS.some((t) => t.value === raw) ? (raw as RouterTab) : 'overview';
  // 同 useDrawer：setSearchParams 每次地址变化都换身份，用 ref 兜住，回调才稳定（卡片 memo 依赖它）
  const setRef = useRef(setSp); setRef.current = setSp;
  // 打开时收起机器抽屉（告警页上开着机器抽屉时点了路由的告警）：两个抽屉不叠在一起
  const open = useCallback((rid: string, t?: RouterTab) => setRef.current((p) => { const n = new URLSearchParams(p); n.delete('m'); n.delete('tab'); n.set('r', rid); if (t) n.set('rtab', t); else n.delete('rtab'); return n; }), []);
  const setTab = useCallback((t: RouterTab) => setRef.current((p) => { const n = new URLSearchParams(p); n.set('rtab', t); return n; }, { replace: true }), []);
  const close = useCallback(() => setRef.current((p) => { const n = new URLSearchParams(p); n.delete('r'); n.delete('rtab'); return n; }), []);
  /** 出口对应的 fleet 入口：换成那台机器的抽屉（压一条历史，后退回到这台路由） */
  const openMachine = useCallback((mid: string) => setRef.current((p) => { const n = new URLSearchParams(p); n.delete('r'); n.delete('rtab'); n.delete('tab'); n.set('m', mid); return n; }), []);
  return { id, tab, open, setTab, close, openMachine };
}

/** 告警对象是路由：规则以 router_ 开头（subject.kind = "router"），点对象要开路由抽屉 */
export const isRouterIncident = (i: { rule: string }) => i.rule.startsWith('router_');

// ---- 接入命令 / 删除后的说明：页面、卡片、抽屉都能弹，由外壳里那一份 RouterDialogs 渲染 ----
/** issued_at：拿到命令的时刻，路由在这之后连上才算「已接入」（重新生成时旧隧道之前也是在线的）；
 *  replaced：这台连上过，旧隧道密钥随之作废（对话框里提醒原来的 fleet-tunnel 会断开） */
export interface BootTarget { id: string; name: string; cmd: string; expires_at: string; issued_at: string; replaced: boolean }
const boot = createStore<BootTarget | null>(null);
export const showBootstrap = boot.set;
export const closeBootstrap = () => boot.set(null);
export const useBootTarget = boot.use;
/** 一键部署对话框（重新部署）：SSH 地址预填这个浏览器上次给这台填的（中控不回传地址，密码从不保存） */
export interface DeployTarget { id: string; name: string }
const deploy = createStore<DeployTarget | null>(null);
export const showDeploy = deploy.set;
export const closeDeploy = () => deploy.set(null);
export const useDeployTarget = deploy.use;
/** 部署时间线：服务端只给当前步骤，走过的步骤由前端在推送里看到时记下（按这次部署的开始时刻区分；刷新页面后只剩当前这一步）。
 *  渲染时调用，同一步重复看到不重复记 */
const seenSteps = new Map<string, { step: string; at: number }[]>();
export function deploySteps(id: string, d: RouterDeploy) {
  const k = `${id}:${d.started_at}`;
  let l = seenSteps.get(k);
  if (!l) seenSteps.set(k, (l = []));
  // 不在跑了才第一次看到（刷新页面后）：时刻记成结束的时刻
  if (d.step && l[l.length - 1]?.step !== d.step) l.push({ step: d.step, at: d.state === 'running' ? Date.now() : Date.parse(d.finished_at ?? d.started_at) });
  return l;
}
/** 删掉的路由名：弹出「在路由上停掉 fleet-tunnel」的说明 */
const removed = createStore<string | null>(null);
export const showRemoved = removed.set;
export const closeRemoved = () => removed.set(null);
export const useRemoved = removed.use;
/** 删除后路由上的清理：停掉并取消开机自启，再删服务脚本与隧道密钥（中控已撤销公钥，它也连不上了） */
export const TUNNEL_CLEANUP = '/etc/init.d/fleet-tunnel stop; /etc/init.d/fleet-tunnel disable; rm -rf /etc/init.d/fleet-tunnel /etc/fleet-tunnel';

// ---- 读数 ----
const ratio = (u: number, t: number) => (t > 0 ? (u / t) * 100 : null);
/** 资源四项（卡片格条、抽屉「当前」共用）：负载按核数折成百分比（同机器卡片），连接表是 conntrack 当前 / 上限 */
export function routerMeters(s: RouterSummary) {
  const memPct = ratio(s.mem_used, s.mem_total); const diskPct = ratio(s.disk_used, s.disk_total); const ctPct = ratio(s.conntrack, s.conntrack_max);
  const loadPct = s.cores ? (s.load1 / s.cores) * 100 : null;
  return {
    cpu: { value: F.pct(s.cpu_pct), pct: s.cpu_pct, sub: `负载 ${F.str(F.pct(loadPct))} · ${s.cores} 核`, tip: `负载 = 1 分钟平均负载 ${s.load1.toFixed(2)} ÷ ${s.cores} 核；超过 100% 表示有进程在排队` },
    mem: { value: F.pct(memPct), pct: memPct, sub: `${F.str(F.mem(s.mem_used))} / ${F.str(F.mem(s.mem_total))}` },
    disk: { value: F.pct(diskPct), pct: diskPct, sub: `${F.str(F.mem(s.disk_used))} / ${F.str(F.mem(s.disk_total))}`, tip: '/overlay（装的软件与配置）的已用 / 总量' },
    ct: { value: F.pct(ctPct), pct: ctPct, sub: `${s.conntrack.toLocaleString()} / ${s.conntrack_max.toLocaleString()}`, tip: '连接跟踪表（conntrack）当前条目 / 上限：满了新连接会被丢弃' },
  };
}
/** 运行时长（秒）→ 天 / 小时 / 分钟 */
export const uptimeFmt = (s: number | null | undefined): F.Fmt => (s == null ? F.NA : s < 3600 ? { v: String(Math.max(1, Math.floor(s / 60))), u: '分钟' } : s < 86400 ? { v: String(Math.floor(s / 3600)), u: '小时' } : { v: String(Math.floor(s / 86400)), u: '天' });
export const tempTone = (t: number | null | undefined) => (t == null ? undefined : t >= 85 ? 'bad' : t >= 70 ? 'warn' : undefined);
/** 在线但有毛病（卡片描边变黄）：OpenClash 启用却没在跑、WAN 断了、有出口测试失败 */
export const routerIssues = (s: RouterSummary | null): string[] => (!s ? [] : [
  s.openclash.enabled && !s.openclash.running && 'OpenClash 已启用但核心没在运行',
  !s.wan.up && 'WAN 口断开',
  s.exits.alive < s.exits.total && `${s.exits.total - s.exits.alive} 个出口延迟测试失败`,
].filter((x): x is string => !!x));
