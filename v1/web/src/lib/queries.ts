// TanStack Query 封装：查询 hook + 写操作 hook。写操作成功后按范围失效（实时流也会再推一次，重复无害）。
// 内容只会因实时流事件而变的查询（eventKeys 覆盖到的）staleTime 设成 Infinity：切回窗口、重新挂载都不再重取，靠事件失效；
// 断线重连时 stream.ts 会让全部查询失效一次，没连着时错过的变化照样补上。
import { useCallback } from 'react';
import { keepPreviousData, useMutation, useQuery, useQueryClient, type QueryClient } from '@tanstack/react-query';
import { del, get, patch, post, put } from './api';
import { collator, makeDefaultCompare } from './sort';
import { quotaTight } from './derive';
import type { ApiKey, Incident, Job, JobSummary, Paged, JobLog, MachineView, Overview, Plan, Provider, ProvisionCheckItem, ProvisionItem, Renewal, RenumberPreview, RetirePreview, RetireResult, RestoreBody, RestorePreview, MoveResult, EvacuateResult, ProbeBeats, SeriesResp, Silence, Subscription, Team, AuditEntry, NotifyChannel, NotifyLogEntry } from './types';

export const useMe = () => useQuery({ queryKey: ['me'], queryFn: () => get<{ username: string }>('/auth/me'), retry: false, staleTime: 5 * 60_000 });
// 团队、套餐带着由机器算出来的台数（node_count / machine_count），机器增减不发 catalog 事件，仍按时间过期；供应商只随 catalog 事件变
export const useTeams = () => useQuery({ queryKey: ['teams'], queryFn: () => get<Team[]>('/teams'), staleTime: 60_000 });
export const useProviders = () => useQuery({ queryKey: ['providers'], queryFn: () => get<Provider[]>('/providers'), staleTime: Infinity });
export const usePlans = () => useQuery({ queryKey: ['plans'], queryFn: () => get<Plan[]>('/plans'), staleTime: 60_000 });
export const useOverview = () => useQuery({ queryKey: ['overview'], queryFn: ({ signal }) => get<Overview>('/overview', signal) });

const machinesQuery = (scope: 'live' | 'retired' | 'all') => ({ queryKey: ['machines', scope], queryFn: ({ signal }: { signal: AbortSignal }) => get<MachineView[]>(`/machines?scope=${scope}`, signal) });
/** 机器列表，已按默认顺序排好：线路机 → 团队 → 编号（故障/离线不下沉） */
export function useMachines(scope: 'live' | 'retired' | 'all' = 'live') {
  const teams = useTeams().data;
  // select 排序：只在列表或团队变化时重算；返回的是查询结果本身，调用方按属性追踪
  const select = useCallback((d: MachineView[]) => [...d].sort(makeDefaultCompare(teams ?? [])), [teams]);
  return useQuery({ ...machinesQuery(scope), select });
}
/** 只用名称等少量字段的页面（访问监控的节点下拉、告警页的对象与续费）：读数不进来 */
export type MachineBrief = Pick<MachineView, 'id' | 'name' | 'kind' | 'team' | 'subscription'>;
const brief = (m: MachineView): MachineBrief => ({ id: m.id, name: m.name, kind: m.kind, team: m.team, subscription: m.subscription });
/** 命令面板：搜索（名称 / IP / 团队 / 类型）与健康标用到的字段 */
export type MachineLabel = Pick<MachineView, 'id' | 'name' | 'ip' | 'kind' | 'team' | 'health' | 'lifecycle' | 'maintenance'>;
const label = (m: MachineView): MachineLabel => ({ id: m.id, name: m.name, ip: m.ip, kind: m.kind, team: m.team, health: m.health, lifecycle: m.lifecycle, maintenance: m.maintenance });
/** 与 useMachines 同一份缓存、同样的默认顺序，每台只取 pick 出的字段（pick 须是模块常量）。
 *  select 的结果做结构共享：实时推送只改读数时这些字段不变，引用不变，页面不重渲染 */
