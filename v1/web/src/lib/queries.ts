// TanStack Query 封装：查询 hook + 写操作 hook。写操作成功后按范围失效（实时流也会再推一次，重复无害）。
// 内容只会因实时流事件而变的查询（eventKeys 覆盖到的）staleTime 设成 Infinity：切回窗口、重新挂载都不再重取，靠事件失效；
// 断线重连时 stream.ts 会让全部查询失效一次，没连着时错过的变化照样补上。
import { useCallback } from 'react';
import { keepPreviousData, useInfiniteQuery, useMutation, useQuery, useQueryClient, type QueryClient } from '@tanstack/react-query';
import { del, get, patch, post, put } from './api';
import { collator, makeDefaultCompare } from './sort';
import { offlineNames, quotaTight, setMeta } from './derive';
import type { ApiKey, Availability, Incident, Job, JobSummary, Paged, JobLog, MachineView, Meta, Overview, Plan, Provider, ProvisionCheckItem, ProvisionItem, Renewal, RenumberPreview, RetirePreview, RetireResult, RestoreBody, RestorePreview, MoveResult, EvacuateResult, ProbeBeats, SeriesResp, Silence, SubField, Team, AuditEntry, NotifyChannel, NotifyLogEntry, AccountSession, TotpSetup, TotpStatus } from './types';

export const useMe = () => useQuery({ queryKey: ['me'], queryFn: () => get<{ username: string }>('/auth/me'), retry: false, staleTime: 5 * 60_000 });
/** 中文名、枚举、阈值、默认值（服务端 meta.rs），一次会话内不变：取回时同时交给 derive 的同步读取（kindLabel 等）。外壳等它到了才渲染页面 */
export const useMeta = (enabled: boolean) => useQuery({
  queryKey: ['meta'], queryFn: () => get<Meta>('/meta').then((m) => { setMeta(m); return m; }), enabled, staleTime: Infinity, gcTime: Infinity,
});
// 团队、套餐带着由机器算出来的台数（node_count / machine_count），机器增减不发 catalog 事件，仍按时间过期；供应商只随 catalog 事件变
export const useTeams = () => useQuery({ queryKey: ['teams'], queryFn: () => get<Team[]>('/teams'), staleTime: 60_000 });
export const useProviders = () => useQuery({ queryKey: ['providers'], queryFn: () => get<Provider[]>('/providers'), staleTime: Infinity });
export const usePlans = () => useQuery({ queryKey: ['plans'], queryFn: () => get<Plan[]>('/plans'), staleTime: 60_000 });
export const useOverview = () => useQuery({ queryKey: ['overview'], queryFn: ({ signal }) => get<Overview>('/overview', signal) });

const machinesQuery = (scope: 'live' | 'retired') => ({ queryKey: ['machines', scope], queryFn: ({ signal }: { signal: AbortSignal }) => get<MachineView[]>(`/machines?scope=${scope}`, signal) });
/** 按默认顺序排好的副本（线路机 → 团队 → 编号，故障/离线不下沉）：机器列表的几个 select 与 readMachines 共用 */
const sortDefault = (d: MachineView[], teams: Team[] | undefined) => [...d].sort(makeDefaultCompare(teams ?? []));
/** 机器列表，已按默认顺序排好 */
export function useMachines(scope: 'live' | 'retired' = 'live') {
  const teams = useTeams().data;
  // select 排序：只在列表或团队变化时重算；返回的是查询结果本身，调用方按属性追踪
  const select = useCallback((d: MachineView[]) => sortDefault(d, teams), [teams]);
  return useQuery({ ...machinesQuery(scope), select });
}
/** 只用名称等少量字段的页面（访问监控的节点下拉、告警页的对象与续费）：读数不进来 */
export type MachineBrief = Pick<MachineView, 'id' | 'name' | 'kind' | 'team' | 'subscription'>;
const brief = (m: MachineView): MachineBrief => ({ id: m.id, name: m.name, kind: m.kind, team: m.team, subscription: m.subscription });
/** 命令面板：搜索（名称 / IP / 团队 / 类型）与健康标用到的字段 */
export type MachineLabel = Pick<MachineView, 'id' | 'name' | 'ip' | 'country' | 'country_set' | 'kind' | 'team' | 'health' | 'lifecycle' | 'maintenance'>;
const label = (m: MachineView): MachineLabel => ({ id: m.id, name: m.name, ip: m.ip, country: m.country, country_set: m.country_set, kind: m.kind, team: m.team, health: m.health, lifecycle: m.lifecycle, maintenance: m.maintenance });
/** 与 useMachines 同一份缓存、同样的默认顺序，每台只取 pick 出的字段（pick 须是模块常量）。
 *  select 的结果做结构共享：实时推送只改读数时这些字段不变，引用不变，页面不重渲染 */
