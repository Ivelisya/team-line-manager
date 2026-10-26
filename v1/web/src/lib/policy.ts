// 访问规则（策略）：类型 + 查询 / 写操作 hook。接口见 v1/docs/API.md「访问策略」。
// 编辑只改草稿（乐观锁 revision）；预览返回 draft_hash，发布 / 丢弃必须带上且一致。
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { get, post, put } from './api';
import { meta } from './derive';

export type PolicyMode = 'observe' | 'allowlist' | 'denylist' | 'blocked' | 'inherit';
export type EffectiveMode = Exclude<PolicyMode, 'inherit'>;

/** 模式的中文名（/meta 的 policy_modes）；下面的说明是界面文案，放进模式旁的「(i)」 */
export const modeLabel = (m: string) => meta().policy_modes[m] ?? m;
export const MODE_INFO: Record<PolicyMode, string> = {
  observe: '只记录访问，不拦截',
  allowlist: '只放行名单内的域名，其余一律阻止',
  denylist: '只阻止名单内的域名',
  blocked: '已经建立的连接不受影响，新连接一律拒绝',
  inherit: '使用所在团队的设置',
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

/** domains_changed / note_changed：服务端判的「域名换了」「备注改了」；旧版本记录没有这两项 */
export interface PolicyChange {
  scope: 'team' | 'node'; id: string; name: string; from: { mode: PolicyMode; domains: number }; to: { mode: PolicyMode; domains: number };
  domains_changed?: boolean; note_changed?: boolean;
}
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
  id: string; seq: number; kind: 'publish' | 'rollback' | 'replace'; rollback_of_seq: number | null; actor: string; created_at: string;
  status: VersionStatus; changes: PolicyChange[]; relays: VersionRelay[];
  /** is_latest：最新一版（即当前生效的）；is_current：内容与当前相同（不可回滚） */
  is_latest: boolean; is_current: boolean; rollbackable: boolean;
}
/** 回滚预览：回滚到这个版本会改什么（当前 → 这个版本） */
export interface RollbackPreview { seq: number; rollbackable: boolean; changes: PolicyChange[] }

/** 草稿 / 发布由 policies 事件、范围树里的节点由机器与目录事件失效（见 queries.ts 的 eventKeys），不按时间过期 */
export const usePolicies = () => useQuery({ queryKey: ['policies', 'list'], queryFn: () => get<PolicyList>('/policies'), staleTime: Infinity });

/** 发布记录：有进行中的版本时 3 秒轮询一次（线路机回报应用结果不会触发实时事件）；新版本由 policies 事件失效 */
export const usePolicyVersions = () => useQuery({
  queryKey: ['policies', 'versions'], queryFn: () => get<PolicyVersion[]>('/policies/versions'), staleTime: Infinity,
  refetchInterval: (q) => (q.state.data?.some((v) => v.status === 'applying') ? 3000 : false),
});

/** 回滚确认框打开时取一次（版本列表不再逐份算差异） */
export const useRollbackPreview = (id: string | null) =>
  useQuery({ queryKey: ['policies', 'rollback', id], queryFn: () => get<RollbackPreview>(`/policies/versions/${id}/rollback`), enabled: !!id, staleTime: 0, gcTime: 0 });

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
