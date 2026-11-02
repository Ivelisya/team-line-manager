// 软路由（客户现场的 OpenWrt + OpenClash）：类型、查询与写操作、抽屉开关、接入命令对话框的状态。
// 中控经反向隧道每 30 秒在路由上跑一次只读采集，结果随 {"type":"routers"} 事件推来（eventKeys 让 ['routers'] 下的列表与详情一起重取），
// 所以列表与详情 staleTime 都是 Infinity；曲线不推，抽屉开着时按范围轮询（同机器抽屉的曲线）。形状见 r21 设计稿「API」。
import { useCallback, useMemo, useRef } from 'react';
import { useSearchParams, type SetURLSearchParams } from 'react-router-dom';
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { del, get, patch, post } from './api';
import { useRelays, useTeams, useWrite } from './queries';
import { collator } from './sort';
import { createStore } from './store';
import type { EntryLifecycle, SeriesPoint, Team } from './types';
import type { BeatCell } from './derive';
import { chartPoll, RANGE_MS } from './chart';
import * as F from './format';

/** pending = 建了记录、路由还没连上过（或重新接入后还没连上）；offline = 连上过、现在隧道不通（连得上但采集出错算 online，带 error） */
export type RouterState = 'pending' | 'online' | 'offline';
/** 列表里的摘要：最近一次采集成功的快照，从没采集成功过为 null。速率是 bit/s，内存 / 存储是字节 */
export interface RouterSummary {
  uptime_s: number; cpu_pct: number; load1: number; cores: number; mem_used: number; mem_total: number; disk_used: number; disk_total: number;
  conntrack: number; conntrack_max: number; temp_c: number | null;
  wan: { up: boolean; ip: string | null; rx_bps: number; tx_bps: number };
  openclash: { enabled: boolean; running: boolean; version: string | null; mode: string | null; conns: number; up_bps: number; down_bps: number };
  /** total / alive / dead 含客户自己加的出口；managed：server:port 对上了 fleet 入口的出口；drift：其中参数与入口当前参数不一致的；dead：不通（连续两次以上测不通，同 exitUsable）的出口名。
   *  fleet：只数 managed 的那些（告警、卡片、降级看它），other：客户自己加的出口个数 */
  exits: { total: number; alive: number; dead: string[]; managed: number; drift: number; fleet: { total: number; alive: number; dead: string[] }; other: number };
  clients: number;
}
/** 一键部署（中控 SSH 到路由上替你执行接入）的进度：最近一次的，从没部署过为 null。step 是当前步骤（中文） */
export interface RouterDeploy { state: 'running' | 'failed' | 'done'; step: string | null; error: string | null; started_at: string; finished_at: string | null }
export interface RouterView {
  id: string; name: string; team: { id: string; name: string } | null; note: string; tunnel_port: number; created_at: string;
  /** offline / pending 时是连不上的原因；online 时有值 = 连得上但采集出错（summary / detail 是上一次成功采集的） */
  state: RouterState; last_seen_at: string | null; error: string | null; summary: RouterSummary | null; deploy: RouterDeploy | null;
  /** 在维护窗口里（自己的或「全部」的）：开着的告警保持、不开新的、不发通知 */
  maintenance: boolean;
  /** 进行中的临时切换（select 组经 mihomo API 切到组里另一个出口；from 是第一次切之前选中的，切回到它）。旧中控没有这个字段 */
  switches?: RouterSwitch[];
  /** 出口对上的入口（没对上的不列；没采集成功过为空）：「出口 → 入口」的对应读它，不必逐台取详情 */
  exit_entries: { exit: string; entry_id: string }[];
}
/** defer_why / deferred_at：到时没切回的原因（原出口还没恢复）与第一次推迟的时刻，此时 back_at 是下次检查的时刻；没推迟过为 null，旧中控没有这两个字段 */
export interface RouterSwitch { group: string; from: string; to: string; back_at: string | null; by: string; switched_at: string; defer_why?: string | null; deferred_at?: string | null }
/** 策略组：type 是 mihomo 的类型，只有 Selector（select）能手动切换；now 是当前选中的；used === false 是没有规则用它（切了不影响流量，不给切换），没有这个字段 = 不知道（旧快照、没取到规则） */
export interface RouterGroup { name: string; type: string; now: string | null; all: string[]; used?: boolean }
export interface RouterIface { name: string; device: string; up: boolean; proto: string; ip: string | null; uptime_s: number | null; rx_bps: number; tx_bps: number; rx_bytes: number; tx_bytes: number }
/** ok：参数与 fleet 入口一致；drift：server:port 对上了，但 uuid / reality 参数不一致；unmanaged：没对上任何 fleet 入口 */
export type ExitSync = 'ok' | 'drift' | 'unmanaged';
export interface RouterExit {
  name: string; type: string; server: string; port: number;
  /** 协议一个词：reality（VLESS Reality）/ anytls，配置里读到的 fleet 协议才有；别的协议、旧中控为空。AnyTLS 出口的 port 是入口的 AnyTLS 端口 */
  proto?: 'reality' | 'anytls' | null;
  /** lifecycle：对上的入口现在的状态（已停用、已删除的对不上，sync 为 drift） */
  entry: { id: string; name: string; node_id: string; relay_id: string; lifecycle: EntryLifecycle } | null; sync: ExitSync;
  /** alive / delay_ms 来自每 5 分钟一次的延迟测试（delay_at 是测试时刻）；还没测过为 null。fail_streak：连续几次测不通。conns 等是 mihomo 连接的汇总 */
  alive: boolean | null; delay_ms: number | null; delay_at: string | null; fail_streak: number; conns: number; up_bps: number; down_bps: number; clients: number;
}
/** 出口算不算可用（同服务端摘要的 alive 与告警）：连续两次测不通才不算，只不通 1 次的还算 */
export const exitUsable = (e: RouterExit) => e.alive !== false || e.fail_streak < 2;
export interface RouterClient { ip: string; mac: string | null; hostname: string | null; online: boolean; conns: number; down_bps: number | null; up_bps: number | null; exits: { name: string; conns: number }[] }
export interface RouterDetail {
  system: { os: string; kernel: string; cpu_model: string; cores: number; load: [number, number, number]; procs: number; clock_skew_s: number | null; clash_rss: number | null };
  interfaces: RouterIface[];
  /** config_error：配置文件取不回、解析不了；config_stale：这时出口与入口的对应沿用上次读到的。delay_error：最近一次延迟测试没做成的原因 */
  openclash: { enabled: boolean; running: boolean; version: string | null; mode: string | null; config_path: string | null; config_mtime: string | null; config_error?: string | null; config_stale?: boolean; delay_error?: string | null };
  exits: RouterExit[]; clients: RouterClient[]; collected_at: string;
  /** mihomo 的 API 调不通时为空；旧中控没有这个字段 */
  groups?: RouterGroup[];
}
/** 详情：没采集成功过（pending）时 detail 为 null */
export type RouterFull = RouterView & { detail: RouterDetail | null };
export interface Bootstrap { cmd: string; expires_at: string }
/** 曲线：点里的字段 cpu_pct load1 mem_pct disk_pct conntrack_pct temp_c wan_rx_bps wan_tx_bps proxy_up_bps proxy_down_bps conns clients exits_alive fleet_alive（fleet 出口里可用的，曲线画它；旧点退回 exits_alive） */
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
/** 曲线的轮询间隔随范围放大（`chartPoll`，同机器抽屉）；换范围时先留着上一段（调用方按 isPlaceholderData 调淡），换了路由不沿用 */
export const useRouterMetrics = (id: string, range: string) =>
  useQuery({ queryKey: ['router-metrics', id, range], queryFn: ({ signal }) => get<RouterMetrics>(`/routers/${id}/metrics?range=${range}`, signal), staleTime: chartPoll(range), refetchInterval: chartPoll(range), refetchOnWindowFocus: false, placeholderData: (prev, pq) => (pq?.queryKey[1] === id ? keepPreviousData(prev) : undefined) });