function useMachinesPicked<T>(pick: (m: MachineView) => T) {
  const teams = useTeams().data;
  const select = useCallback((d: MachineView[]) => [...d].sort(makeDefaultCompare(teams ?? [])).map(pick), [teams, pick]);
  return useQuery({ ...machinesQuery('live'), select });
}
export const useMachineBriefs = () => useMachinesPicked(brief);
export const useMachineLabels = () => useMachinesPicked(label);
/** 上架页「在已有机器上添加 IP」的宿主机候选：在用、自身不是同机 IP 节点的落地机，带出它的团队、线路机（第一个入口的）、SSH 端口与供应商。
 *  同一份机器列表缓存，结果做结构共享：推送只改读数时引用不变（批量网格的 memo 不被击穿） */
export interface HostChoice { id: string; name: string; team_id: string; relay_id: string; ssh_port: number; provider: string | null }
const hostChoices = (d: MachineView[]) => d.filter((m) => m.kind === 'vps' && m.lifecycle === 'active' && !m.host_machine)
  .map((m): HostChoice => ({ id: m.id, name: m.name, team_id: m.team?.id ?? '', relay_id: m.entries[0]?.relay.id ?? '', ssh_port: m.ssh_port, provider: m.provider?.name ?? null }))
  .sort((a, b) => collator.compare(a.name, b.name));
export const useHostChoices = (enabled: boolean) => useQuery({ ...machinesQuery('live'), select: hostChoices, enabled });
/** 首页 KPI「流量额度」：同一份机器列表缓存，只取吃紧的台数与最紧的一台；结果做结构共享，没变时首页总览不跟着推送重渲染 */
export const useQuotaTight = () => useQuery({ ...machinesQuery('live'), select: quotaTight });
/** 按键等“用时才读”的场景：直接读缓存里的机器列表（同样的默认顺序），不订阅、不触发请求 */
export function readMachines(qc: QueryClient): MachineView[] | undefined {
  const raw = qc.getQueryData<MachineView[]>(['machines', 'live']);
  return raw && [...raw].sort(makeDefaultCompare(qc.getQueryData<Team[]>(['teams']) ?? []));
}
/** 机器列表缓存（在用 / 已退役 / 全部都找）里的这台，和那份列表取回的时刻 */
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
export const useRenewals = (id: string | null) => useQuery({ queryKey: ['renewals', id], queryFn: ({ signal }) => get<Renewal[]>(`/machines/${id}/renewals`, signal), enabled: !!id });
export const useAudit = (id: string | null) => useQuery({ queryKey: ['audit', id], queryFn: ({ signal }) => get<AuditEntry[]>(`/audit?target_id=${id}&limit=10`, signal), enabled: !!id });
/** 作业列表一页（按创建先后倒序）；换页时先留着上一页，不闪骨架 */
export const useJobs = (state: 'active' | 'done' | 'all' = 'all', offset = 0, limit = 100) =>
  useQuery({ queryKey: ['jobs', state, offset, limit], queryFn: ({ signal }) => get<Paged<JobSummary>>(`/jobs?state=${state}&limit=${limit}&offset=${offset}`, signal), placeholderData: keepPreviousData, staleTime: Infinity });
/** 作业详情：靠实时流的 job 事件失效重取 */
export const useJob = (id: string | null) => useQuery({ queryKey: ['job', id], queryFn: ({ signal }) => get<Job>(`/jobs/${id}`, signal), enabled: !!id, staleTime: Infinity });
/** 告警列表一页（严重度优先）。默认参数与告警页第一页相同：顶栏徽标取 total，和告警页共用同一份缓存 */
export const INCIDENT_PAGE = 50;
export const useIncidents = (state: 'firing' | 'resolved' | 'all' = 'all', offset = 0, limit = INCIDENT_PAGE) =>
  useQuery({ queryKey: ['incidents', state, offset, limit], queryFn: ({ signal }) => get<Paged<Incident>>(`/incidents?state=${state}&limit=${limit}&offset=${offset}`, signal), placeholderData: keepPreviousData, staleTime: Infinity });
/** 抽屉「最近告警」：这台机器最近的 5 条（进行中、已恢复都要）。服务端是严重度优先，这里只有几条，按进行中在前、再按开始时间新的在前排，读起来是时间顺序。
 *  键在 ['incidents'] 下：随 incidents 事件、确认与静默等写操作一起失效 */
const byRecent = (a: Incident, b: Incident) => (a.state === b.state ? Date.parse(b.opened_at) - Date.parse(a.opened_at) : a.state === 'firing' ? -1 : 1);
const recentIncidents = (d: Paged<Incident>) => [...d.items].sort(byRecent);
export const useMachineIncidents = (id: string) =>
  useQuery({ queryKey: ['incidents', 'machine', id], queryFn: ({ signal }) => get<Paged<Incident>>(`/incidents?subject_id=${id}&state=all&limit=5`, signal), select: recentIncidents, staleTime: Infinity });
