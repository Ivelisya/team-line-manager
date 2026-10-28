// 时刻显示按组织时区（P-23）：浏览器在别的时区（如 UTC）时，00:00–08:00 SGT 的告警不能显示成前一天。
// 跑法：pnpm test（node 自带的 test runner，直接跑 TS）；固定 TZ=UTC 模拟浏览器时区和组织时区不同
import { test } from 'node:test';
import assert from 'node:assert/strict';
import * as F from '../src/lib/format.ts';

const setTz = F.setTimeZone;

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

// 复审：按天分组、零点对齐、datetime-local 输入同样按组织时区；非法时间戳不崩页
test('日期键、今天 / 昨天按组织时区', () => {
  setTz('Asia/Singapore');
  assert.equal(F.dayKey('2026-10-28T18:30:00Z'), '2026-10-29');
  const now = Date.parse('2026-10-28T17:00:00Z'); // 新加坡 10-29 01:00
  assert.equal(F.dayLabel('2026-10-28T16:30:00Z', now), '今天');
  assert.equal(F.dayLabel('2026-10-28T15:59:00Z', now), '昨天');
  assert.equal(F.dayLabel('2026-10-26T12:00:00Z', now), '10月26日 周一');
});

test('组织时区的零点', () => {
  setTz('Asia/Singapore');
  assert.equal(F.startOfDay(Date.parse('2026-10-28T18:30:00Z')), Date.parse('2026-10-28T16:00:00Z'));
  setTz('America/Los_Angeles');
  assert.equal(F.startOfDay(Date.parse('2026-10-29T03:00:00Z')), Date.parse('2026-10-28T07:00:00Z'));
});

test('datetime-local 的值按组织时区写、按组织时区解析', () => {
  setTz('Asia/Singapore');
  const t = Date.parse('2026-10-28T18:30:00Z');
  assert.equal(F.wallInput(t), '2026-10-29T02:30');
  assert.equal(F.parseWall('2026-10-29T02:30'), t);
  assert.ok(Number.isNaN(F.parseWall('')));
  assert.ok(Number.isNaN(F.parseWall('2026-10-29T')));
});

test('非法时间戳显示「—」、非法时区退回本机时区，不抛错', () => {
  setTz('Asia/Singapore');
  assert.equal(F.dt('garbage'), '—');
  assert.equal(F.hm(Number.NaN), '—');
  assert.equal(F.dtFull('2026-13-45'), '—');
  assert.equal(F.clock('x'), '—');
  assert.doesNotThrow(() => setTz('Not/AZone'));
  assert.equal(F.dt('2026-10-28T18:30:00Z'), '10-28 18:30');
});