/** 配置变更（GET /routers/{id}/changes，新的在前，最多 200 条）：source = job 是中控作业改的（job_id / job_kind），router 是路由上改的；
 *  diff 是出口的增删改（changed 在 server:port 变了时带 from / to，凭据变了 creds = true，凭据本身不出）；note 是比不出差异的原因 */
export interface RouterChangeItem { exit: string; op: 'added' | 'removed' | 'changed'; from?: string; to?: string; creds?: boolean }
export interface RouterChange { id: string; at: string; old_sha: string; new_sha: string; source: 'job' | 'router'; job_id: string | null; job_kind: string | null; diff: RouterChangeItem[]; note: string | null }
/** 变更随 ['routers'] 的实时事件重取（配置变了采集会推一次） */
export const useRouterChanges = (id: string) =>
  useQuery({ queryKey: ['routers', id, 'changes'], queryFn: ({ signal }) => get<RouterChange[]>(`/routers/${id}/changes`, signal), staleTime: Infinity });
export const useRouterExitMetrics = (id: string, range: string) =>
  useQuery({ queryKey: ['router-exit-metrics', id, range], queryFn: ({ signal }) => get<RouterExitMetrics>(`/routers/${id}/exits/metrics?range=${range}`, signal), staleTime: chartPoll(range), refetchInterval: chartPoll(range), refetchOnWindowFocus: false, placeholderData: (prev, pq) => (pq?.queryKey[1] === id ? keepPreviousData(prev) : undefined) });

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
/** 有没做完的作业改过它的出口、没确认改回时 409（错误信息直接给用户看）；force：照删，这些作业不再改回它（返回摘掉了哪些作业） */
export const useDeleteRouter = () => useWrite(({ id, force }: { id: string; force?: boolean }) => del<{ jobs?: string[] }>(`/routers/${id}${force ? '?force=true' : ''}`), K);
/** 同步出口参数的预览（勾选「仍要同步」换了查询键：新结果回来前沿用上一份，调用方按 isPlaceholderData 调淡并禁用确认，对话框内容不卸载）：要改的出口（from / to 都是 server:port，不变，换的是凭据）、同步不了的（入口不在用）、不能做的原因 */
export interface SyncPreview {
  ok: boolean; blockers: string[]; skipped: string[]; warnings: string[]; routers: { exits: { exit: string; from: string; to: string | null }[] }[];
  /** 从没对上过入口的出口（路由上有人加的？）：不点名时不同步 */
  untrusted: { exit: string; entry: string }[];
  /** 点名要同步、但从没对上过入口、入口又不属于这台路由的团队的：没有 force 进 blockers */
  need_force: { exit: string; entry: string; why: string }[];
}
export const useSyncPreview = (id: string | null, exits: string[], force: boolean) =>
  useQuery({ queryKey: ['router-sync', id, exits.join(','), force], queryFn: ({ signal }) => get<SyncPreview>(`/routers/${id}/sync?exits=${encodeURIComponent(exits.join(','))}${force ? '&force=true' : ''}`, signal), enabled: !!id, staleTime: 0, gcTime: 0, placeholderData: (p) => p });