/** 静默到期（ends_at 过了就不再列出）没有事件，仍按时间过期 */
export const useSilences = () => useQuery({ queryKey: ['silences'], queryFn: () => get<Silence[]>('/silences') });

export interface StreamEvent { type: string; id?: string }
/** 事件对应要失效的查询键（实时流用）。resync 由 stream.ts 处理成全部失效。
 *  machine_job：作业推进时服务端给目标机器发的，只重取详情（列表的 active_job 由 metrics 推送；改生命周期时服务端另发 machines）
 *  incidents：机器的健康与总览不看告警表，不重取整表；静默带来的「维护中」由服务端随后的 metrics 推送送达
 *  audit：抽屉里机器的操作记录，随改名、续费、退役等机器变更增加；policies：策略页的范围树列着各团队的节点（名字、有没有入口），
 *  机器增减、改名换团队、团队改名都会改它（只有挂着的查询会重取，策略页没开时只是标成过期）；
 *  incidents / jobs 列表里带机器名，machine（改名、生命周期）很少发，顺带重取让名字立刻跟上 */
export function eventKeys(ev: StreamEvent): unknown[][] {
  switch (ev.type) {
    case 'machine': return [['machine', ev.id], ['machines'], ['overview'], ['audit'], ['policies'], ['incidents'], ['jobs']];
    case 'machine_job': return [['machine', ev.id]];
    case 'machines': return [['machine'], ['machines'], ['relays'], ['overview'], ['audit'], ['policies']];
    case 'relay': return [['relays'], ['machines']];
    case 'job': return [['jobs'], ['job', ev.id]];
    case 'incidents': return [['incidents'], ['silences']];
    case 'catalog': return [['teams'], ['providers'], ['plans'], ['machines'], ['machine'], ['notify-channels'], ['policies']];
    case 'api_keys': return [['api-keys']];
    case 'policies': return [['policies'], ['machines'], ['relays']];
    default: return [];
  }
}

