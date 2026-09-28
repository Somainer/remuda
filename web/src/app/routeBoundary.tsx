import {
  Component,
  Suspense,
  lazy,
  useMemo,
  useState,
  type ComponentType,
  type ReactNode,
} from "react";

/**
 * Route chunk resilience (c-perffu r2).
 *
 * React.lazy memoizes a REJECTED import: resetting a boundary and rendering
 * the same lazy element throws the cached rejection again. `LazyRoute`
 * therefore creates the lazy component per load attempt: 重试 calls the
 * dynamic import a second time (re-import), and a fresh lazy element mounts.
 *
 * The boundary sits INSIDE each authed route element, so a rejected chunk
 * (offline first visit, or an old tab after a deploy deleted the hashed
 * asset) replaces only the page body — the Shell (sidebar/tabs/nav) stays
 * mounted and the operator can retry the chunk or reload the document.
 * The service worker / precaching is deliberately untouched.
 */

export class RouteErrorBoundary extends Component<
  { children: ReactNode; onRetry: () => void },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error): { error: Error } {
    return { error };
  }

  componentDidCatch(error: Error): void {
    // The rejection is surfaced in-panel; keep it visible for debugging.
    console.warn("route chunk failed:", error);
  }

  render(): ReactNode {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div data-testid="route-load-error" role="alert">
        <p>页面加载失败（可能已离线，或这是一次旧部署的标签页）。</p>
        <button
          type="button"
          data-testid="route-load-retry"
          onClick={() => this.props.onRetry()}
        >
          重试
        </button>
        <button
          type="button"
          data-testid="route-load-reload"
          onClick={() => window.location.reload()}
        >
          重新加载页面
        </button>
      </div>
    );
  }
}

function RouteFallback() {
  return (
    <p
      style={{
        margin: 0,
        minHeight: "100dvh",
        padding: "var(--space-5)",
        background: "var(--bg-canvas)",
        color: "var(--fg-muted)",
      }}
    >
      加载中…
    </p>
  );
}

type AnyModule = Record<string, unknown>;

/**
 * A lazy route that can RE-IMPORT its chunk after a rejection.
 * @param loader the dynamic import factory (called fresh on every attempt)
 * @param named  when set, the named page export becomes the component
 * @param componentProps props for the resolved page component
 */
export function LazyRoute<T = Record<string, never>>({
  loader,
  named,
  componentProps,
}: {
  loader: () => Promise<AnyModule>;
  named?: string;
  componentProps?: T;
}) {
  const [attempt, setAttempt] = useState(0);
  // A fresh lazy per attempt: this is the re-import the retry button triggers.
  const Lazy = useMemo(() => {
    return lazy(async () => {
      const mod = await loader();
      const resolved = named
        ? (mod[named] as ComponentType<Record<string, unknown>>)
        : (mod.default as ComponentType<Record<string, unknown>>);
      return { default: resolved };
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [attempt]);
  return (
    // Keyed by attempt: a retry remounts the boundary with its error cleared,
    // above the newly created (re-importing) lazy element.
    <RouteErrorBoundary key={attempt} onRetry={() => setAttempt((n) => n + 1)}>
      <Suspense fallback={<RouteFallback />}>
        <Lazy {...((componentProps ?? {}) as Record<string, unknown>)} />
      </Suspense>
    </RouteErrorBoundary>
  );
}
