// 「改软路由出口」步骤的结果（替换节点、换线路机、搬空）：每个出口一行「软路由 · 出口 → 改成哪台线路机:端口 · 改后测速」，
// 不再原样输出 JSON。软路由名点开它的抽屉；备份文件的路径放在软路由名的悬停里。样式同调优结果。
import { Button } from '@/components/ui/button';
import { useAddrName, useRouterDrawer } from '@/lib/routers';
import { ms, str } from '@/lib/format';

interface Edited { id: string; name: string; exits: { exit: string; to: string }[]; backup?: string | null; delay_ms?: (number | null)[] }

/** 步骤输出是不是改软路由出口的结果（有 routers 数组） */
export const isRouterEditOutput = (o: Record<string, unknown> | null | undefined): o is Record<string, unknown> & { routers: Edited[] } => !!o && Array.isArray(o.routers);

export function RouterEditResult({ out }: { out: { routers: Edited[] } }) {
  const { open } = useRouterDrawer();
  const addr = useAddrName();
  const rows = out.routers.flatMap((r) => r.exits.map((e, i) => ({ r, e, delay: r.delay_ms?.[i] })));
  const n = out.routers.length;
  return (
    <div className="tune-res">
      <div className="tune-sum">改了 <b className="num">{n}</b> 台软路由的 <b className="num">{rows.length}</b> 个出口。每台都先备份了原配置，改完重启 OpenClash 并测了延迟</div>
      <table className="tune-tbl">
        <thead><tr><th>软路由</th><th>出口</th><th>改成</th><th>改后测速</th></tr></thead>
        <tbody>
          {rows.map(({ r, e, delay }) => {
            const to = addr(e.to);
            return (
              <tr key={`${r.id}-${e.exit}`}>
                <td><Button variant="link" size="inline-sm" data-tip={`打开软路由详情${r.backup ? `\n备份文件：${r.backup}` : ''}`} onClick={() => open(r.id, 'exits')}>{r.name}</Button></td>
                <td className="mono">{e.exit}</td>
                <td className="num" data-tip={to.tip}>{to.text}</td>
                <td className="num">{delay != null ? str(ms(delay)) : <span className="t-muted">没测到</span>}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