function useMachinesPicked<T>(pick: (m: MachineView) => T) {
  const teams = useTeams().data;
  const select = useCallback((d: MachineView[]) => sortDefault(d, teams).map(pick), [teams, pick]);
  return useQuery({ ...machinesQuery('live'), select });
}
export const useMachineBriefs = () => useMachinesPicked(brief);
export const useMachineLabels = () => useMachinesPicked(label);
/** 上架页「在已有机器上添加 IP」的宿主机候选：在用、自身不是同机 IP 节点的 VPS 节点，带出它的团队、线路机（第一个入口的）、SSH 端口与供应商。
 *  同一份机器列表缓存，结果做结构共享：推送只改读数时引用不变（批量网格的 memo 不被击穿） */
export interface HostChoice { id: string; name: string; ip: string; team_id: string; relay_id: string; ssh_port: number; provider: string | null }
const hostChoices = (d: MachineView[]) => d.filter((m) => m.kind === 'vps' && m.lifecycle === 'active' && !m.host_machine)
  .map((m): HostChoice => ({ id: m.id, name: m.name, ip: m.ip, team_id: m.team?.id ?? '', relay_id: m.entries[0]?.relay.id ?? '', ssh_port: m.ssh_port, provider: m.provider?.name ?? null }))
  .sort((a, b) => collator.compare(a.name, b.name));
export const useHostChoices = (enabled: boolean) => useQuery({ ...machinesQuery('live'), select: hostChoices, enabled });
/** 团队与套餐页：各团队的编号是否已经连续（01..N），已连续时「重新编号」不可点。范围同服务端重新编号：团队里未退役的非线路机 */
const numbersCompact = (d: MachineView[]) => {
  const by: Record<string, number[]> = {};
  for (const m of d) if (m.team && m.kind !== 'relay' && m.lifecycle !== 'retired' && m.number != null) (by[m.team.id] ??= []).push(m.number);
  return Object.fromEntries(Object.entries(by).map(([t, ns]) => [t, ns.sort((a, b) => a - b).every((n, i) => n === i + 1)]));
};
export const useTeamsCompact = () => useQuery({ ...machinesQuery('live'), select: numbersCompact });
/** 首页 KPI「流量额度」：同一份机器列表缓存，只取吃紧的台数与最紧的一台；结果做结构共享，没变时首页总览不跟着推送重渲染 */
export const useQuotaTight = () => useQuery({ ...machinesQuery('live'), select: quotaTight });
/** 集群状态在线率格条的悬停：离线机器的名字。同一份缓存，结构共享：没人上下线时引用不变 */
export const useOfflineNames = () => useQuery({ ...machinesQuery('live'), select: offlineNames });
/** readMachines 的排序结果按原数组引用缓存：抽屉每次渲染都要算上一台 / 下一台，列表没换（推送间隙）就不再复制、排序整表 */
const sortedCache = new WeakMap<MachineView[], { teams: Team[] | undefined; list: MachineView[] }>();
/** 按键等“用时才读”的场景：直接读缓存里的机器列表（同样的默认顺序），不订阅、不触发请求 */
export function readMachines(qc: QueryClient): MachineView[] | undefined {
  const raw = qc.getQueryData<MachineView[]>(['machines', 'live']);
  if (!raw) return undefined;
  const teams = qc.getQueryData<Team[]>(['teams']);
  const hit = sortedCache.get(raw);
  if (hit && hit.teams === teams) return hit.list;
  const list = sortDefault(raw, teams);
  sortedCache.set(raw, { teams, list });
  return list;
}
/** 机器列表缓存（在用 / 已退役都找）里的这台，和那份列表取回的时刻 */
function fromLists(qc: QueryClient, id: string | null) {
  for (const q of qc.getQueryCache().findAll({ queryKey: ['machines'] })) {
    const m = (q.state.data as MachineView[] | undefined)?.find((x) => x.id === id);
    if (m) return { m, at: q.state.dataUpdatedAt };
  }
}
/** 单台机器（抽屉、悬浮卡、各对话框共用一个 hook、一份缓存）：列表缓存里的那份直接当初始数据，并带上列表取回的时刻——
 *  列表还新鲜就不再单独取这台（列表与详情是同一份视图，实时推送两边都写），切换机器时也不闪“加载中” */
