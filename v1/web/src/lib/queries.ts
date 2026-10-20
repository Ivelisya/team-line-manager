// TanStack Query 封装：查询 hook + 写操作 hook。写操作成功后按范围失效（实时流也会再推一次，重复无害）。
import { useMemo } from 'react';
import { useMutation, useQuery, useQueryClient, type QueryClient } from '@tanstack/react-query';
import { del, get, patch, post, put } from './api';
import { makeDefaultCompare } from './sort';
import { jobLive } from './jobs';
import { usePoll } from './streamState';
import type { ApiKey, Incident, Job, JobLog, MachineView, Overview, Plan, Provider, ProvisionCheckItem, ProvisionItem, Renewal, RenumberPreview, RetirePreview, SeriesResp, Silence, Team, AuditEntry, NotifyChannel, NotifyLogEntry } from './types';

export const useMe = () => useQuery({ queryKey: ['me'], queryFn: () => get<{ username: string }>('/auth/me'), retry: false, staleTime: 5 * 60_000 });
export const useTeams = () => useQuery({ queryKey: ['teams'], queryFn: () => get<Team[]>('/teams'), staleTime: 60_000 });
export const useProviders = () => useQuery({ queryKey: ['providers'], queryFn: () => get<Provider[]>('/providers'), staleTime: 60_000 });
export const usePlans = () => useQuery({ queryKey: ['plans'], queryFn: () => get<Plan[]>('/plans'), staleTime: 60_000 });
export function useOverview() {
  const poll = usePoll(30_000);
  return useQuery({ queryKey: ['overview'], queryFn: () => get<Overview>('/overview'), refetchInterval: poll });
}

/** 机器列表，已按默认顺序排好：线路机 → 团队 → 编号（故障/离线不下沉） */
export function useMachines(scope: 'live' | 'retired' | 'all' = 'live') {
  const poll = usePoll(30_000);
  const q = useQuery({ queryKey: ['machines', scope], queryFn: () => get<MachineView[]>(`/machines?scope=${scope}`), refetchInterval: poll });
  const teams = useTeams();
  const data = useMemo(() => (q.data ? [...q.data].sort(makeDefaultCompare(teams.data ?? [])) : undefined), [q.data, teams.data]);
  return { ...q, data };
}
export function useMachine(id: string | null) {
  const poll = usePoll(30_000);
  return useQuery({ queryKey: ['machine', id], queryFn: () => get<MachineView>(`/machines/${id}`), enabled: !!id, refetchInterval: poll });
}
/** 悬浮卡用：先用机器列表缓存里的那份（立即有内容），再按需取单台；不轮询 */
export function useMachinePreview(id: string | null) {
  const qc = useQueryClient();
  return useQuery({
    queryKey: ['machine', id], queryFn: () => get<MachineView>(`/machines/${id}`), enabled: !!id, staleTime: 15_000,
    initialData: () => qc.getQueryData<MachineView[]>(['machines', 'live'])?.find((m) => m.id === id),
    initialDataUpdatedAt: () => qc.getQueryState(['machines', 'live'])?.dataUpdatedAt,
  });
}
export function useRelays() {
  const poll = usePoll(30_000);
  const q = useQuery({ queryKey: ['relays'], queryFn: () => get<MachineView[]>('/relays'), refetchInterval: poll });
  const data = useMemo(() => (q.data ? [...q.data].sort((a, b) => a.name.localeCompare(b.name, 'zh-Hans-CN', { numeric: true })) : undefined), [q.data]);
  return { ...q, data };
}
/** poll：卡片上的心跳条没有抽屉那样的 tick 失效，自己按这个间隔刷新 */
export const useSeries = (id: string | null, metric: 'host' | 'traffic' | 'probe', range: string, enabled = true, poll?: number) =>
  useQuery({ queryKey: ['series', id, metric, range], queryFn: () => get<SeriesResp>(`/machines/${id}/series?metric=${metric}&range=${range}`), enabled: !!id && enabled, staleTime: 30_000, refetchInterval: poll });
export const useRenewals = (id: string | null) => useQuery({ queryKey: ['renewals', id], queryFn: () => get<Renewal[]>(`/machines/${id}/renewals`), enabled: !!id });
export const useAudit = (id: string | null) => useQuery({ queryKey: ['audit', id], queryFn: () => get<AuditEntry[]>(`/audit?target_id=${id}&limit=10`), enabled: !!id });
export function useJobs(state: 'active' | 'done' | 'all' = 'all') {
  const poll = usePoll(10_000);
  return useQuery({ queryKey: ['jobs', state], queryFn: () => get<Job[]>(`/jobs?state=${state}&limit=100`), refetchInterval: poll });
}
export const useJob = (id: string | null) => useQuery({ queryKey: ['job', id], queryFn: () => get<Job>(`/jobs/${id}`), enabled: !!id, refetchInterval: (q) => (q.state.data && jobLive(q.state.data.state) ? 2500 : false) });
export function useIncidents(state: 'firing' | 'resolved' | 'all' = 'all') {
  const poll = usePoll(30_000);
  return useQuery({ queryKey: ['incidents', state], queryFn: () => get<Incident[]>(`/incidents?state=${state}`), refetchInterval: poll });
}
export const useSilences = () => useQuery({ queryKey: ['silences'], queryFn: () => get<Silence[]>('/silences') });

export interface StreamEvent { type: string; id?: string }
/** 按事件类型让查询失效（实时流用）。resync 由 stream.ts 处理成全部失效 */
export function invalidateFor(qc: QueryClient, ev: StreamEvent) {
  const inv = (...keys: unknown[][]) => keys.forEach((k) => qc.invalidateQueries({ queryKey: k }));
  switch (ev.type) {
    case 'resync': qc.invalidateQueries(); break;
    case 'machine': inv(['machine', ev.id], ['machines'], ['overview']); break;
    case 'machines': inv(['machine'], ['machines'], ['relays'], ['overview']); break;
    case 'relay': inv(['relays'], ['machines']); break;
    case 'job': inv(['jobs'], ['job', ev.id], ['machines'], ['machine']); break;
    case 'incidents': inv(['incidents'], ['silences'], ['overview'], ['machines']); break;
    case 'catalog': inv(['teams'], ['providers'], ['plans'], ['machines'], ['machine'], ['notify-channels']); break;
    case 'api_keys': inv(['api-keys']); break;
    case 'policies': inv(['policies'], ['machines'], ['relays']); break;
    case 'tick': {
      inv(['machines'], ['machine'], ['relays'], ['overview']);
      // 曲线只刷新当前打开的抽屉，其余视图的曲线不在每个 tick 重取
      const open = new URLSearchParams(window.location.search).get('m');
      if (open) inv(['series', open]);
      break;
    }
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
export const useAddPlan = () => useWrite((b: Partial<Plan>) => post('/plans', b), [['plans']]);
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
export const useRetirePreview = (id: string | null, enabled: boolean) => useQuery({ queryKey: ['retire-preview', id], queryFn: () => get<RetirePreview>(`/machines/${id}/retire`), enabled: !!id && enabled, staleTime: 0, gcTime: 0 });
