// 终端和抽屉的层级（P-20）：终端面板在抽屉下面一层（抽屉、对话框临时盖在它上面）。所以打开终端时先收起开着的抽屉，关掉终端再按原样打开
// ——不管从哪儿打开（抽屉里的「终端」按钮、Ctrl+K、机器菜单）。跑法：pnpm test
import { test } from 'node:test';
import assert from 'node:assert/strict';
import * as T from '../src/lib/terminal.ts';

const calls: string[] = [];
T.setTerminalStash(() => { calls.push('stash'); return () => calls.push('restore'); });

test('打开终端先收起抽屉，关掉终端再打开回来', () => {
  calls.length = 0;
  T.openTerminal({ kind: 'machine', id: 'a', name: 'A' });
  assert.deepEqual(calls, ['stash']);
  T.closeTerminal();
  assert.deepEqual(calls, ['stash', 'restore']);
});

test('指定了关窗回调的照它的来，不再另外收抽屉', () => {
  calls.length = 0;
  let back = 0;
  T.openTerminal({ kind: 'router', id: 'r', name: 'R' }, () => { back++; });
  T.closeTerminal();
  assert.deepEqual(calls, []);
  assert.equal(back, 1);
});

test('连着时换台：确认之前不动抽屉', () => {
  calls.length = 0;
  T.openTerminal({ kind: 'machine', id: 'a', name: 'A' });
  T.setTerminalLive(true);
  T.openTerminal({ kind: 'machine', id: 'b', name: 'B' });
  assert.deepEqual(calls, ['stash']);
  T.cancelTerminalAsk();
  T.setTerminalLive(false);
  T.closeTerminal();
  assert.deepEqual(calls, ['stash', 'restore']);
});
