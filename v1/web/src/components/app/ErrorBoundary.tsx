// 渲染出错时显示可重试的卡片，不要白屏。resetKey 变化（换了页面）时自动清除错误。
// 发版后旧页面去拉已被替换掉的懒加载块会 404；React.lazy 会缓存这次失败，重试无效，只能刷新，所以单独提示。
// silent：用于抽屉/对话框宿主这类浮层——出错时不渲染占位（避免挤坏布局），只弹一条提示并通过 onError 收起浮层。
import * as React from 'react';
import { TriangleAlert } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { toast } from '@/components/ui/toast';

interface Props { children: React.ReactNode; resetKey?: string; compact?: boolean; silent?: boolean; onError?: (error: Error) => void }
interface State { error: Error | null }

/** 懒加载的块拉取失败（多半是发版后旧文件已被删除） */
function isChunkError(e: unknown): boolean {
  const s = e instanceof Error ? `${e.name} ${e.message}` : String(e);
  return /Failed to fetch dynamically imported module|error loading dynamically imported module|Importing a module script failed|ChunkLoadError|Loading chunk [\w-]+ failed|Loading CSS chunk/i.test(s);
}

export class ErrorBoundary extends React.Component<Props, State> {
  state: State = { error: null };
  static getDerivedStateFromError(error: Error): State { return { error }; }
  componentDidCatch(error: Error, info: React.ErrorInfo) {
    console.error('[ErrorBoundary]', error, info.componentStack);
    if (this.props.silent) toast.err(isChunkError(error) ? '页面已更新，请刷新页面后再试' : `界面出错：${error.message || '未知错误'}`);
    this.props.onError?.(error);
  }
  componentDidUpdate(prev: Props) { if (this.state.error && prev.resetKey !== this.props.resetKey) this.setState({ error: null }); }
  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    if (this.props.silent) return null;
    const stale = isChunkError(error);
    return (
      <div className="panel err-card" role="alert">
        <TriangleAlert size={22} />
        <div className="err-card-t">{stale ? '有新版本' : this.props.compact ? '这部分内容出错了' : '这个页面出错了'}</div>
        <div className="hint" style={{ maxWidth: 520, overflowWrap: 'anywhere' }}>{stale ? '刷新页面就能用上新版' : error.message || '未知错误'}</div>
        <div className="row" style={{ justifyContent: 'center' }}>
          {!stale && <Button onClick={() => this.setState({ error: null })}>重试</Button>}
          <Button variant={stale ? 'default' : 'outline'} onClick={() => window.location.reload()}>刷新页面</Button>
        </div>
      </div>
    );
  }
}