export function useMachine(id: string | null) {
  const qc = useQueryClient();
  return useQuery<MachineView>({
    queryKey: ['machine', id], queryFn: ({ signal }) => get<MachineView>(`/machines/${id}`, signal), enabled: !!id,
    initialData: () => fromLists(qc, id)?.m,
    initialDataUpdatedAt: () => fromLists(qc, id)?.at,
  });
}
const byName = (a: { name: string }, b: { name: string }) => collator.compare(a.name, b.name);
const sortRelays = (d: MachineView[]) => [...d].sort(byName);
const relayChoices = (d: MachineView[]) => d.filter((m) => m.lifecycle === 'active').map((m) => ({ id: m.id, name: m.name })).sort(byName);
const relaysQuery = { queryKey: ['relays'], queryFn: ({ signal }: { signal: AbortSignal }) => get<MachineView[]>('/relays', signal) };
export const useRelays = () => useQuery({ ...relaysQuery, select: sortRelays });
/** 只要“已上线线路机的 id 和名称”（上架页的下拉和名称匹配用）。select 的结果会做结构共享：机器的探测、流量等字段每次推送都在变，但 id/名称不变时引用不变，批量网格的 memo 才不会被击穿 */
export const useRelayChoices = () => useQuery({ ...relaysQuery, select: relayChoices });
/** 卡片心跳条：全部机器近 1 小时的探测，一次取回（页面回到前台时若已过期也重取）。
 *  pick 取一台的那份：结构共享下这台的数据没变时引用不变，心跳条不重渲染。
 *  每 2 分钟的轮询只由首页挂一个（useProbeBeatsPoll），不必每张卡各带一个 refetchInterval */
const probeBeatsQuery = { queryKey: ['probe-beats'], queryFn: ({ signal }: { signal: AbortSignal }) => get<ProbeBeats>('/probe-beats', signal), staleTime: 120_000 };
export const useProbeBeats = <T,>(pick: (d: ProbeBeats) => T) => useQuery({ ...probeBeatsQuery, select: pick });
const noBeats = () => null; // 轮询观察者不读数据：select 恒为 null，数据变了也不重渲染首页
export const useProbeBeatsPoll = (enabled: boolean) => { useQuery({ ...probeBeatsQuery, select: noBeats, refetchInterval: 120_000, enabled }); };
/** poll：实时流只推当前读数不推曲线，抽屉曲线按这个间隔刷新；staleTime 同间隔、切回窗口不重取。
 *  换时间范围时先留着上一段曲线（调用方按 isPlaceholderData 调淡、横轴按数据自带的 range 格式化），不闪骨架；换了机器不沿用别台的曲线 */
export const useSeries = (id: string | null, metric: 'host' | 'traffic' | 'probe', range: string, enabled: boolean, poll: number) =>
  useQuery({ queryKey: ['series', id, metric, range], queryFn: ({ signal }) => get<SeriesResp>(`/machines/${id}/series?metric=${metric}&range=${range}`, signal), enabled: !!id && enabled, staleTime: poll, refetchInterval: poll, refetchOnWindowFocus: false,
    placeholderData: (prev, prevQuery) => (prevQuery?.queryKey[1] === id ? keepPreviousData(prev) : undefined) });
/** 抽屉「连接质量」的近 N 天可用率：来自小时汇总（每 5 分钟汇总一次），抽屉开着时 10 分钟重取一次、切回窗口不重取。
 *  按账单时区分天：改订阅时服务端发 machine 事件，随这台的详情一起失效（eventKeys） */
export const useAvailability = (id: string, days: number) =>
  useQuery({ queryKey: ['availability', id, days], queryFn: ({ signal }) => get<Availability>(`/machines/${id}/availability?days=${days}`, signal), staleTime: 600_000, refetchInterval: 600_000, refetchOnWindowFocus: false });
export const useRenewals = (id: string | null) => useQuery({ queryKey: ['renewals', id], queryFn: ({ signal }) => get<Renewal[]>(`/machines/${id}/renewals`, signal), enabled: !!id });
export const useAudit = (id: string | null) => useQuery({ queryKey: ['audit', id], queryFn: ({ signal }) => get<AuditEntry[]>(`/audit?target_id=${id}&limit=10`, signal), enabled: !!id });
/** 「操作记录」页：全站审计，一页 100 条、按 before_id 往前翻（不满一页就到头了）；分类（cat）与搜索（q）由服务端筛。键在 ['audit'] 下，随机器类事件与写操作一起失效
 *  （实时流把 audit 当整表合并：失效会把已加载的每一页依次重取，批量上架时不能每 300ms 来一轮）；登录等不发事件的，过 30 秒后切回页面时重取。
 *  换筛选时先留着上一份结果，不闪骨架 */
