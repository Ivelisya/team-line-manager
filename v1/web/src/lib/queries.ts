// TanStack Query 封装：查询 hook + 写操作 hook。写操作成功后按范围失效（实时流也会再推一次，重复无害）。
import { useCallback } from 'react';
import { keepPreviousData, useMutation, useQuery, useQueryClient, type QueryClient } from '@tanstack/react-query';
import { del, get, patch, post, put } from './api';
import { collator, makeDefaultCompare } from './sort';
import type { ApiKey, Incident, Job, Paged, JobLog, MachineView, Overview, Plan, Provider, ProvisionCheckItem, ProvisionItem, Renewal, RenumberPreview, RetirePreview, RestoreBody, RestorePreview, MoveResult, EvacuateResult, ProbeBeats, SeriesResp, Silence, Team, AuditEntry, NotifyChannel, NotifyLogEntry } from './types';

export const useMe = () => useQuery({ queryKey: ['me'], queryFn: () => get<{ username: string }>('/auth/me'), retry: false, staleTime: 5 * 60_000 });
export const useTeams = () => useQuery({ queryKey: ['teams'], queryFn: () => get<Team[]>('/teams'), staleTime: 60_000 });
export const useProviders = () => useQuery({ queryKey: ['providers'], queryFn: () => get<Provider[]>('/providers'), staleTime: 60_000 });
export const usePlans = () => useQuery({ queryKey: ['plans'], queryFn: () => get<Plan[]>('/plans'), staleTime: 60_000 });
export const useOverview = () => useQuery({ queryKey: ['overview'], queryFn: ({ signal }) => get<Overview>('/overview', signal) });

/** 机器列表，已按默认顺序排好：线路机 → 团队 → 编号（故障/离线不下沉） */
export function useMachines(scope: 'live' | 'retired' | 'all' = 'live', enabled = true) {
  const teams = useTeams().data;
  // select 排序：只在列表或团队变化时重算；返回的是查询结果本身，调用方按属性追踪
  const select = useCallback((d: MachineView[]) => [...d].sort(makeDefaultCompare(teams ?? [])), [teams]);
  return useQuery({ queryKey: ['machines', scope], queryFn: ({ signal }) => get<MachineView[]>(`/machines?scope=${scope}`, signal), select, enabled });
}
/** 按键等“用时才读”的场景：直接读缓存里的机器列表（同样的默认顺序），不订阅、不触发请求 */
export function readMachines(qc: QueryClient, scope: 'live' | 'retired' | 'all' = 'live'): MachineView[] | undefined {
  const raw = qc.getQueryData<MachineView[]>(['machines', scope]);
  return raw ? [...raw].sort(makeDefaultCompare(qc.getQueryData<Team[]>(['teams']) ?? [])) : undefined;
}
export const useMachine = (id: string | null) => useQuery({ queryKey: ['machine', id], queryFn: ({ signal }) => get<MachineView>(`/machines/${id}`, signal), enabled: !!id });
/** 悬浮卡用：先用机器列表缓存里的那份（立即有内容），再按需取单台 */
export function useMachinePreview(id: string | null) {
  const qc = useQueryClient();
  return useQuery({
    queryKey: ['machine', id], queryFn: ({ signal }) => get<MachineView>(`/machines/${id}`, signal), enabled: !!id, staleTime: 15_000,
    initialData: () => qc.getQueryData<MachineView[]>(['machines', 'live'])?.find((m) => m.id === id),
    initialDataUpdatedAt: () => qc.getQueryState(['machines', 'live'])?.dataUpdatedAt,
  });
}
const byName = (a: { name: string }, b: { name: string }) => collator.compare(a.name, b.name);
const sortRelays = (d: MachineView[]) => [...d].sort(byName);
const relayChoices = (d: MachineView[]) => d.filter((m) => m.lifecycle === 'active').map((m) => ({ id: m.id, name: m.name })).sort(byName);
const relaysQuery = { queryKey: ['relays'], queryFn: ({ signal }: { signal: AbortSignal }) => get<MachineView[]>('/relays', signal) };
export const useRelays = () => useQuery({ ...relaysQuery, select: sortRelays });
/** 只要“已上线线路机的 id 和名称”（上架页的下拉和名称匹配用）。select 的结果会做结构共享：机器的探测、流量等字段每次推送都在变，但 id/名称不变时引用不变，批量网格的 memo 才不会被击穿 */
export const useRelayChoices = () => useQuery({ ...relaysQuery, select: relayChoices });
/** 卡片心跳条：全部机器近 1 小时的探测，一次取回（每 2 分钟，页面回到前台时若已过期也重取）。
 *  pick 取一台的那份：结构共享下这台的数据没变时引用不变，心跳条不重渲染 */
