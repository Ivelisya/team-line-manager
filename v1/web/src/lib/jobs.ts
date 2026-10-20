// 作业的展示辅助：kind 中文名、state 分类、步骤输出摘要。取值以 API.md「返回形状补充」为准。
import type { Job, JobStep } from './types';

export const JOB_KIND: Record<string, string> = {
  provision_vps: '上架节点', provision_proxy: '上架代理节点', provision_relay: '上架线路机',
  attach_entry: '新增入口', retire: '退役', purge: '彻底删除', upgrade_agent: '升级 agent',
};
export const jobKindLabel = (k: string) => JOB_KIND[k] ?? k;

/** 进行中：还没到终态 */
export const jobLive = (state: string) => ['queued', 'running', 'waiting', 'cancelling'].includes(state);
export const stepDone = (s: JobStep) => s.state === 'succeeded' || s.state === 'done';
export const stepFailed = (s: JobStep) => s.state === 'failed';
export const stepSkipped = (s: JobStep) => s.state === 'skipped' || s.state === 'cancelled' || s.state === 'canceled';
export const stepRunning = (s: JobStep) => s.state === 'running' || s.state === 'waiting' || s.state === 'cancelling';
/** 已补偿：作业取消或失败后，做过的步骤被逆序撤销 */
export const stepCompensated = (s: JobStep) => s.state === 'compensated';
export const stepsDone = (j: Job) => j.steps.filter(stepDone).length;

/** 步骤的展示类别（步骤条、时间线共用） */
export type StepKind = 'done' | 'running' | 'failed' | 'skipped' | 'compensated' | 'pending';
export const stepKind = (s: JobStep): StepKind =>
  stepDone(s) ? 'done' : stepFailed(s) ? 'failed' : stepRunning(s) ? 'running' : stepCompensated(s) ? 'compensated' : stepSkipped(s) ? 'skipped' : 'pending';
export const STEP_KIND_LABEL: Record<StepKind, string> = { done: '已完成', running: '进行中', failed: '失败', skipped: '已跳过', compensated: '已补偿', pending: '等待' };

/** 总进度：已完成步骤 / 总步骤；作业成功时一定是 100 */
export function jobProgress(j: Job) {
  const total = j.step_count || j.steps.length || 1;
  const done = j.state === 'succeeded' ? total : Math.min(total, stepsDone(j));
  return { done, total, pct: (done / total) * 100 };
}
/** 当前（或失败的）步骤标题，进度浮层里显示 */
export function currentStepTitle(j: Job): string | null {
  return (j.steps.find(stepRunning) ?? j.steps.find(stepFailed))?.title ?? null;
}

/** error_kind 的中文说明；未收录的原样显示 */
export const ERROR_KIND_LABEL: Record<string, string> = {
  ssh_auth: 'SSH 认证失败', ssh_connect: 'SSH 连不上', timeout: '超时', network: '网络错误', permission: '权限不足',
  not_found: '资源不存在', conflict: '冲突', precondition: '前置条件不满足', agent: 'agent 异常', internal: '内部错误', cancelled: '已取消',
};
export const errorKindLabel = (k: string) => ERROR_KIND_LABEL[k] ?? k;

/** 步骤输出（JSON 对象）→ 每行 “key: value”；嵌套值用单行 JSON */
export function outputLines(o: JobStep['output']): string[] {
  if (o == null) return [];
  if (typeof o !== 'object') return [String(o)];
  return Object.entries(o).map(([k, v]) => `${k}: ${v !== null && typeof v === 'object' ? JSON.stringify(v) : String(v)}`);
}