const AUDIT_PAGE = 100;
export const useAuditLog = (cat: string, q: string) => useInfiniteQuery({
  queryKey: ['audit', 'all', cat, q], initialPageParam: '', staleTime: 30_000, placeholderData: keepPreviousData,
  queryFn: ({ pageParam, signal }) => {
    const p = new URLSearchParams({ limit: String(AUDIT_PAGE) });
    if (pageParam) p.set('before_id', pageParam);
    if (cat) p.set('cat', cat);
    if (q) p.set('q', q);
    return get<AuditEntry[]>(`/audit?${p}`, signal);
  },
  getNextPageParam: (last) => (last.length < AUDIT_PAGE ? undefined : String(last[last.length - 1].id)),
});
/** 作业列表一页（按创建先后倒序）；kind：作业类型（provision / restore 是大类，其余是作业类型原值，'' = 全部）。换页时先留着上一页，不闪骨架 */
export const useJobs = (state: 'active' | 'done' | 'all' = 'all', offset = 0, limit = 100, kind = '') =>
  useQuery({ queryKey: ['jobs', state, offset, limit, kind], queryFn: ({ signal }) => get<Paged<JobSummary>>(`/jobs?state=${state}&limit=${limit}&offset=${offset}${kind ? `&kind=${kind}` : ''}`, signal), placeholderData: keepPreviousData, staleTime: Infinity });
/** 作业详情：靠实时流的 job 事件失效重取 */
export const useJob = (id: string | null) => useQuery({ queryKey: ['job', id], queryFn: ({ signal }) => get<Job>(`/jobs/${id}`, signal), enabled: !!id, staleTime: Infinity });
/** 告警列表一页（严重度优先）。默认参数与告警页第一页相同：顶栏徽标取 total，和告警页共用同一份缓存。acked：其中已确认的条数（total 含它们） */
export const INCIDENT_PAGE = 50;
export const useIncidents = (state: 'firing' | 'resolved' | 'all' = 'all', offset = 0, limit = INCIDENT_PAGE) =>
  useQuery({ queryKey: ['incidents', state, offset, limit], queryFn: ({ signal }) => get<Paged<Incident> & { acked: number }>(`/incidents?state=${state}&limit=${limit}&offset=${offset}`, signal), placeholderData: keepPreviousData, staleTime: Infinity });
/** 抽屉「最近告警」：这台机器最近的 5 条（进行中、已恢复都要）。带 subject_id 时服务端按时间排：进行中在前、再按开始时间新的在前。
 *  键在 ['incidents'] 下：随 incidents 事件、确认与静默等写操作一起失效 */
const items = (d: Paged<Incident>) => d.items;
export const useMachineIncidents = (id: string) =>
  useQuery({ queryKey: ['incidents', 'machine', id], queryFn: ({ signal }) => get<Paged<Incident>>(`/incidents?subject_id=${id}&state=all&limit=5`, signal), select: items, staleTime: Infinity });
/** 静默到期（ends_at 过了就不再列出）没有事件，仍按时间过期 */
export const useSilences = () => useQuery({ queryKey: ['silences'], queryFn: () => get<Silence[]>('/silences') });

export interface StreamEvent { type: string; id?: string }
/** 事件对应要失效的查询键（实时流用）。resync 由 stream.ts 处理成全部失效。
 *  incidents：机器的健康与总览不看告警表，不重取整表；静默带来的「维护中」由服务端随后的 metrics 推送送达
 *  audit：抽屉里机器的操作记录，随改名、续费、退役等机器变更增加；['policies', 'list'] / ['policies', 'versions']：策略页的范围树列着各团队的节点、发布记录列着线路机名（名字、有没有入口），
 *  机器增减、改名换团队、团队改名都会改它（只有挂着的查询会重取，策略页没开时只是标成过期；不带发布确认框里的预览，免得框开着时内容在眼前变）；
 *  incidents / jobs 列表里带机器名，machine（改名、生命周期）很少发，顺带重取让名字立刻跟上；
 *  availability：近 N 天可用率按账单时区分天，改订阅（machine 事件）可能换了时区 */