export const useProbeBeats = <T,>(pick: (d: ProbeBeats) => T) =>
  useQuery({ queryKey: ['probe-beats'], queryFn: ({ signal }) => get<ProbeBeats>('/probe-beats', signal), select: pick, staleTime: 120_000, refetchInterval: 120_000 });
/** poll：实时流只推当前读数不推曲线，抽屉曲线按这个间隔刷新；此时 staleTime 同间隔、切回窗口不重取 */
export const useSeries = (id: string | null, metric: 'host' | 'traffic' | 'probe', range: string, enabled = true, poll?: number) =>
  useQuery({ queryKey: ['series', id, metric, range], queryFn: ({ signal }) => get<SeriesResp>(`/machines/${id}/series?metric=${metric}&range=${range}`, signal), enabled: !!id && enabled, staleTime: poll ?? 30_000, refetchInterval: poll, refetchOnWindowFocus: poll ? false : undefined });
export const useRenewals = (id: string | null) => useQuery({ queryKey: ['renewals', id], queryFn: ({ signal }) => get<Renewal[]>(`/machines/${id}/renewals`, signal), enabled: !!id });
export const useAudit = (id: string | null) => useQuery({ queryKey: ['audit', id], queryFn: ({ signal }) => get<AuditEntry[]>(`/audit?target_id=${id}&limit=10`, signal), enabled: !!id });
/** 作业列表一页（按创建先后倒序）；换页时先留着上一页，不闪骨架 */
export const useJobs = (state: 'active' | 'done' | 'all' = 'all', offset = 0, limit = 100) =>
  useQuery({ queryKey: ['jobs', state, offset, limit], queryFn: ({ signal }) => get<Paged<Job>>(`/jobs?state=${state}&limit=${limit}&offset=${offset}`, signal), placeholderData: keepPreviousData });
/** 作业详情：靠实时流的 job 事件失效重取 */
export const useJob = (id: string | null) => useQuery({ queryKey: ['job', id], queryFn: ({ signal }) => get<Job>(`/jobs/${id}`, signal), enabled: !!id });
/** 告警列表一页（严重度优先）。默认参数与告警页第一页相同：顶栏徽标取 total，和告警页共用同一份缓存 */
export const INCIDENT_PAGE = 50;
export const useIncidents = (state: 'firing' | 'resolved' | 'all' = 'all', offset = 0, limit = INCIDENT_PAGE) =>
  useQuery({ queryKey: ['incidents', state, offset, limit], queryFn: ({ signal }) => get<Paged<Incident>>(`/incidents?state=${state}&limit=${limit}&offset=${offset}`, signal), placeholderData: keepPreviousData });
export const useSilences = () => useQuery({ queryKey: ['silences'], queryFn: () => get<Silence[]>('/silences') });

export interface StreamEvent { type: string; id?: string }
/** 事件对应要失效的查询键（实时流用）。resync 由 stream.ts 处理成全部失效。
 *  machine_job：作业推进时服务端给目标机器发的，只重取详情（列表的 active_job 由 metrics 推送；改生命周期时服务端另发 machines） */
