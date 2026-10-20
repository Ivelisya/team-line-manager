// 渲染出错时显示可重试的卡片，不要白屏。resetKey 变化（换了页面）时自动清除错误。
import * as React from 'react';
import { TriangleAlert } from 'lucide-react';
import { Button } from '@/components/ui/button';

interface Props { children: React.ReactNode; resetKey?: string; compact?: boolean }
interface State { error: Error | null }

export class ErrorBoundary extends React.Component<Props, State> {
  state: State = { error: null };
  static getDerivedStateFromError(error: Error): State { return { error }; }
  componentDidCatch(error: Error, info: React.ErrorInfo) { console.error('[ErrorBoundary]', error, info.componentStack); }
  componentDidUpdate(prev: Props) { if (this.state.error && prev.resetKey !== this.props.resetKey) this.setState({ error: null }); }
  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div className="panel err-card" role="alert">
        <TriangleAlert size={22} />
        <div className="err-card-t">这个页面出错了</div>
        <div className="hint" style={{ maxWidth: 520, overflowWrap: 'anywhere' }}>{error.message || '未知错误'}</div>
        <div className="row" style={{ justifyContent: 'center' }}>
          <Button variant="primary" onClick={() => this.setState({ error: null })}>重试</Button>
          <Button onClick={() => window.location.reload()}>刷新页面</Button>
        </div>
      </div>
    );
  }
}