export function eventKeys(ev: StreamEvent): unknown[][] {
  switch (ev.type) {
    case 'machine': return [['machine', ev.id], ['machines'], ['overview'], ['audit'], ['policies', 'list'], ['policies', 'versions'], ['incidents'], ['jobs'], ['availability', ev.id]];
    case 'machines': return [['machine'], ['machines'], ['relays'], ['overview'], ['audit'], ['policies', 'list'], ['policies', 'versions']];
    case 'relay': return [['relays'], ['machines']];
    case 'job': return [['jobs'], ['job', ev.id]];
    case 'incidents': return [['incidents'], ['silences']];
    case 'catalog': return [['teams'], ['providers'], ['plans'], ['machines'], ['machine'], ['notify-channels'], ['policies', 'list'], ['policies', 'versions']];
    case 'api_keys': return [['api-keys']];
    case 'settings': return [['access-retention'], ['access'], ['audit']];
    case 'policies': return [['policies'], ['machines'], ['relays']];
    // 软路由：增删改与每轮采集（最多一次）都发这一个，列表与各台详情一起重取（曲线另按时间轮询，见 lib/routers）
    case 'routers': return [['routers']];
    default: return [];
  }
}

/** 通用写操作：成功后失效 keys */
export function useWrite<A, R>(fn: (a: A) => Promise<R>, keys: unknown[][]) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: fn, onSuccess: () => { keys.forEach((k) => qc.invalidateQueries({ queryKey: k })); } });
}
const M = [['machines'], ['machine'], ['relays'], ['overview'], ['audit']];
/** PATCH /machines/{id}：没传的不改；provider_id / plan_id / service_ref 传 null = 清空 */
export interface MachinePatchBody {
  revision: number; team_id?: string; number?: number; label?: string; note?: string;
  provider_id?: string | null; plan_id?: string | null; service_ref?: string | null;
  /** 两位代码；null = 改回按 IP 自动 */
  country?: string | null;
}
/** PUT /machines/{id}/subscription：到期日 + 单独设置，整份替换——没传或 null = 跟随套餐 / 供应商（和模板相同的值服务端也按跟随存）。
 *  auto_renew 没传 = 不改；traffic_used_gb 按面板校准，没传 = 不改、null = 清掉 */
export type SubscriptionBody = { due_date: string; auto_renew?: boolean; traffic_used_gb?: number | null }
  & { [K in SubField]?: (K extends 'currency' | 'cycle' | 'billing_tz' ? string : number) | null };
/** POST /plans：套餐字段（不含 id 与统计）。PATCH 传其中任意几项（没传的不改，规格类传 null = 清空），archived 归档 / 取消归档 */
export type PlanBody = Omit<Plan, 'id' | 'archived_at' | 'machine_count'>;
export const useLogin = () => useWrite((b: { username: string; password: string; totp?: string }) => post<{ username: string }>('/auth/login', b), [['me']]);
export const usePatchMachine = () => useWrite(({ id, ...b }: { id: string } & MachinePatchBody) => patch<MachineView>(`/machines/${id}`, b), [...M, ['teams']]);
export const useRetire = () => useWrite(({ id, ...b }: { id: string; confirm: string; reason?: string; cleanup?: boolean }) => post<RetireResult>(`/machines/${id}/retire`, b), [...M, ['jobs']]);
export const usePurge = () => useWrite(({ id, ...b }: { id: string; confirm: string }) => post<{ job_id: string }>(`/machines/${id}/purge`, b), [...M, ['jobs']]);
export const useSaveSubscription = () => useWrite(({ id, ...b }: { id: string } & SubscriptionBody) => put(`/machines/${id}/subscription`, b), M);
/** 续费：new_due 与 cycles（续几个周期，服务端算新到期日）二选一 */
export const useRenew = () => useWrite(({ id, ...b }: { id: string; amount_cents?: number; note?: string } & ({ new_due: string } | { cycles: number })) =>
  post<{ id: string; old_due: string | null; new_due: string }>(`/machines/${id}/renewals`, b), [...M, ['renewals']]);
