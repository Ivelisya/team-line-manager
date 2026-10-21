// 实时流连接状态的小仓库：顶栏指示据此显示。断开期间不轮询，重连后由 stream.ts 让全部查询失效补上。
import { createStore } from './store';

export type StreamState = 'connecting' | 'open' | 'error';

const status = createStore<StreamState>('connecting');
export const setStreamState = status.set;
export const useStreamState = status.use;
