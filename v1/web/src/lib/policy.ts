// 访问规则（策略）：类型 + 查询 / 写操作 hook。接口见 v1/docs/API.md「访问策略」。
// 编辑只改草稿（乐观锁 revision）；预览返回 draft_hash，发布 / 丢弃必须带上且一致。
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { get, post, put } from './api';
import { usePoll } from './streamState';

export type PolicyMode = 'observe' | 'allowlist' | 'denylist' | 'blocked' | 'inherit';
export type EffectiveMode = Exclude<PolicyMode, 'inherit'>;

export const MODE_LABEL: Record<PolicyMode, string> = {
  observe: '仅观察', allowlist: '白名单', denylist: '黑名单', blocked: '暂停新连接', inherit: '跟随团队',
};
export const MODE_HINT: Record<PolicyMode, string> = {
  observe: '不产生任何规则，只记录访问。',
  allowlist: '只放行名单内的域名，其余连接全部阻止。',
  denylist: '阻止名单内的域名，其余连接照常放行。',
  blocked: '暂停该范围内所有新连接（已建立的连接不受影响）。',
  inherit: '使用所在团队的设置。',
};

export interface PolicyBody { mode: PolicyMode; domains: string[]; note: string }
export interface ScopePolicy extends PolicyBody {
  scope: 'team' | 'node'; id: string;
  /** 未发布的草稿；null = 没有改动 */
  draft: PolicyBody | null;
  /** 乐观锁；还没有记录的范围为 0 */
  revision: number; updated_by: string | null; updated_at: string | null;
}
export interface EffectivePolicy { mode: EffectiveMode; domains: string[]; note: string; source: 'node' | 'team' | 'default' }
export interface PolicyNode {
  id: string; name: string; number: number | null; has_entry: boolean;
  policy: ScopePolicy; effective: EffectivePolicy; draft_effective: EffectivePolicy;
}
export interface PolicyTeam { id: string; name: string; policy: ScopePolicy; nodes: PolicyNode[] }
export interface PolicyList { draft_hash: string | null; draft_count: number; teams: PolicyTeam[] }

export interface PolicyChange { scope: 'team' | 'node'; id: string; name: string; from: { mode: PolicyMode; domains: number }; to: { mode: PolicyMode; domains: number } }
export interface PreviewNode {
  node_id: string; name: string; team: string | null; mode: EffectiveMode; source: 'node' | 'team' | 'default'; changed: boolean;
  total_conns: number; would_block: number; current_blocked: number; domains: { host: string; conns: number }[];
}
export interface PolicyPreview {
  draft_hash: string | null; draft_count: number; window_hours: number; affected_relays: number; changes: PolicyChange[]; nodes: PreviewNode[];
}

export type VersionStatus = 'applying' | 'applied' | 'failed';
export interface VersionRelay {
  relay_id: string; name: string | null; generation: number; applied_generation: number | null;
  status: VersionStatus | 'gone'; error: string | null;
}
export interface PolicyVersion {
  id: string; seq: number; kind: 'publish' | 'rollback'; rollback_of_seq: number | null; actor: string; created_at: string;
  status: VersionStatus; changes: PolicyChange[]; relays: VersionRelay[];
  is_latest: boolean; is_current: boolean; rollbackable: boolean;
}

export const usePolicies = () => {
  const poll = usePoll(30_000);
  return useQuery({ queryKey: ['policies', 'list'], queryFn: () => get<PolicyList>('/policies'), refetchInterval: poll });
};

/** 发布记录：有进行中的版本时 3 秒轮询一次（线路机回报应用结果不会触发实时事件） */
export const usePolicyVersions = () => {
  const poll = usePoll(30_000);
  return useQuery({
    queryKey: ['policies', 'versions'], queryFn: () => get<PolicyVersion[]>('/policies/versions'),
    refetchInterval: (q) => (q.state.data?.some((v) => v.status === 'applying') ? 3000 : poll),
  });
};

/** 预览是 POST 但没有副作用：对话框打开时取一次，每次打开都重新算 */
export const usePolicyPreview = (open: boolean) =>
  useQuery({ queryKey: ['policies', 'preview'], queryFn: () => post<PolicyPreview>('/policies/preview'), enabled: open, staleTime: 0, gcTime: 0 });

export interface SaveBody { mode: PolicyMode; domains: string[]; note: string; expected_revision: number }
export function useSavePolicy() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ scope, id, ...b }: SaveBody & { scope: 'team' | 'node'; id: string }) => put<{ policy: ScopePolicy; draft_hash: string | null; draft_count: number }>(`/policies/${scope}/${id}`, b),
    // 失败（多半是 409 草稿已被修改）也重新取一遍，让界面拿到最新的 revision
    onSettled: () => { void qc.invalidateQueries({ queryKey: ['policies'] }); },
  });
}
export function usePublishPolicies() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (draft_hash: string) => post<PolicyVersion>('/policies/publish', { draft_hash }),
    onSettled: () => { void qc.invalidateQueries({ queryKey: ['policies'] }); void qc.invalidateQueries({ queryKey: ['machines'] }); void qc.invalidateQueries({ queryKey: ['relays'] }); },
  });
}
export function useDiscardPolicies() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (draft_hash: string) => post<{ discarded: number }>('/policies/discard', { draft_hash }),
    onSettled: () => { void qc.invalidateQueries({ queryKey: ['policies'] }); },
  });
}
export function useRollbackPolicy() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => post<PolicyVersion>(`/policies/versions/${id}/rollback`),
    onSettled: () => { void qc.invalidateQueries({ queryKey: ['policies'] }); void qc.invalidateQueries({ queryKey: ['machines'] }); void qc.invalidateQueries({ queryKey: ['relays'] }); },
  });
}