/** 发起同步作业（router_sync：备份、改、重启 OpenClash、测延迟，失败自动改回） */
export const useSyncRouter = () => useWrite(({ id, exits, force }: { id: string; exits: string[]; force?: boolean }) => post<{ job_id: string }>(`/routers/${id}/sync`, { exits, force: !!force }), [...K, ['jobs']]);
/** 临时切换：back_after_s 不给 = 不自动切回。返回切之前选中的（from）与第一次切之前的（original） */
export const useSwitchGroup = () => useWrite(({ id, ...b }: { id: string; group: string; to: string; back_after_s?: number }) => post<{ from: string; to: string; original: string; back_at: string | null; ended: boolean }>(`/routers/${id}/switch`, b), K);
export const useSwitchBack = () => useWrite(({ id, group }: { id: string; group: string }) => post<{ from: string; to: string }>(`/routers/${id}/switch/back`, { group }), K);

// ---- 抽屉：?r=<路由id>&rtab=<页签>，与机器抽屉（?m=）各管各的 ----
export type RouterTab = 'overview' | 'exits' | 'clients' | 'charts' | 'changes' | 'manage';
/** 同机器抽屉：操作（改名、接入命令、重新部署、终端、删除）收在最后的「管理」页签 */
export const ROUTER_TABS: { value: RouterTab; label: string }[] = [
  { value: 'overview', label: '概览' }, { value: 'exits', label: '出口' }, { value: 'clients', label: '客户端' }, { value: 'charts', label: '曲线' }, { value: 'changes', label: '变更' }, { value: 'manage', label: '管理' },
];

