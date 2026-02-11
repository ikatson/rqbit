import React, { Component, ErrorInfo, ReactNode } from "react";
import { ErrorComponent } from "./ErrorComponent";

interface Props {
  children?: ReactNode;
  fallback?: ReactNode;
  scope?: string;
}

interface State {
  hasError: boolean;
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  public state: State = {
    hasError: false,
    error: null,
  };

  public static getDerivedStateFromError(error: Error): State {
    return { hasError: true, error };
  }

  public componentDidCatch(error: Error, errorInfo: ErrorInfo) {
    console.error(`Uncaught error in ${this.props.scope || "component"}:`, error, errorInfo);
  }

  private handleRetry = () => {
    this.setState({ hasError: false, error: null });
  };

  public render() {
    if (this.state.hasError) {
      if (this.props.fallback) {
        return this.props.fallback;
      }
      return (
        <div className="p-4">
          <ErrorComponent
            error={{
              text: `Something went wrong in ${this.props.scope || "the UI"}`,
              details: { text: this.state.error?.message || "Unknown error" }
            }}
          />
          <button
            onClick={this.handleRetry}
            className="mt-2 px-3 py-1 text-sm rounded bg-primary/20 text-primary hover:bg-primary/30 transition-colors"
          >
            Try Again
          </button>
        </div>
      );
    }

    return this.props.children;
  }
}
