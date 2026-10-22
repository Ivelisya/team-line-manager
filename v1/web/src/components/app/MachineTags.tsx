// 机器的标签：卡片标签行（三种密度）与抽屉头共用同一套、同一顺序——供应商 → 类型（只给线路机、家宽代理；VPS 节点是默认，不出）→ 同机关系 → 维护中。
// 每枚都有悬停说明（「供应商：DeerVPS」「类型：线路机」…）。服务编号是标识不是分类，不进标签，在抽屉概览里（带复制）。
import type { MachineView } from '@/lib/types';
import { tagColor, type TagColor } from '@/lib/derive';
import { KIND_LABEL } from '@/lib/sort';
import { HostTag } from './primitives';
import { Tag } from './viz';

const KIND_TAG = { relay: 'teal', proxy: 'orange' } as const;
export type TagItem = { key: string; c: TagColor; text: string; tip: string } | { key: 'rel' };
type Src = Pick<MachineView, 'provider' | 'provider_code' | 'kind' | 'maintenance' | 'host_machine' | 'ip_nodes'>;

/** 标签项：同机关系（rel）由 HostTag 自己画（可点、自带说明） */
export function machineTagItems(m: Src): TagItem[] {
  const items: TagItem[] = [];
  const provider = m.provider?.name ?? m.provider_code;
  if (provider) items.push({ key: 'provider', c: tagColor(m.provider_code ?? provider), text: provider, tip: `供应商：${provider}` });
  if (m.kind !== 'vps') items.push({ key: 'kind', c: KIND_TAG[m.kind], text: KIND_LABEL[m.kind], tip: `类型：${KIND_LABEL[m.kind]}` });
  if (m.host_machine || m.ip_nodes.length) items.push({ key: 'rel' });
  if (m.maintenance) items.push({ key: 'maint', c: 'amber', text: '维护中', tip: '维护中：在维护窗口里，不发告警、暂停健康判定' });
  return items;
}

/** 一枚标签（同机关系是可点的 HostTag） */
export function MachineTag({ t, m, tabIndex }: { t: TagItem; m: Pick<MachineView, 'id' | 'host_machine' | 'ip_nodes'>; tabIndex?: number }) {
  return 'c' in t ? <Tag c={t.c} tip={t.tip}>{t.text}</Tag> : <HostTag id={m.id} host={m.host_machine} nodes={m.ip_nodes} tabIndex={tabIndex} />;
}

/** 不折叠的一排（抽屉头） */
export function MachineTags({ m }: { m: Src & Pick<MachineView, 'id'> }) {
  return <>{machineTagItems(m).map((t) => <MachineTag key={t.key} t={t} m={m} />)}</>;
}
