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
 * Route chunk resilience (c-perffu r2/r3).
 *
 * CRITICAL: every page must be its OWN component type. A single shared
 * <LazyRoute loader=.../> element used by all routes is reconciled as the
 * same instance across sibling routes (/hosts → /fleet, /sessions →
 * /board, /m → /m/inbox): memoizing the lazy module on [attempt] ignored
 * the changing loader, so the URL changed but the old page kept rendering,
 * and a failed route's error panel survived navigation. `createLazyRoute`
 * therefore builds ONE DISTINCT component type per page at module scope;
 * React unmounts the old page and mounts the new one on navigation.
 *
 * React.lazy caches a REJECTED import, so retry recreates the lazy
 * component per attempt (a genuine re-import) and remounts the boundary via
 * key. The boundary sits INSIDE each authed route element, so a rejected
 * chunk (offline first visit, or an old tab after a deploy deleted the
 * hashed asset) replaces only the page body — the Shell (sidebar/tabs/nav)
 * stays mounted and the operator can retry the chunk or reload the
 * document. The service worker / precaching is deliberately untouched.
 */

type AnyModule = Record<string, unknown>;

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

/**
 * Build a stable, distinct route component for one page. Call ONCE per page
 * at module scope (never inside render): the returned component is what
 * gives sibling routes different reconciliation identities.
 *
 * @param loader the dynamic import factory (called fresh on every attempt)
 * @param named  named page export; omit for a module default export
 */
export function createLazyRoute<P extends object = Record<string, never>>(
  loader: () => Promise<AnyModule>,
  named?: string,
): ComponentType<P> {
  function LazyRouteImpl(componentProps: P) {
    const [attempt, setAttempt] = useState(0);
    // Fresh lazy per attempt: this is the re-import the 重试 button triggers.
    const ResolvedComponent = useMemo(
      () =>
        lazy(async () => {
          const mod = await loader();
          const resolved = named
            ? (mod[named] as ComponentType<Record<string, unknown>>)
            : (mod.default as ComponentType<Record<string, unknown>>);
          return { default: resolved };
        }),
      // eslint-disable-next-line react-hooks/exhaustive-deps
      [attempt],
    );
    return (
      // Keyed by attempt: retry remounts the boundary with its error cleared,
      // above the newly created (re-importing) lazy element.
      <RouteErrorBoundary key={attempt} onRetry={() => setAttempt((n) => n + 1)}>
        <Suspense fallback={<RouteFallback />}>
          <ResolvedComponent {...(componentProps as Record<string, unknown>)} />
        </Suspense>
      </RouteErrorBoundary>
    );
  }
  return LazyRouteImpl as unknown as ComponentType<P>;
}
