// derive.ts 的主要纯函数：错误原因的中文化与端口合并、入口探测的判定与摘要、首页 KPI 用的几个列表归纳、心跳格
import { beforeAll, describe, expect, it } from 'vitest';
import {
  beatCells, beatRate, compactPorts, entryBad, errText, nextDue, nodeKpiWho, probeErr, probeFail, probeFailLabel, probeSummary,
  quotaCalibrated, quotaNotes, quotaTight, reachFail, reachFlaky, relayRttMs, setMeta, statusRank, successPct, uptimeTone,
} from '@/lib/derive';
import { entry, machine, META, probe } from './fixtures';

beforeAll(() => setMeta(META));

describe('errText / compactPorts', () => {
  it('常见英文错误换成中文，去掉行尾空白', () => {
    expect(errText('connection refused ')).toBe('连接被拒绝');
    expect(errText('no route to host')).toBe('网络不可达');
    expect(errText('broken pipe')).toBe('连接被关闭');
  });
  it('同一原因超过两个端口时合并成「N 个端口…」，明细放 parts', () => {
    const s = '中控连不上（端口 20001：超时 10 秒；端口 20002：超时 10 秒；端口 20003：超时 10 秒；端口 20004：拒绝）';
    const r = compactPorts(s);
    expect(r.text).toBe('中控连不上（3 个端口超时 10 秒；端口 20004：拒绝）');
    expect(r.parts).toHaveLength(4);
  });
  it('每个原因都不超过两个端口时原样返回', () => {
    const s = '（端口 1：超时；端口 2：拒绝）';
    expect(compactPorts(s)).toEqual({ text: s, parts: null });
    expect(compactPorts('没有端口列表')).toEqual({ text: '没有端口列表', parts: null });
  });
});

describe('probeErr', () => {
  it('超时写成「超时 N 秒」，（换成中文的）原文放第二行', () => {
    expect(probeErr('connect timeout after 5s')).toBe('超时 5 秒\nconnect 超时 5 秒');
    expect(probeErr('os error 110 (10 秒)')).toBe('超时 10 秒\nos error 110 (10 秒)');
    expect(probeErr('refused')).toBe('端口拒绝连接\n连接被拒绝');
  });
  it('「timed out」「time out」「time-out」也认成超时（原来正则漏了 timed out）', () => {
    expect(probeErr('connect timed out after 5s')).toBe('超时 5 秒\nconnect 超时 5 秒');
    expect(probeErr('read time-out')).toBe('连接超时\nread time-out');
    expect(probeErr('operation timed out')).toBe('连接超时\noperation 超时');
  });
  it('人话与原文一样时只写一行；没有原因为 undefined', () => {
    expect(probeErr('超时 5 秒')).toBe('超时 5 秒');
    expect(probeErr(null)).toBeUndefined();
  });
});

describe('入口探测判定', () => {
  it('TCP 不通优先于出口不通；直出、穿透不看 TCP', () => {
    const e = entry({ tcp: probe(false), egress: probe(false) });
    expect(probeFail(e)).toBe(e.tcp);
    expect(probeFailLabel(e)).toBe('TCP 不通');
    const direct = entry({ direct: true, tcp: probe(false), egress: probe(false) });
    expect(probeFail(direct)).toBe(direct.egress);
    expect(probeFailLabel(direct)).toBe('出口不通');
    expect(probeFail(entry({ mode: 'reverse', tcp: probe(false) }))).toBeNull();
  });
  it('没在运行、还没应用的入口不算失败', () => {
    expect(probeFail(entry({ lifecycle: 'pending', tcp: probe(false) } as never))).toBeNull();
    expect(probeFail(entry({ applied: false, tcp: probe(false) }))).toBeNull();
  });
  it('中控连不上：连续 3 次才算，1–2 次是「刚才连不上」', () => {
    const reach = (fails: number) => entry({ reach: { ok: false, ts: '', detail: null, fails } });
    expect(reachFail(reach(2))).toBeNull();
    expect(reachFlaky(reach(2))).not.toBeNull();
    expect(reachFail(reach(3))).not.toBeNull();
    expect(reachFlaky(reach(3))).toBeNull();
    expect(entryBad(reach(3))).toBe(true);
    expect(entryBad(entry({ apply_error: 'x' }))).toBe(true);
    expect(entryBad(entry())).toBe(false);
  });
});

describe('probeSummary', () => {
  it('TCP 读数取各入口中位数里最小的一条；连通数不含直出与穿透', () => {
    const m = machine({
      entries: [
        entry({ id: 'a', tcp: probe(true, 65) }),
        entry({ id: 'b', tcp: probe(true, 5), relay: { id: 'r2', name: '线路机2' } }),
        entry({ id: 'c', direct: true, tcp: null }),
        entry({ id: 'd', mode: 'reverse', tcp: probe(false, null) }),
        entry({ id: 'e', lifecycle: 'disabled' } as never),
      ],
    });
    const s = probeSummary(m);
    expect(s.tcp).toBe(5);
    expect(s.total).toBe(2); // a、b：经节点的
    expect(s.tcpOk).toBe(2);
    expect(s.all).toBe(4); // 在用的 4 条（含直出、穿透）
    expect(s.off).toBe(1);
    expect(s.reverse).toBe(1);
    expect(s.usable).toBe(4);
  });
  it('全部失败才算 tcpFail / egressFail', () => {
    const s = probeSummary(machine({ entries: [entry({ id: 'a', tcp: probe(false), egress: probe(false) }), entry({ id: 'b', tcp: probe(true), egress: probe(false) })] }));
    expect(s.tcpFail).toBe(false);
    expect(s.egressFail).toBe(true);
    expect(s.egressBad).toBe(2);
  });
});

