// 生命周期操作（换线、搬空、替换、退役）里几处页面上也要用的小件：判断能不能做、补号说明。
// 单独成文件：抽屉「管理」页签、线路机页只要这几样，不必为此静态带上整套对话框（对话框在打开时才懒加载）
import * as React from 'react';
import type { MachineView } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { InfoTip } from '@/components/ui/info-tip';
import { Note } from './primitives';

/** 节点当前在用的入口（可以被换走的）。线路机的直出入口不搬：退役线路机时随之摘除 */
export const movableEntries = (m: MachineView) => m.entries.filter((e) => e.lifecycle === 'active' && !e.direct);

/** 能接替的节点：在用、不是线路机、不是自己 */
export const canReplace = (m: MachineView) => m.kind !== 'relay' && m.lifecycle === 'active';

/** 编号压缩（后面的节点前移补号）：正常的副作用，收成一行，要看逐台再展开成两列表格（替换、退役共用） */
/** bare：不套提示条（放进别的提示条的列表里，和其它几条说明合成一块时用） */
export function RenameNote({ renames, tip, bare }: { renames: { machine_id: string; old_name: string; new_name: string }[]; tip: string; bare?: boolean }) {
  const [open, setOpen] = React.useState(false);
  const first = renames[0]; const last = renames.at(-1);
  if (!first || !last) return null; // 没有改号
  const Wrap = bare ? 'div' : Note;
  return (
    <Wrap style={bare ? { flex: 1, minWidth: 0 } : undefined}>
      <div className="row row-between" style={{ gap: 8 }}>
        {/* 一行说完：只写台数（一台时写 旧名 → 新名），改了哪几个名放进 (i) */}
        <span>补号：{renames.length > 1 ? `${renames.length} 台节点前移` : `${first.old_name} → ${first.new_name}`} <InfoTip text={renames.length > 1 ? `${tip}\n${first.old_name} … ${last.old_name}` : tip} /></span>
        {renames.length > 1 && <Button variant="link" size="inline-sm" aria-expanded={open} onClick={() => setOpen((v) => !v)}>{open ? '收起' : '展开'}</Button>}
      </div>
      {open && (
        <div style={{ maxHeight: 200, overflow: 'auto', marginTop: 6 }}>
          <table className="rx-tbl">
            <thead><tr><th>原名</th><th>补号后</th></tr></thead>
            <tbody>{renames.map((r) => <tr key={r.machine_id}><td className="num">{r.old_name}</td><td className="num"><b>{r.new_name}</b></td></tr>)}</tbody>
          </table>
        </div>
      )}
    </Wrap>
  );
}