export const useEntryAction = () => useWrite(({ id, ...b }: { id: string; action: 'enable' | 'disable' | 'rotate'; confirm?: string }) => post(`/entries/${id}/actions`, b), M);
export const useAddEntry = () => useWrite(({ id, relay_id }: { id: string; relay_id: string }) => post<{ job_id: string }>(`/machines/${id}/entries`, { relay_id }), [...M, ['jobs']]);
export const useMoveEntry = () => useWrite(({ id, ...b }: { id: string; to_relay_id: string; from_relay_id?: string }) => post<MoveResult>(`/machines/${id}/move`, b), [...M, ['jobs']]);
export const useEvacuate = () => useWrite(({ id, to_relay_id }: { id: string; to_relay_id: string }) => post<EvacuateResult>(`/relays/${id}/evacuate`, { to_relay_id }), [...M, ['jobs']]);
export const useRestore = () => useWrite(({ id, ...b }: { id: string } & RestoreBody) => post<{ job_id: string; machine_id: string; name: string; old_name: string; number: number | null }>(`/machines/${id}/restore`, b), [...M, ['jobs'], ['teams']]);
/** 恢复预检；probe=false 不试 SSH（改团队/编号时只想看新名字，不必每次重新探测） */
export const useRestorePreview = (id: string | null, enabled: boolean, o: { team_id?: string; number?: number; label?: string; probe?: boolean } = {}) =>
  useQuery<RestorePreview>({
    queryKey: ['restore-preview', id, o.team_id ?? '', o.number ?? '', o.label ?? '', o.probe !== false], enabled: !!id && enabled, staleTime: 0, gcTime: 0, placeholderData: (p) => p,
    // 预检会真的 SSH 探测机器：切回窗口 / 网络恢复时不要自动重跑，改选项（queryKey 变化）才重查
    refetchOnWindowFocus: false, refetchOnReconnect: false,
    queryFn: ({ signal }) => {
      const q = new URLSearchParams();
      if (o.team_id) q.set('team_id', o.team_id);
      if (o.number != null) q.set('number', String(o.number));
      if (o.label) q.set('label', o.label);
      if (o.probe === false) q.set('probe', 'false');
      return get<RestorePreview>(`/machines/${id}/restore${q.size ? `?${q}` : ''}`, signal);
    },
  });
export const useProvisionCheck = () => useMutation({ mutationFn: (items: ProvisionItem[]) => post<{ items: ProvisionCheckItem[] }>('/provision/check', { items }) });
export const useProvision = () => useWrite((items: ProvisionItem[]) => post<{ jobs: { machine_id: string; job_id: string; name: string }[] }>('/provision', { items }), [...M, ['jobs'], ['teams']]);
export const useUpgradeAgent = () => useWrite((id: string) => post<{ machine_id: string; job_id: string }>(`/machines/${id}/upgrade-agent`), [...M, ['jobs']]);
export const useUpgradeAllAgents = () => useWrite(() => post<{ jobs: { machine_id: string; job_id: string }[]; order: string }>('/agents/upgrade'), [...M, ['jobs']]);
/** 重试：主机指纹不符 / SSH 认证失败时可带 reset_host_key、ssh_password */
export interface RetryBody { reset_host_key?: boolean; ssh_password?: string }
export const useRetryJob = () => useWrite(({ id, ...b }: { id: string } & RetryBody) => post(`/jobs/${id}/retry`, b), [...M, ['jobs'], ['job']]);
export const useCancelJob = () => useWrite((id: string) => post(`/jobs/${id}/cancel`), [...M, ['jobs'], ['job']]);
export const useAck = () => useWrite((id: string) => post(`/incidents/${id}/ack`), [['incidents']]);
export const useAddSilence = () => useWrite((b: Partial<Silence>) => post('/silences', b), [['silences'], ['incidents']]);
export const useDelSilence = () => useWrite((id: string) => del(`/silences/${id}`), [['silences'], ['incidents']]);
type MaintSubject = Pick<MachineView, 'id' | 'name' | 'host_machine'>;
/** 让这台机器「维护中」的、正在生效的维护窗口归谁：自己的优先；同机 IP 节点自己没有、宿主机有时是宿主机的（宿主机维护时它也算维护中）。
 *  windows 为空 = 只在「全部机器」的维护窗口里 */
