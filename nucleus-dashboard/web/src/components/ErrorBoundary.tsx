import { Component, type ReactNode } from "react";

// Route-level error boundary (ADR-020). A render crash in one page must
// not blank the whole app — the sidebar/topbar stay alive and the crashed
// surface shows a recoverable error panel instead. Mounted in App.tsx
// keyed by location.pathname, so navigating away auto-resets the boundary.
//
// Class component on purpose: React 19 still has no hook equivalent of
// getDerivedStateFromError / componentDidCatch.

type Props = { children: ReactNode };
type State = { error: Error | null };

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error): void {
    // Surface in the console for debugging; the panel below is the
    // operator-facing signal.
    console.error("[ErrorBoundary] surface crashed:", error);
  }

  private reset = () => this.setState({ error: null });

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <div className="p-10 font-mono text-sm">
        <div className="text-[var(--color-status-down)]">
          [error] <span className="text-[var(--color-nucleus-accent)]">▸</span>{" "}
          this surface crashed
        </div>
        <pre className="mt-3 max-w-xl overflow-auto rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3 text-xs text-[var(--color-nucleus-faint)]">
          {this.state.error.message || String(this.state.error)}
        </pre>
        <button
          onClick={this.reset}
          className="mt-4 rounded border border-[var(--color-nucleus-border)] px-3 py-1.5 text-[var(--color-nucleus-accent)] hover:bg-[var(--color-nucleus-surface)]"
        >
          retry
        </button>
      </div>
    );
  }
}
