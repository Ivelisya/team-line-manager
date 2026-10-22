// 系统调优作业的步骤结果：摘要 + 「参数 / 结果 / 说明」表 + 其余几项写成一行中文（不再把 JSON 原样输出）。
import { InfoTip } from '@/components/ui/form';
import { mem, str } from '@/lib/format';

type Skipped = { key: string; why: string; got: string | null };
type Row = { key: string; result: string; note: string };

const arr = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : []);

/** 步骤输出是不是调优结果（有 applied 数组） */
export const isTuneOutput = (o: Record<string, unknown> | null | undefined): o is Record<string, unknown> => !!o && Array.isArray(o.applied);

export function TuneResult({ out }: { out: Record<string, unknown> }) {
  const applied = arr(out.applied), ok = arr(out.ok), kept = arr(out.kept);
  const skipped = (Array.isArray(out.skipped) ? out.skipped : []) as Skipped[];
  const rows: Row[] = [
    ...applied.map((key) => ({ key, result: '已应用', note: '' })),
    ...skipped.map((s) => ({ key: s.key, result: '跳过', note: s.why === 'readonly' ? `只读${s.got ? `（当前 ${s.got}）` : ''}` : '内核没有这一项' })),
    ...kept.map((key) => ({ key, result: '保持现值', note: '现值比推荐值更大' })),
    ...ok.map((key) => ({ key, result: '已是推荐值', note: '' })),
  ];
  const container = out.container === true;
  const ntp = out.ntp === 'synced' ? '时间同步正常' : out.ntp === 'unsynced' ? '时间还没同步' : out.ntp === 'container' ? '时间同步由宿主机负责' : '';
  const extra = [
    container && '容器内',
    out.bbr != null && `BBR ${out.bbr ? '已启用' : '未启用'}`,
    ntp,
    out.journald === 'changed' ? 'journald 已限额' : out.journald === 'same' ? 'journald 限额已是最新' : '',
    out.nofile ? `文件句柄 ${String(out.nofile)}` : '',
    out.swap_mb != null && `交换 ${String(out.swap_mb)} MB`,
    out.mem_mb != null && `内存 ${str(mem(Number(out.mem_mb) * 1024 * 1024))}`,
  ].filter(Boolean);
  return (
    <div className="tune-res">
      <div className="tune-sum">已应用 <b className="num">{applied.length}</b> 项 · 保持 <b className="num">{ok.length + kept.length}</b> 项 · 跳过 <b className="num">{skipped.length}</b> 项
        <InfoTip text={'已应用：这次写入了推荐值\n保持：现值已是推荐值，或比推荐值更大（不往小改）\n跳过：内核没有这一项，或容器内只读'} /></div>
      {rows.length > 0 && (
        <table className="tune-tbl">
          <thead><tr><th>参数</th><th>结果</th><th>说明</th></tr></thead>
          <tbody>{rows.map((r) => <tr key={r.key}><td className="mono">{r.key}</td><td>{r.result}</td><td>{r.note}</td></tr>)}</tbody>
        </table>
      )}
      {extra.length > 0 && <div className="tune-sum">{extra.join(' · ')}</div>}
    </div>
  );
}
