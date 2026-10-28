// 时刻显示按组织时区（P-23）：浏览器在别的时区（如 UTC）时，00:00–08:00 SGT 的告警不能显示成前一天。
// 跑法：pnpm test（node 自带的 test runner，直接跑 TS）；固定 TZ=UTC 模拟浏览器时区和组织时区不同
import { test } from 'node:test';
import assert from 'node:assert/strict';
import * as F from '../src/lib/format.ts';

const setTz = (tz?: string) => (F as { setTimeZone?: (tz?: string) => void }).setTimeZone?.(tz);

test('跨日：UTC 前一天 18:30 = 新加坡次日 02:30', () => {
  setTz('Asia/Singapore');
  const t = '2026-10-28T18:30:05Z';
  assert.equal(F.dt(t), '10-29 02:30');
  assert.equal(F.md(t), '10-29');
  assert.equal(F.dtFull(t), '2026-10-29 02:30');
  assert.equal(F.hm(t), '02:30');
  assert.equal(F.clock(t), '02:30:05');
  assert.equal(F.dt(Date.parse(t)), '10-29 02:30');
});

test('跨年、跨月同样按组织时区', () => {
  setTz('Asia/Singapore');
  assert.equal(F.dtFull('2026-12-31T23:59:00Z'), '2027-01-01 07:59');
  assert.equal(F.dt('2026-10-31T16:00:00Z'), '11-01 00:00');
});

test('西半球组织时区：UTC 次日凌晨仍是当地前一天', () => {
  setTz('America/Los_Angeles');
  assert.equal(F.dt('2026-10-29T03:00:00Z'), '10-28 20:00');
});

test('没设组织时区（/meta 取回前）用本机时区；空值为「—」', () => {
  setTz(undefined);
  assert.equal(F.dt('2026-10-28T18:30:00Z'), '10-28 18:30');
  assert.equal(F.dt(null), '—');
  assert.equal(F.dtFull(''), '—');
});