/** 通用写操作：成功后失效 keys */
function useWrite<A, R>(fn: (a: A) => Promise<R>, keys: unknown[][]) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: fn, onSuccess: () => { keys.forEach((k) => qc.invalidateQueries({ queryKey: k })); } });
}
const M = [['machines'], ['machine'], ['relays'], ['overview'], ['audit']];
/** PATCH /machines/{id}：没传的不改；provider_id / plan_id / service_ref 传 null = 清空 */
export interface MachinePatchBody {
  revision: number; team_id?: string; number?: number; label?: string; note?: string;
  provider_id?: string | null; plan_id?: string | null; service_ref?: string | null;
}
/** PUT /machines/{id}/subscription：整份替换（due_days 由服务端算） */
export type SubscriptionBody = Omit<Subscription, 'due_days'>;
/** POST /plans：套餐字段（不含 id 与统计）。PATCH 传其中任意几项（没传的不改，规格类传 null = 清空），archived 归档 / 取消归档 */
export type PlanBody = Omit<Plan, 'id' | 'archived_at' | 'machine_count'>;
export const useLogin = () => useWrite((b: { username: string; password: string }) => post<{ username: string }>('/auth/login', b), [['me']]);
export const usePatchMachine = () => useWrite(({ id, ...b }: { id: string } & MachinePatchBody) => patch<MachineView>(`/machines/${id}`, b), [...M, ['teams']]);
export const useRetire = () => useWrite(({ id, ...b }: { id: string; confirm: string; reason?: string; cleanup?: boolean }) => post<RetireResult>(`/machines/${id}/retire`, b), [...M, ['jobs']]);
export const usePurge = () => useWrite(({ id, ...b }: { id: string; confirm: string }) => post<{ job_id: string }>(`/machines/${id}/purge`, b), [...M, ['jobs']]);
export const useSaveSubscription = () => useWrite(({ id, ...b }: { id: string } & SubscriptionBody) => put(`/machines/${id}/subscription`, b), M);
export const useRenew = () => useWrite(({ id, ...b }: { id: string; new_due: string; amount_cents?: number; note?: string }) => post(`/machines/${id}/renewals`, b), [...M, ['renewals']]);
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
export const useUpgradeAllAgents = () => useWrite(() => post<{ jobs: { machine_id: string; job_id: string }[] }>('/agents/upgrade'), [...M, ['jobs']]);
/** 重试：主机指纹不符 / SSH 认证失败时可带 reset_host_key、ssh_password */
export interface RetryBody { reset_host_key?: boolean; ssh_password?: string }
export const useRetryJob = () => useWrite(({ id, ...b }: { id: string } & RetryBody) => post(`/jobs/${id}/retry`, b), [...M, ['jobs'], ['job']]);
export const useCancelJob = () => useWrite((id: string) => post(`/jobs/${id}/cancel`), [...M, ['jobs'], ['job']]);
export const useAck = () => useWrite((id: string) => post(`/incidents/${id}/ack`), [['incidents']]);
export const useAddSilence = () => useWrite((b: Partial<Silence>) => post('/silences', b), [['silences'], ['incidents']]);
export const useDelSilence = () => useWrite((id: string) => del(`/silences/${id}`), [['silences'], ['incidents']]);
export const useAddTeam = () => useWrite((b: { name: string; sort_order?: number }) => post('/teams', b), [['teams']]);
export const usePatchTeam = () => useWrite(({ id, ...b }: { id: string; name?: string; sort_order?: number }) => patch(`/teams/${id}`, b), [['teams'], ...M]);
export const useRenumber = () => useWrite(({ id, token }: { id: string; token: string }) => post(`/teams/${id}/renumber`, { token }), [['teams'], ...M]);
export const fetchRenumber = (id: string) => get<RenumberPreview>(`/teams/${id}/renumber`);
export const useAddProvider = () => useWrite((b: Partial<Provider>) => post('/providers', b), [['providers']]);
export const usePatchProvider = () => useWrite(({ id, ...b }: { id: string } & Partial<Provider>) => patch(`/providers/${id}`, b), [['providers'], ...M]);
export const useAddPlan = () => useWrite((b: PlanBody) => post('/plans', b), [['plans']]);
export const usePatchPlan = () => useWrite(({ id, ...b }: { id: string; archived?: boolean } & Partial<PlanBody>) => patch(`/plans/${id}`, b), [['plans'], ...M]);
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

// ---- 设置页：API 密钥与告警通知渠道 ----
/** 最近使用时间（last_used_at）随调用更新、不发事件，仍按时间过期 */
export const useApiKeys = () => useQuery({ queryKey: ['api-keys'], queryFn: () => get<ApiKey[]>('/api-keys') });
export const useCreateApiKey = () => useWrite((b: { name: string; scopes: string[]; expires_days?: number }) => post<{ id: string; token: string }>('/api-keys', b), [['api-keys']]);
export const useRevokeApiKey = () => useWrite((id: string) => del(`/api-keys/${id}`), [['api-keys']]);
export const useNotifyChannels = () => useQuery({ queryKey: ['notify-channels'], queryFn: () => get<NotifyChannel[]>('/notify-channels'), staleTime: Infinity });
export const useAddChannel = () => useWrite((b: { name: string; kind: string; config: Record<string, string>; min_severity: string; enabled: boolean }) => post<{ id: string }>('/notify-channels', b), [['notify-channels']]);
export const usePatchChannel = () => useWrite(({ id, ...b }: { id: string; name?: string; config?: Record<string, string>; min_severity?: string; enabled?: boolean }) => patch(`/notify-channels/${id}`, b), [['notify-channels']]);
export const useDelChannel = () => useWrite((id: string) => del(`/notify-channels/${id}`), [['notify-channels'], ['notify-log']]);
export const useTestChannel = () => useWrite((id: string) => post<{ ok: boolean; error: string | null }>(`/notify-channels/${id}/test`), [['notify-log']]);
export const useNotifyLog = () => useQuery({ queryKey: ['notify-log'], queryFn: () => get<NotifyLogEntry[]>('/notify-log?limit=100') });
export const useRetirePreview = (id: string | null, enabled: boolean) => useQuery({ queryKey: ['retire-preview', id], queryFn: ({ signal }) => get<RetirePreview>(`/machines/${id}/retire`, signal), enabled: !!id && enabled, staleTime: 0, gcTime: 0, refetchOnWindowFocus: false, refetchOnReconnect: false });