export function eventKeys(ev: StreamEvent): unknown[][] {
  switch (ev.type) {
    case 'machine': return [['machine', ev.id], ['machines'], ['overview']];
    case 'machine_job': return [['machine', ev.id]];
    case 'machines': return [['machine'], ['machines'], ['relays'], ['overview']];
    case 'relay': return [['relays'], ['machines']];
    case 'job': return [['jobs'], ['job', ev.id]];
    case 'incidents': return [['incidents'], ['silences'], ['overview'], ['machines']];
    case 'catalog': return [['teams'], ['providers'], ['plans'], ['machines'], ['machine'], ['notify-channels']];
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
const M = [['machines'], ['machine'], ['relays'], ['overview']];
export const useLogin = () => useWrite((b: { username: string; password: string }) => post<{ username: string }>('/auth/login', b), [['me']]);
export const usePatchMachine = () => useWrite(({ id, ...b }: { id: string; revision: number } & Record<string, unknown>) => patch<MachineView>(`/machines/${id}`, b), [...M, ['teams']]);
export const useRetire = () => useWrite(({ id, ...b }: { id: string; confirm: string; reason?: string; cleanup?: boolean }) => post<{ job_id: string }>(`/machines/${id}/retire`, b), [...M, ['jobs']]);
export const usePurge = () => useWrite(({ id, ...b }: { id: string; confirm: string }) => post<{ job_id: string }>(`/machines/${id}/purge`, b), [...M, ['jobs']]);
export const useSaveSubscription = () => useWrite(({ id, ...b }: { id: string } & Record<string, unknown>) => put(`/machines/${id}/subscription`, b), M);
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
export const useRetryJob = () => useWrite((id: string) => post(`/jobs/${id}/retry`), [['jobs'], ['job']]);
export const useCancelJob = () => useWrite((id: string) => post(`/jobs/${id}/cancel`), [['jobs'], ['job']]);
export const useAck = () => useWrite((id: string) => post(`/incidents/${id}/ack`), [['incidents']]);
export const useAddSilence = () => useWrite((b: Partial<Silence>) => post('/silences', b), [['silences'], ['incidents']]);
export const useDelSilence = () => useWrite((id: string) => del(`/silences/${id}`), [['silences'], ['incidents']]);
export const useAddTeam = () => useWrite((b: { name: string; sort_order?: number }) => post('/teams', b), [['teams']]);
export const usePatchTeam = () => useWrite(({ id, ...b }: { id: string; name?: string; sort_order?: number }) => patch(`/teams/${id}`, b), [['teams'], ...M]);
export const useRenumber = () => useWrite(({ id, token }: { id: string; token: string }) => post(`/teams/${id}/renumber`, { token }), [['teams'], ...M]);
export const fetchRenumber = (id: string) => get<RenumberPreview>(`/teams/${id}/renumber`);
export const useAddProvider = () => useWrite((b: Partial<Provider>) => post('/providers', b), [['providers']]);
export const usePatchProvider = () => useWrite(({ id, ...b }: { id: string } & Partial<Provider>) => patch(`/providers/${id}`, b), [['providers'], ...M]);
export const useAddPlan = () => useWrite((b: Record<string, unknown>) => post('/plans', b), [['plans']]);
export const usePatchPlan = () => useWrite(({ id, ...b }: { id: string } & Record<string, unknown>) => patch(`/plans/${id}`, b), [['plans'], ...M]);
export const fetchShare = (entryId: string) => get<{ url: string }>(`/entries/${entryId}/share`);
export const fetchLogs = (jobId: string, after: number) => get<JobLog[]>(`/jobs/${jobId}/logs?after=${after}`);
const LOG_PAGE = 1000; // 服务端单次最多返回 1000 条
/** 取 after 之后的全部日志：满页就接着往后拉，直到拉完 */
export async function fetchAllLogs(jobId: string, after: number): Promise<JobLog[]> {
  const all: JobLog[] = [];
  for (let guard = 0; guard < 200; guard++) {
    const page = await fetchLogs(jobId, after);
    all.push(...page);
    if (page.length < LOG_PAGE) break;
    after = page[page.length - 1].id;
  }
  return all;
}

// ---- 设置页：API 密钥与告警通知渠道 ----
export const useApiKeys = () => useQuery({ queryKey: ['api-keys'], queryFn: () => get<ApiKey[]>('/api-keys') });
export const useCreateApiKey = () => useWrite((b: { name: string; scopes: string[]; expires_days?: number }) => post<{ id: string; token: string }>('/api-keys', b), [['api-keys']]);
export const useRevokeApiKey = () => useWrite((id: string) => del(`/api-keys/${id}`), [['api-keys']]);
export const useNotifyChannels = () => useQuery({ queryKey: ['notify-channels'], queryFn: () => get<NotifyChannel[]>('/notify-channels') });
export const useAddChannel = () => useWrite((b: { name: string; kind: string; config: Record<string, string>; min_severity: string; enabled: boolean }) => post<{ id: string }>('/notify-channels', b), [['notify-channels']]);
export const usePatchChannel = () => useWrite(({ id, ...b }: { id: string; name?: string; config?: Record<string, string>; min_severity?: string; enabled?: boolean }) => patch(`/notify-channels/${id}`, b), [['notify-channels']]);
export const useDelChannel = () => useWrite((id: string) => del(`/notify-channels/${id}`), [['notify-channels'], ['notify-log']]);
export const useTestChannel = () => useWrite((id: string) => post<{ ok: boolean; error: string | null }>(`/notify-channels/${id}/test`), [['notify-log']]);
export const useNotifyLog = () => useQuery({ queryKey: ['notify-log'], queryFn: () => get<NotifyLogEntry[]>('/notify-log?limit=100') });
export const useRetirePreview = (id: string | null, enabled: boolean) => useQuery({ queryKey: ['retire-preview', id], queryFn: ({ signal }) => get<RetirePreview>(`/machines/${id}/retire`, signal), enabled: !!id && enabled, staleTime: 0, gcTime: 0, refetchOnWindowFocus: false, refetchOnReconnect: false });
