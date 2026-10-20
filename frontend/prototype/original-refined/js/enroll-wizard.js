// enroll-wizard.js — “新增节点”入口的薄封装。全站只有一个“新增家宽节点”向导：js/provision.js 的 openProvisionWizard（线路页构建者实现，
// 原版 openProvisionWizard('node')，inventory ops §4.1；SPEC §3.17、§6.3、§7.3）。运营总览、节点页的主按钮“新增节点”都走这里：
//   openEnrollWizard(app)                      → 默认选第一台线路机
//   openEnrollWizard(app, { relay: '线路机2' })  → 预选线路机（名称或 id 都行）
import { openProvisionWizard } from './provision.js';

export function openEnrollWizard(app, { relay = null, onJob } = {}) {
  const r = relay ? app.store.relay(relay) : null;
  return openProvisionWizard(app, 'node', { relayId: r?.id ?? null, onJob });
}
