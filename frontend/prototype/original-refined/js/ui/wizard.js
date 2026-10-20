// wizard.js — 向导步骤条 WizardSteps（SPEC §3.17）：一行 `1 填写 ─ 2 确认 ─ 3 执行`。
// 圆圈 20：当前 = --accent-solid 底白字；已完成 = --ok 底 + Check；未到 = 1 px --line-strong 边 + --fg-2 字；文字 13；连线 1 px --line。
// 线路页的“新增线路机 / 新增家宽节点”向导首用（js/provision.js），别的向导也用它。

import { icon } from '../icons.js';
import { esc } from './dom.js';

/** wizardSteps(['填写', '确认', '执行'], current, { label }) → <ol class="wiz">；current 从 0 起。 */
export function wizardSteps(labels, current = 0, { label = '步骤' } = {}) {
  return `<ol class="wiz" aria-label="${esc(label)}">${labels.map((l, i) => {
    const state = i < current ? 'done' : i === current ? 'current' : 'todo';
    const line = i ? '<li class="wiz-line" aria-hidden="true"></li>' : '';
    return `${line}<li class="wiz-step" data-state="${state}"${state === 'current' ? ' aria-current="step"' : ''}><span class="wiz-dot">${state === 'done' ? icon('check', 12) : i + 1}</span><span>${esc(l)}</span></li>`;
  }).join('')}</ol>`;
}