export function maintOwner(list: Silence[], m: MaintSubject, now = Date.now()) {
  const live = (id: string) => list.filter((s) => s.subject_id === id && s.is_maintenance && Date.parse(s.starts_at) <= now && Date.parse(s.ends_at) > now);
  const own = live(m.id);
  if (own.length || !m.host_machine) return { owner: m, onHost: false, windows: own };
  const host = live(m.host_machine.id);
  return host.length ? { owner: { ...m.host_machine, host_machine: null }, onHost: true, windows: host } : { owner: m, onHost: false, windows: own };
}
/** 维护结束、新的健康还没到时的健康：服务端维护中不给原因，估不出来，先标未知（不冒充正常） */
const MAINT_ENDED: MachineView['health'] = { status: 'unknown', reasons: [{ key: 'maintenance_ended', severity: 'info', text: '维护刚结束，正在重新判定' }] };
/** 结束一台机器的维护（卡片菜单、抽屉「管理」）：删掉 maintOwner 找到的维护窗口（同机 IP 节点因宿主机维护时结束的是宿主机的）；
 *  「全部机器」的那种不动，只有那种时报错。返回结束了谁的维护。
 *  删完先在缓存里把这台（宿主机连同它的同机 IP 节点）改成不在维护、健康未知（同开始维护时提示一出、卡片就跟上），
 *  再重取机器列表与详情（同机 IP 节点的详情也变了，按前缀全失效）拿到服务端的准确健康 */
export const useEndMaintenance = () => {
  const qc = useQueryClient();
  return useWrite(async (m: MaintSubject) => {
    const list = await qc.fetchQuery({ queryKey: ['silences'], queryFn: () => get<Silence[]>('/silences'), staleTime: 0 });
    const { owner, windows } = maintOwner(list, m);
    if (!windows.length) throw new Error('这台机器在「全部机器」的维护窗口里，请到告警页结束');
    for (const s of windows) await del(`/silences/${s.id}`);
    const hit = (x: MachineView) => x.id === owner.id || x.host_machine?.id === owner.id;
    const off = (x: MachineView) => (hit(x) && x.maintenance ? { ...x, maintenance: false, health: x.health.status === 'maintenance' ? MAINT_ENDED : x.health } : x);
    qc.setQueriesData<MachineView[]>({ queryKey: ['machines'] }, (d) => d?.map(off));
    qc.setQueriesData<MachineView>({ queryKey: ['machine'] }, (d) => d && off(d));
    return owner;
  }, [['silences'], ['incidents'], ['machines'], ['machine'], ['overview']]);
};
export const useAddTeam = () => useWrite((b: { name: string; sort_order?: number }) => post('/teams', b), [['teams']]);
export const usePatchTeam = () => useWrite(({ id, ...b }: { id: string; name?: string; sort_order?: number }) => patch(`/teams/${id}`, b), [['teams'], ...M]);
export const useRenumber = () => useWrite(({ id, token }: { id: string; token: string }) => post(`/teams/${id}/renumber`, { token }), [['teams'], ...M]);
export const fetchRenumber = (id: string) => get<RenumberPreview>(`/teams/${id}/renumber`);
export const useAddProvider = () => useWrite((b: Partial<Provider>) => post('/providers', b), [['providers']]);
export const usePatchProvider = () => useWrite(({ id, ...b }: { id: string } & Partial<Provider>) => patch(`/providers/${id}`, b), [['providers'], ...M]);
export const useAddPlan = () => useWrite((b: PlanBody) => post('/plans', b), [['plans']]);
export const usePatchPlan = () => useWrite(({ id, ...b }: { id: string; archived?: boolean } & Partial<PlanBody>) => patch(`/plans/${id}`, b), [['plans'], ...M]);
/** 删团队 / 供应商 / 套餐：还有机器、套餐用着时中控回 409，写明还有什么 */
export const useDelTeam = () => useWrite((id: string) => del(`/teams/${id}`), [['teams'], ['policies']]);
export const useDelProvider = () => useWrite((id: string) => del(`/providers/${id}`), [['providers']]);
export const useDelPlan = () => useWrite((id: string) => del(`/plans/${id}`), [['plans']]);
export const fetchShare = (entryId: string) => get<{ url: string }>(`/entries/${entryId}/share`);
const fetchLogs = (jobId: string, after: number, signal?: AbortSignal) => get<JobLog[]>(`/jobs/${jobId}/logs?after=${after}`, signal);
const LOG_PAGE = 1000; // 服务端单次最多返回 1000 条
/** 取 after 之后的全部日志：满页就接着往后拉，直到拉完。signal：离开页面 / 换作业时中止 */
export async function fetchAllLogs(jobId: string, after: number, signal?: AbortSignal): Promise<JobLog[]> {
  const all: JobLog[] = [];
  for (let guard = 0; guard < 200; guard++) {
    const page = await fetchLogs(jobId, after, signal);
    all.push(...page);
    if (page.length < LOG_PAGE) break;
    after = page[page.length - 1].id;
  }
  return all;
}
/** 只取最后 n 行（打开日志很长的作业时用）：skipped 是前面没给的行数，after 是之后续拉的游标 */
export const fetchTailLogs = (jobId: string, n: number, signal?: AbortSignal) =>
  get<{ logs: JobLog[]; skipped: number; after: number }>(`/jobs/${jobId}/logs?tail=${n}`, signal);

