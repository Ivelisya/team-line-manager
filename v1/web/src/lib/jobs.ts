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
export const stepsDone = (j: Job) => j.steps.filter(stepDone).length;

/** 步骤输出（JSON 对象）→ 每行 “key: value”；嵌套值用单行 JSON */
export function outputLines(o: JobStep['output']): string[] {
  if (o == null) return [];
  if (typeof o !== 'object') return [String(o)];
  return Object.entries(o).map(([k, v]) => `${k}: ${v !== null && typeof v === 'object' ? JSON.stringify(v) : String(v)}`);
}