// 上一台 / 下一台的顺序：软路由页把当前筛选后的顺序交过来（同 setDrawerOrder），离开页面时清掉；没有时按列表的默认顺序
let order: readonly string[] | null = null;
export const setRouterOrder = (ids: readonly string[] | null) => { order = ids; };
/** 抽屉头部的上一台 / 下一台：到头为 null，不绕回；这台不在顺序里时返回 undefined（不显示按钮） */
export function routerSiblings(list: readonly { id: string }[] | undefined, id: string | null): { prev: string | null; next: string | null } | undefined {
  if (!id) return undefined;
  const ids = order?.includes(id) ? order : list?.map((r) => r.id);
  const i = ids ? ids.indexOf(id) : -1;
  return !ids || i < 0 ? undefined : { prev: i > 0 ? ids[i - 1] : null, next: i < ids.length - 1 ? ids[i + 1] : null };
}
// 同 lib/drawer.ts：useRouterDrawer 渲染时记下地址参数的写函数，模块级的 openRouterDrawer 用它（入口行的「软路由」小标等成批的小按钮不必各自订阅地址）
let setLatest: { current: SetURLSearchParams } | null = null;
// 打开时收起机器抽屉（告警页上开着机器抽屉时点了路由的告警）：两个抽屉不叠在一起
const openWith = (set: SetURLSearchParams, rid: string, t?: RouterTab) => set((p) => { const n = new URLSearchParams(p); n.delete('m'); n.delete('tab'); n.delete('range'); n.set('r', rid); if (t) n.set('rtab', t); else n.delete('rtab'); return n; });
/** 打开软路由抽屉（不订阅地址）：外壳里一直挂着 useRouterDrawer，调用时总有写函数 */
export const openRouterDrawer = (rid: string, t?: RouterTab) => { if (setLatest) openWith(setLatest.current, rid, t); };
export function useRouterDrawer() {
  const [sp, setSp] = useSearchParams();
  const id = sp.get('r');
  const raw = sp.get('rtab');
  const tab: RouterTab = ROUTER_TABS.some((t) => t.value === raw) ? (raw as RouterTab) : 'overview';
  // 同 useDrawer：setSearchParams 每次地址变化都换身份，用 ref 兜住，回调才稳定（卡片 memo 依赖它）
  const setRef = useRef(setSp); setRef.current = setSp; setLatest = setRef;
  const open = useCallback((rid: string, t?: RouterTab) => openWith(setRef.current, rid, t), []);
  const setTab = useCallback((t: RouterTab) => setRef.current((p) => { const n = new URLSearchParams(p); n.set('rtab', t); return n; }, { replace: true }), []);
  /** 开着抽屉换一台（头部的上一台 / 下一台）：留在当前页签，替换当前历史，后退仍是一步关掉 */
  const switchTo = useCallback((rid: string) => setRef.current((p) => { const n = new URLSearchParams(p); n.set('r', rid); return n; }, { replace: true }), []);
  const close = useCallback(() => setRef.current((p) => { const n = new URLSearchParams(p); n.delete('r'); n.delete('rtab'); n.delete('range'); return n; }), []);
  /** 出口对应的 fleet 入口：换成那台机器的抽屉（压一条历史，后退回到这台路由） */
  const openMachine = useCallback((mid: string) => setRef.current((p) => { const n = new URLSearchParams(p); n.delete('r'); n.delete('rtab'); n.delete('tab'); n.delete('range'); n.set('m', mid); return n; }), []);
  return { id, tab, open, setTab, switchTo, close, openMachine };
}

/** 告警对象是路由：软路由引擎写的（engine = "router"，subject.kind 同为 "router"），点对象要开路由抽屉 */
export const isRouterIncident = (i: { engine: string }) => i.engine === 'router';
/** 点告警对象开路由抽屉的哪一页：出口不通的直接开出口页签，其余开概览 */
export const routerIncidentTab = (i: { rule: string }): RouterTab | undefined => (i.rule === 'router_exits_down' || i.rule === 'router_exit_drift' ? 'exits' : undefined);

