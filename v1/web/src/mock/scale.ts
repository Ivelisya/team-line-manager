// 性能测试用的机器规模（tools/perf.mjs）：sessionStorage 的 mock:machines = N 时把 mock 的机器列表调到 N 台（不含已回收的），默认不动（约 28 台）。
// 少于现有：从普通的在用 VPS 节点里按编号从大到小去掉（mock 启动时要找的几台不动；截图、冒烟脚本用的是默认规模），它们的入口一并从线路机上去掉；
// 多于现有：照几台普通节点的样子复制出新团队（团队3、团队4…，每队 24 台）的节点，各带一条新入口挂到原线路机上。
// 只改 db 里的数据，路由不动。也可以在地址里带 ?mock_machines=N 打开一次（写进 sessionStorage，刷新后照旧）。
// mock:ws-delay = 毫秒：实时流握手的耗时（默认 50）。生产上握手要 TCP + TLS + 升级几趟往返（每趟约 150 ms），性能测试按 350 设，首屏请求与连上的先后才像真的
import type { MachineView, Team } from '@/lib/types';
import { displayName } from '@/lib/format';
import { trafficOf, type Db } from './fixtures';

const KEY = 'mock:machines';
/** mock 启动时按 id 找的机器（进行中的上架、退役作业）和同机 IP 节点的宿主机：不能去掉 */
const PINNED = new Set(['m-t1-03', 'm-t1-13p', 'm-t1-14', 'm-t2-07']);
const PER_TEAM = 24;

export function scaleTarget(): number | null {
  try {
    const q = new URLSearchParams(location.search).get('mock_machines');
    if (q) sessionStorage.setItem(KEY, q);
    const n = Number(sessionStorage.getItem(KEY));
    return Number.isInteger(n) && n > 0 ? n : null;
  } catch { return null; }
}

/** mock 实时流从 new WebSocket 到 onopen 的毫秒数 */
export function streamOpenDelay(): number {
  try { const n = Number(sessionStorage.getItem('mock:ws-delay')); return Number.isFinite(n) && n > 0 ? n : 50; } catch { return 50; }
}

export function scaleMachines(db: Db, n: number) {
  const relays = db.machines.filter((m) => m.kind === 'relay');
  const dropEntries = (ids: Set<string>) => { for (const r of relays) r.entries = r.entries.filter((e) => !ids.has(e.node.id)); };
  if (n < db.machines.length) {
    const spare = db.machines.filter((m) => m.kind === 'vps' && m.lifecycle === 'active' && !m.host_machine && !m.ip_nodes.length && !PINNED.has(m.id))
      .sort((a, b) => (b.number ?? 0) - (a.number ?? 0));
    const out = new Set(spare.slice(0, db.machines.length - n).map((m) => m.id));
    db.machines = db.machines.filter((m) => !out.has(m.id));
    dropEntries(out);
    return;
  }
  // 复制的样板：普通的在用 VPS 节点（含故障 / 降级 / 维护的几台，状态分布接近真实）
  const tpl = db.machines.filter((m) => m.kind === 'vps' && !m.spare && !m.host_machine && !m.ip_nodes.length && m.lifecycle === 'active');
  let seq = 0;
  let eSeq = 9000;
  while (db.machines.length < n) {
    const ti = 3 + Math.floor(seq / PER_TEAM);
    const num = (seq % PER_TEAM) + 1;
    let team = db.teams.find((t) => t.id === `t${ti}`);
    if (!team) {
      team = { ...db.teams[0], id: `t${ti}`, name: `团队${ti}`, sort_order: ti } as Team;
      db.teams.splice(db.teams.findIndex((t) => t.spare), 0, team);
    }
    const src = tpl[seq % tpl.length];
    const id = `m-t${ti}-${String(num).padStart(2, '0')}`;
    const name = displayName(team.name, num);
    const m: MachineView = structuredClone(src);
    const relay = relays[seq % relays.length];
    Object.assign(m, { id, name, number: num, team: { id: team.id, name: team.name }, ip: `198.18.${ti}.${num}`, note: '', service_ref: `SVC-${5000 + seq}` });
    m.entries = m.entries.map((e) => ({ ...e, id: `e${eSeq++}`, node: { id, name }, relay: { id: relay.id, name: relay.name }, port: 21000 + seq }));
    relay.entries.push(...m.entries.map((e) => structuredClone(e)));
    db.machines.push(m);
    seq++;
  }
  for (const m of db.machines) if (m.subscription) Object.assign(m, trafficOf(m, db.machines));
}
