// ─── ErrorBoundary — last-line catcher for React render errors ──
//
// Hooks can't catch render-phase errors (only event-handler errors via
// try/catch and useState). React requires a class component with
// `componentDidCatch` / `getDerivedStateFromError`. We wrap:
//   1. App at the root (main.tsx) — a render crash anywhere shows the
//      `ErrorState` band instead of a blank white page.
//   2. Each canvas inside WorkspaceShell — one canvas crashing doesn't
//      blank the whole workspace; the chat rail keeps working.
//
// The `fallback` prop is consulted first; otherwise we render a
// centered editorial error card with a "Reload" affordance. We keep
// state minimal — the goal is to present a humane wall, not to invent
// recovery semantics. Hard reload is the right escape hatch.

import { Component, type ErrorInfo, type ReactNode } from "react";
import { ErrorState } from "./ErrorState";

export interface ErrorBoundaryProps {
  children: ReactNode;
  /** Optional render override. Receives the caught error + a reset
   *  callback that clears the boundary's state (re-mounts children).
   *  Useful for per-canvas wrappers where "switch tabs" already exists
   *  as the natural recovery path. */
  fallback?: (error: Error, reset: () => void) => ReactNode;
  /** Short label included in the default fallback. Helps the user know
   *  *which* surface failed when multiple boundaries exist. */
  label?: string;
  /** Optional callback for telemetry. We don't ship analytics here —
   *  the parent decides whether to log to console / track event. */
  onError?: (error: Error, info: ErrorInfo) => void;
}

interface ErrorBoundaryState {
  error: Error | null;
}

export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // Always log so the dev console shows a stack — production can
    // wire `onError` to a real reporter without us needing to import
    // anything here.
    // eslint-disable-next-line no-console
    console.error("[ErrorBoundary]", this.props.label ?? "(root)", error, info);
    this.props.onError?.(error, info);
  }

  reset = () => this.setState({ error: null });

  render() {
    if (!this.state.error) return this.props.children;

    if (this.props.fallback) {
      return this.props.fallback(this.state.error, this.reset);
    }

    const where = this.props.label ? ` in ${this.props.label}` : "";
    return (
      <div
        data-testid="error-boundary"
        className="h-full min-h-[200px] flex items-center justify-center p-6 bg-paper"
      >
        <div className="max-w-md w-full">
          <h3 className="font-display italic text-2xl font-medium text-ink mb-2">
            Something went sideways{where}.
          </h3>
          <p className="font-serif text-sm text-ink-soft mb-4">
            We've logged it. Try the action below — if it keeps happening,
            a refresh usually clears it.
          </p>
          <ErrorState
            message={this.state.error.message || "Unknown error"}
            onRetry={this.reset}
            retryLabel="Try again"
          />
        </div>
      </div>
    );
  }
}