// ---- 设置页：API 密钥与告警通知渠道 ----
/** 最近使用时间（last_used_at）随调用更新、不发事件，仍按时间过期 */
export const useApiKeys = () => useQuery({ queryKey: ['api-keys'], queryFn: () => get<ApiKey[]>('/api-keys') });
/** 新建要当前密码（开了两步验证还要 totp：验证码或恢复码） */
export const useCreateApiKey = () => useWrite((b: { name: string; scopes: string[]; expires_days?: number; password: string; totp?: string }) => post<{ id: string; token: string }>('/api-keys', b), [['api-keys']]);
export const useRevokeApiKey = () => useWrite((id: string) => del(`/api-keys/${id}`), [['api-keys']]);
export const useNotifyChannels = () => useQuery({ queryKey: ['notify-channels'], queryFn: () => get<NotifyChannel[]>('/notify-channels'), staleTime: Infinity });
export const useAddChannel = () => useWrite((b: { name: string; kind: string; config: Record<string, string>; min_severity: string; enabled: boolean }) => post<{ id: string }>('/notify-channels', b), [['notify-channels']]);
export const usePatchChannel = () => useWrite(({ id, ...b }: { id: string; name?: string; config?: Record<string, string>; min_severity?: string; enabled?: boolean }) => patch(`/notify-channels/${id}`, b), [['notify-channels']]);
export const useDelChannel = () => useWrite((id: string) => del(`/notify-channels/${id}`), [['notify-channels'], ['notify-log']]);
export const useTestChannel = () => useWrite((id: string) => post<{ ok: boolean; error: string | null }>(`/notify-channels/${id}/test`), [['notify-log']]);
export const useNotifyLog = () => useQuery({ queryKey: ['notify-log'], queryFn: () => get<NotifyLogEntry[]>('/notify-log?limit=100') });
export const useRetirePreview = (id: string | null, enabled: boolean) => useQuery({ queryKey: ['retire-preview', id], queryFn: ({ signal }) => get<RetirePreview>(`/machines/${id}/retire`, signal), enabled: !!id && enabled, staleTime: 0, gcTime: 0, refetchOnWindowFocus: false, refetchOnReconnect: false });

// ---- 设置页：账户安全（两步验证、登录会话、改密码） ----
export const useTotpStatus = () => useQuery({ queryKey: ['account', 'totp'], queryFn: () => get<TotpStatus>('/account/totp') });
/** 生成待确认的密钥（要当前密码）：每次开启都重新生成，旧的作废 */
export const useTotpSetup = () => useMutation({ mutationFn: (password: string) => post<TotpSetup>('/account/totp/setup', { password }) });
export const useTotpEnable = () => useWrite((code: string) => post<{ recovery_codes: string[] }>('/account/totp/enable', { code }), [['account', 'totp']]);
export const useTotpDisable = () => useWrite((b: { password: string; code: string }) => post('/account/totp/disable', b), [['account', 'totp']]);
export const useRegenRecovery = () => useWrite((code: string) => post<{ recovery_codes: string[] }>('/account/totp/recovery-codes', { code }), [['account', 'totp']]);
export const useSessions = () => useQuery({ queryKey: ['account', 'sessions'], queryFn: () => get<AccountSession[]>('/account/sessions') });
export const useRevokeSession = () => useWrite((id: string) => del(`/account/sessions/${id}`), [['account', 'sessions']]);
/** 退出其它设备：要当前密码（开了两步验证还要验证码） */
export const useRevokeOtherSessions = () => useWrite((b: { password: string; totp?: string }) => post<{ revoked: number }>('/account/sessions/revoke-others', b), [['account', 'sessions']]);
/** 改密码：成功后服务端撤销本设备以外的所有会话；revoke_api_keys 同时撤销全部 API 密钥 */
export const useChangePassword = () => useWrite((b: { current: string; new: string; totp?: string; revoke_api_keys?: boolean }) => post<{ ok: boolean; revoked_sessions: number; revoked_api_keys: number }>('/account/password', b), [['account', 'sessions'], ['api-keys']]);