// ---- 同步出口参数、临时切换：抽屉与告警页都能弹，由外壳里那一份 RouterOpsDialogs 渲染 ----
export interface SyncTarget { id: string; name: string; exits: string[] }
const syncT = createStore<SyncTarget | null>(null);
export const showSync = syncT.set;
export const useSyncTarget = syncT.use;
/** group / to 给了就预选（告警的「临时切到…」） */
export interface SwitchTarget { id: string; name: string; group?: string; to?: string }
const switchT = createStore<SwitchTarget | null>(null);
export const showSwitch = switchT.set;
export const useSwitchTarget = switchT.use;

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
/** 部署时间线：服务端只给当前步骤，走过的步骤由前端在推送里看到时记下（刷新页面后只剩当前这一步）。
 *  每台路由只留最近一次部署（按开始时刻认），换了一次部署就丢掉上一次的，不越攒越多。渲染时调用，同一步重复看到不重复记 */
const seenSteps = new Map<string, { started: string; steps: { step: string; at: number }[] }>();
export function deploySteps(id: string, d: RouterDeploy) {
  let e = seenSteps.get(id);
  if (e?.started !== d.started_at) seenSteps.set(id, (e = { started: d.started_at, steps: [] }));
  const l = e.steps;
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
    cpu: { value: F.pct(s.cpu_pct), pct: s.cpu_pct, sub: `负载 ${F.str(F.pct(loadPct))} · ${s.cores} 核`, tip: `1 分钟平均负载 ${s.load1.toFixed(2)} 除以 ${s.cores} 核。超过 100% 说明有进程在排队` },
    mem: { value: F.pct(memPct), pct: memPct, sub: `${F.str(F.mem(s.mem_used))} / ${F.str(F.mem(s.mem_total))}` },
    disk: { value: F.pct(diskPct), pct: diskPct, sub: `${F.str(F.mem(s.disk_used))} / ${F.str(F.mem(s.disk_total))}`, tip: '存放软件和配置的存储空间（/overlay）' },
    ct: { value: F.pct(ctPct), pct: ctPct, sub: `${s.conntrack.toLocaleString()} / ${s.conntrack_max.toLocaleString()}`, tip: '连接表的已用 / 上限。满了之后，新连接会被丢弃' },
  };
}
/** 运行时长（秒）→ 天 / 小时 / 分钟 */
export const uptimeFmt = (s: number | null | undefined): F.Fmt => (s == null ? F.NA : s < 3600 ? { v: String(Math.max(1, Math.floor(s / 60))), u: '分钟' } : s < 86400 ? { v: String(Math.floor(s / 3600)), u: '小时' } : { v: String(Math.floor(s / 86400)), u: '天' });
export const tempTone = (t: number | null | undefined) => (t == null ? undefined : t >= 85 ? 'bad' : t >= 70 ? 'warn' : undefined);
/** 出口的可用 / 总数与不通名单：只看 fleet 出口（对应面板入口的，同告警 router_exits_down），客户自己加的出口单列 other，不通也不算降级。
 */
export const fleetExits = (s: RouterSummary) => ({ ...s.exits.fleet, other: s.exits.other });
/** 在线但有毛病（卡片描边变黄，状态叫「降级」）：OpenClash 启用却没在跑、WAN 断了、有 fleet 出口测试不通（客户自己的出口不算）。不通的多于 3 个只列前 3 个 */
export const routerIssues = (s: RouterSummary | null): string[] => {
  if (!s) return [];
  const ex = fleetExits(s); const names = ex.dead;
  return [
    s.openclash.enabled && !s.openclash.running && 'OpenClash 已启用但没在运行',
    !s.wan.up && 'WAN 口断开',
    ex.alive < ex.total && (names.length ? `${names.slice(0, 3).join('、')}${names.length > 3 ? ` 等 ${names.length} 个出口` : ''} 连续两次延迟测试不通` : `${ex.total - ex.alive} 个出口连续两次延迟测试不通`),
  ].filter((x): x is string => !!x);
};

// ---- 各出口的延迟测试：按固定时间窗切格 ----
/** 各范围切几格：每格要稳稳盖住至少一次测试（5 分钟一次，实际间隔 300–330 秒），格宽取 2 倍以上，所以 1 小时只切 6 格（10 分钟一格） */
const BEAT_BINS: Record<string, number> = { '1h': 6, '6h': 24, '24h': 24, '7d': 28, '30d': 30 };
export const beatBins = (range: string) => BEAT_BINS[range] ?? 24;
/** 一个出口在所选范围内的测试结果切成 bins 格（从 end 往前，每格一样宽，不按数据的首末）：
 *  颜色只看成功率——全成功绿、有失败黄、失败过半红；这格里没有测试为空（路由还没接入、离线）。延迟高低看延迟曲线，不进格子。
 *  7 天 / 30 天的点是一小时的汇总，带 alive_pct（这一小时测通的比例）。
 *  from：数据的起点；晚于所选范围的起点（新接入）就从它起切，不让前面一大段斜纹挤掉真正的数据。rate：整个窗口的测通比例，没有数据为 null */
