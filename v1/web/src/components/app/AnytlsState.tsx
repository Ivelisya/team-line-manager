// 线路机的 AnyTLS（sing-box 外壳）状态：在跑 / 没在跑 / 出错 / 未安装；旧 agent 没报过为「未知」。线路机抽屉与线路页共用
import type { AnytlsStatus } from '@/lib/types';
import { InfoTip } from '@/components/ui/form';
import { errText } from '@/lib/derive';

export function AnytlsState({ a }: { a: AnytlsStatus | null | undefined }) {
  if (!a) return <span className="t-sub">未知 <InfoTip text="这台的 agent 还没报过 AnyTLS 状态（版本旧）" /></span>;
  if (!a.installed) return <span className="t-sub">未安装 <InfoTip text="中控 dist 里有 sing-box 后，升级这台的 agent 时装上；没装时入口只有 Reality" /></span>;
  const v = a.version ? ` · ${a.version}` : '';
  if (a.last_error) return <span style={{ color: 'var(--bad-text)' }}>出错{v} <InfoTip text={errText(a.last_error)} /></span>;
  return a.running ? <>在跑{v}</> : <span style={{ color: 'var(--warn-text)' }}>没在跑{v}</span>;
}
