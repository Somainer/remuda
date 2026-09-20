import type { Instance } from "../types/instance";

type EchoedRoute = NonNullable<Instance["apiRoute"]>;

/**
 * Hub diagnostic nativeName published when a proxied session's proxy host
 * link drops (D-047 §B.5). Adjacent to an idle API error: retrying cannot
 * clear a route whose host is gone.
 */
export const API_ROUTE_DOWN_DIAGNOSTIC = "api_route_down";

function viaHostLabel(route: EchoedRoute): string {
  const label = route.viaHostLabel?.trim();
  if (label) return label;
  const id = route.viaHostId?.trim();
  if (id) return id;
  // `apiVia: self` names the Hub host and carries no host id on the echo.
  return "Hub 主机";
}

/**
 * The API route this session is actually using, as one readable strip clause
 * (D-035): `直连`, `经 <host> 直连网络`, or `经 <host> Hub 中转`.
 *
 * Fed ONLY by the Node-echoed apiRoute — the requested route never reaches
 * here — and `null` until an echo exists, so the strip cannot report intent
 * as observation.
 */
export function apiRouteClause(route: Instance["apiRoute"] | null | undefined): string | null {
  if (!route) return null;
  if (route.mode !== "via") return "直连";
  const host = viaHostLabel(route);
  if (route.route === "direct-net") return `经 ${host} 直连网络`;
  // A via echo without a resolved kind is treated as the in-band route: the
  // Hub only projects a via echo that named one, so an absent kind here means
  // an older projection that only carried the mode.
  return `经 ${host} Hub 中转`;
}

/** The resolved route kind the echo names, for the strip's data attribute. */
export function apiRouteKind(route: Instance["apiRoute"] | null | undefined): string | null {
  if (!route || route.mode !== "via") return null;
  return route.route ?? null;
}

/** Minimal journal-event shape the scan reads; real observations fit it. */
type LifecycleLike = { kind?: unknown; payload?: unknown };

/**
 * The latest `api_route_down` Hub diagnostic in the session journal, if the
 * proxy host went away mid-flight. The route clause above stays rendered —
 * the session did not reroute; this surfaces the block as an error.
 */
export function routeDownMessage(events: readonly LifecycleLike[] | undefined): string | null {
  let message: string | null = null;
  for (const event of events ?? []) {
    if (event.kind !== "lifecycle") continue;
    const payload = (event.payload ?? {}) as {
      type?: unknown;
      nativeName?: unknown;
      message?: unknown;
    };
    if (
      payload.type === "native"
      && payload.nativeName === API_ROUTE_DOWN_DIAGNOSTIC
      && typeof payload.message === "string"
      && payload.message.trim()
    ) {
      message = payload.message;
    }
  }
  return message;
}