export function exitBeats(points: SeriesPoint[], range: string, bins: number, end: number, from?: number): { cells: BeatCell[]; titles: string[]; rate: number | null } {
  const span0 = RANGE_MS[range] ?? RANGE_MS['6h'];
  const width = (from != null && from > end - span0 ? end - from : span0) / bins; const start = end - width * bins;
  const acc = Array.from({ length: bins }, () => ({ n: 0, ok: 0, sum: 0, sumN: 0 }));
  for (const p of points) {
    const i = Math.min(bins - 1, Math.floor((Date.parse(p.ts) - start) / width));
    if (i < 0) continue;
    const b = acc[i]; const ok = typeof p.alive_pct === 'number' ? p.alive_pct / 100 : typeof p.delay_ms === 'number' ? 1 : 0;
    b.n++; b.ok += ok;
    if (typeof p.delay_ms === 'number') { b.sum += p.delay_ms; b.sumN++; }
  }
  const cells: BeatCell[] = []; const titles: string[] = [];
  const total = acc.reduce((s, b) => s + b.n, 0);
  acc.forEach((b, i) => {
    const t0 = new Date(start + i * width).toISOString(); const t1 = new Date(start + (i + 1) * width).toISOString();
    const span = `${F.dt(t0)} – ${F.dt(t1).slice(6)}`;
    if (!b.n) { cells.push(null); titles.push(`${span}\n没有数据`); return; }
    const rate = b.ok / b.n;
    cells.push(rate === 1 ? 'ok' : rate < 0.5 ? 'bad' : 'warn');
    titles.push(`${span}\n${Number.isInteger(b.ok) ? `成功 ${b.ok}/${b.n}` : `测通 ${Math.round(rate * 100)}%`}${b.sumN ? ` · 平均 ${Math.round(b.sum / b.sumN)} ms` : ''}`);
  });
  return { cells, titles, rate: total ? acc.reduce((s, b) => s + b.ok, 0) / total : null };
}

// ---- 地址 → 线路机名 / 入口被哪些软路由出口在用 ----
/** 预览与作业结果里的「server:port」：server 是某台线路机的 IP 或公网地址的，换成线路机名（原地址放悬停），认不得的原样 */
export function useAddrName() {
  const relays = useRelays().data;
  const names = useMemo(() => new Map((relays ?? []).flatMap((r) => [[r.ip, r.name] as const, ...(r.relay ? [[r.relay.public_host, r.name] as const] : [])])), [relays]);
  return useCallback((addr: string) => {
    const i = addr.lastIndexOf(':'); const host = i < 0 ? addr : addr.slice(0, i);
    const n = names.get(host);
    return { text: n ? `${n}${i < 0 ? '' : addr.slice(i)}` : addr, tip: n ? addr : undefined };
  }, [names]);
}
/** 入口 id → 在用它的软路由出口（软路由 + 出口名）：列表里每台带出口 → 入口的对应（exit_entries），抽屉入口表、线路页的入口行用它标「软路由」。
 *  不再为它逐台取完整详情；select 只抽这一项，结构相等时引用不变（react-query 的结构共享），列表每轮采集重取、没人改出口 / 入口时 Map 不重建，入口行（EntryRow 的 memo）不被击穿 */
export function useEntryRouters() {
  const uses = useQuery({ queryKey: ['routers', 'list'], queryFn: ({ signal }) => get<RouterView[]>('/routers', signal), staleTime: Infinity, select: exitEntries }).data;
  return useMemo(() => {
    const m = new Map<string, { id: string; name: string; exit: string }[]>();
    for (const r of uses ?? []) for (const e of r.exits) m.set(e.entry_id, [...(m.get(e.entry_id) ?? []), { id: r.id, name: r.name, exit: e.exit }]);
    return m;
  }, [uses]);
}
const exitEntries = (d: RouterView[]) => d.filter((r) => r.exit_entries.length).map((r) => ({ id: r.id, name: r.name, exits: r.exit_entries }));
