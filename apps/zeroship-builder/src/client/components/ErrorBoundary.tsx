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
// centered crystal ErrorState band with a "Try again" affordance. We
// keep state minimal — the goal is to present a humane wall, not to
// invent recovery semantics. Hard reload is the right escape hatch.

import { Component, type ErrorInfo, type ReactNode } from "react";
import { Button, Center, ErrorState } from "@zeroship/ui";
import "./ErrorBoundary.css";

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
      <Center data-testid="error-boundary" minHeight="12.5rem">
        <ErrorState
          live
          title={`Something went sideways${where}.`}
          description="We've logged it. Try the action below — if it keeps happening, a refresh usually clears it."
        >
          <ErrorState.Description className="zs-builder-error-boundary__message">
            {this.state.error.message || "Unknown error"}
          </ErrorState.Description>
          <ErrorState.Actions>
            <Button variant="filled" onClick={this.reset}>
              Try again
            </Button>
          </ErrorState.Actions>
        </ErrorState>
      </Center>
    );
  }
}