describe('首页 KPI 的列表归纳', () => {
  it('statusRank：有问题的在前', () => {
    const ranks = ['ok', 'down', 'degraded'].map((status) => statusRank(machine({ health: { status, reasons: [] } } as never)));
    expect(ranks).toEqual([7, 1, 2]);
    expect(statusRank(machine({ lifecycle: 'failed' }))).toBe(0);
  });
  it('nodeKpiWho：不算线路机和闲置备用；备用机带上指着它的出口', () => {
    const list = [
      machine({ id: 'a', health: { status: 'down', reasons: [] } }),
      machine({ id: 'b', kind: 'relay', health: { status: 'down', reasons: [] } }),
      machine({ id: 'c', spare: true, router_exits: ['路由 · EXIT_1'] }),
      machine({ id: 'd', spare: true, idle_spare: true }),
    ];
    const who = nodeKpiWho(list);
    expect(who.down.map((x) => x.id)).toEqual(['a']);
    expect(who.held).toEqual([{ id: 'c', name: '团队1-01', spare: true, exits: ['路由 · EXIT_1'] }]);
  });
  it('nextDue：即将到期里天数最少的一台，不续费的备用机不算', () => {
    const sub = (due_days: number, due_state = 'soon') => ({ due_days, due_state }) as never;
    const list = [
      machine({ id: 'a', subscription: sub(5) }),
      machine({ id: 'b', subscription: sub(2), spare_unbilled: true }),
      machine({ id: 'c', subscription: sub(3) }),
      machine({ id: 'd', subscription: sub(1, 'overdue') }),
    ];
    expect(nextDue(list)).toEqual({ id: 'c', name: '团队1-01', days: 3 });
    expect(nextDue([])).toBeNull();
  });
  it('quotaTight：已用超过阈值或 7 天内用满的计数，已超限的排最前', () => {
    const q = (used_gb: number, limit_gb: number, forecast_full_at: string | null = null) => ({ ...machine().quota, used: used_gb * 1e9, limit_gb, limit_source: 'plan', forecast_full_at });
    const soon = new Date(Date.now() + 2 * 864e5).toISOString();
    const list = [
      machine({ id: 'a', name: 'A', quota: q(50, 100) } as never),
      machine({ id: 'b', name: 'B', quota: q(85, 100) } as never),
      machine({ id: 'c', name: 'C', quota: q(10, 100, soon) } as never),
      machine({ id: 'd', name: 'D', quota: q(120, 100) } as never),
    ];
    const t = quotaTight(list);
    expect(t.n).toBe(3);
    expect(t.top).toMatchObject({ id: 'd', pct: 120, over: true, full: null });
    expect(quotaTight(list.slice(0, 1))).toEqual({ n: 0, top: null });
  });
  it('按面板校准：月度原样全算；滚动 30 天逐日线性衰减，拆出录入后的实测，30 天后完全失效', () => {
    const at = '2026-11-07T04:00:00Z'; const now = Date.parse('2026-11-22T04:00:00Z');
    const q = (mode: string, used_gb: number, period_end: string | null) => machine({ quota: { ...machine().quota, mode, used: used_gb * 1e9, period_end, base: 300e9, base_at: at } } as never);
    const monthly = q('monthly', 320, '2026-12-01');
    expect(quotaCalibrated(monthly, now)).toBe(300e9);
    expect(quotaNotes(monthly, now)[0]).toBe('录入后实测 20 GB，加上按面板校准的读数 300 GB（11-07 录入）\n12-01 重置后只算实测');
    const rolling = q('rolling30', 162, null);
    expect(quotaCalibrated(rolling, now)).toBe(150e9);
    expect(quotaNotes(rolling, now)[0]).toBe('录入后实测 12 GB，加上按面板校准的读数 300 GB（11-07 录入）\n近 30 天滚动：逐日衰减，现在还算 150 GB，12-07 起完全失效');
    expect(quotaCalibrated(rolling, Date.parse('2026-12-08T00:00:00Z'))).toBe(0);
    expect(quotaCalibrated(machine(), now)).toBeNull();
  });
});

describe('心跳格', () => {
  it('节点按成功比例定色，线路机按断了几次', () => {
    expect(beatCells([[10, 10], [100, 85], [10, 5], [0, 0]] as never)).toEqual(['ok', 'warn', 'bad', null]);
    expect(beatCells([[200, 198], [200, 197], [100, 80], [100, 79]] as never)).toEqual(['ok', 'warn', 'warn', 'bad']); // 99% 与 80% 两道线
    expect(beatCells([[6, 6], [6, 5], [6, 4], [0, 0]] as never, true)).toEqual(['ok', 'warn', 'bad', null]);
  });
  it('relayRttMs 取中位数，beatRate 是总成功率', () => {
    expect(relayRttMs([[1, 1, 10], [1, 1, 30], [1, 1, null], [1, 1, 20]] as never)).toBe(20);
    expect(relayRttMs([[1, 1, 10], [1, 1, 30]] as never)).toBe(20);
    expect(relayRttMs([])).toBeNull();
    expect(beatRate([[10, 9], [10, 10]] as never)).toBe(95);
    expect(beatRate([])).toBeNull();
  });
  it('成功率：先按显示位数舍入再着色，不到 100% 不写成 100%', () => {
    expect(uptimeTone(99.86, 1)).toBeUndefined();
    expect(uptimeTone(99.86, 2)).toBe('warn');
    expect(uptimeTone(97.5)).toBe('bad');
    expect(successPct(99.999)).toBe('99.99%');
    expect(successPct(100)).toBe('100%');
    expect(successPct(null)).toBe('—');
  });
});
