// 系统调优作业的步骤结果：摘要 + 「参数 / 结果 / 说明」表（只列写入的、跳过的、保留现值的；已是推荐值的收成一行，点开才列）+ 其余几项写成一行中文（不再把 JSON 原样输出）。
import * as React from 'react';
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
  const [more, setMore] = React.useState(false);
  const rows: Row[] = [
    ...applied.map((key) => ({ key, result: '已写入', note: '' })),
    ...skipped.map((s) => ({ key: s.key, result: '跳过', note: s.why === 'readonly' ? `只读${s.got ? `（当前 ${s.got}）` : ''}` : '内核没有这一项' })),
    ...kept.map((key) => ({ key, result: '保留现值', note: '现值比推荐值更大' })),
    ...(more ? ok.map((key) => ({ key, result: '已是推荐值', note: '' })) : []),
  ];
  const sum = ([['已写入', applied.length], ['已是推荐值', ok.length], ['保留现值', kept.length], ['跳过', skipped.length]] as const).filter(([, n]) => n > 0);
  const container = out.container === true;
  const ntp = out.ntp === 'synced' ? '时间同步正常' : out.ntp === 'unsynced' ? '时间还没同步' : out.ntp === 'container' ? '时间同步由宿主机负责' : '';
  const extra = [
    container && '容器内',
    out.bbr != null && `BBR ${out.bbr ? '已启用' : '未启用'}`,
    ntp,
    out.journald === 'changed' ? 'journald 已限额' : out.journald === 'same' ? 'journald 限额已是最新' : '',
    out.nofile ? `文件句柄 ${String(out.nofile)}` : '',
    out.swap_mb != null && `交换 ${str(mem(Number(out.swap_mb) * 1024 * 1024))}`,
    out.mem_mb != null && `内存 ${str(mem(Number(out.mem_mb) * 1024 * 1024))}`,
  ].filter(Boolean);
  return (
    <div className="tune-res">
      <div className="tune-sum">{sum.length ? sum.map(([label, n], i) => <React.Fragment key={label}>{i > 0 && ' · '}{label} <b className="num">{n}</b> 项</React.Fragment>) : '没有参数需要调整'}
        <InfoTip text={'已写入：这次写入了推荐值\n已是推荐值：现值本来就是，没有动\n保留现值：现值比推荐值更大，不往小改\n跳过：内核没有这一项，或容器内只读'} /></div>
      {rows.length > 0 && (
        <table className="tune-tbl">
          <thead><tr><th>参数</th><th>结果</th><th>说明</th></tr></thead>
          <tbody>{rows.map((r) => <tr key={r.key}><td className="mono">{r.key}</td><td>{r.result}</td><td>{r.note}</td></tr>)}</tbody>
        </table>
      )}
      {ok.length > 0 && <button type="button" className="btn btn-link btn-sm tune-more" aria-expanded={more} onClick={() => setMore((v) => !v)}>{more ? '收起' : `另 ${ok.length} 项已是推荐值，展开`}</button>}
      {extra.length > 0 && <div className="tune-sum">{extra.join(' · ')}</div>}
    </div>
  );
}
