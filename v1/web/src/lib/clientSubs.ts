// 客户端订阅（对应 docs/API.md「客户端订阅」）：自己的客户端（Clash Verge、OpenClash、v2rayN）按一个链接拉入口，
// 按规则在拉取时现算，换节点、换线路机、轮换凭据后客户端更新订阅就是新的。链接只在新建、轮换时返回一次（url）
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { del, get, patch, post } from './api';

/** team_mode：all 全部团队（不含备用团队）；only 只要 team_ids；except 全部团队（不含备用团队）除了 team_ids。
 *  include_ids：另外带上的机器（节点，或线路机 = 它的直出入口）；exclude_ids：一律不带，优先于其余规则 */
export interface SubRule { team_mode: 'all' | 'only' | 'except'; team_ids: string[]; direct: boolean; include_ids: string[]; exclude_ids: string[] }
/** items：按现在的入口算的条目数；last_fetch_*：最近一次成功拉取（10 分钟内只记一次），轮换后清空 */
export interface ClientSub {
  id: string; name: string; enabled: boolean; rule: SubRule; rule_text: string; items: number;
  token_at: string; last_fetch_at: string | null; last_fetch_ip: string | null;
}
/** 条目：客户端里的名字、走哪台线路机哪个端口（不含凭据） */
export interface ClientSubEntry { name: string; entry_id: string; relay: string; port: number }
export type ClientSubWithUrl = ClientSub & { url: string };

export const DEFAULT_RULE: SubRule = { team_mode: 'all', team_ids: [], direct: true, include_ids: [], exclude_ids: [] };

export const useClientSubs = () => useQuery({ queryKey: ['client-subs'], queryFn: ({ signal }) => get<ClientSub[]>('/client-subs', signal), staleTime: 30_000 });
export const useClientSub = (id: string | null) =>
  useQuery({ queryKey: ['client-subs', id], queryFn: ({ signal }) => get<ClientSub & { entries: ClientSubEntry[] }>(`/client-subs/${id}`, signal), enabled: !!id });

function useSubWrite<A, R>(fn: (a: A) => Promise<R>) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: fn, onSuccess: () => { void qc.invalidateQueries({ queryKey: ['client-subs'] }); void qc.invalidateQueries({ queryKey: ['audit'] }); } });
}
export const useCreateClientSub = () => useSubWrite((b: { name: string; rule: SubRule }) => post<ClientSubWithUrl>('/client-subs', b));
export const usePatchClientSub = () => useSubWrite(({ id, ...b }: { id: string; name?: string; rule?: SubRule; enabled?: boolean }) => patch<ClientSub>(`/client-subs/${id}`, b));
export const useRotateClientSub = () => useSubWrite((id: string) => post<ClientSubWithUrl>(`/client-subs/${id}/rotate`));
export const useDelClientSub = () => useSubWrite((id: string) => del(`/client-subs/${id}`));
