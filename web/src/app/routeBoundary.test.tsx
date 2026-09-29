import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes, useLocation, useNavigate } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createLazyRoute, RouteErrorBoundary } from "./routeBoundary";

/**
 * c-perffu r2/r3 item 1 + 4:
 *  - a rejected route chunk (offline first visit, stale-tab hashed asset)
 *    shows an in-shell panel and 重试 RE-IMPORTS (React.lazy caches the
 *    rejection, so a fresh lazy per attempt is required);
 *  - r3: each page is its own component type, so sibling navigation unmounts
 *    the old page (including a failed page) instead of re-rendering it.
 */

function GoodPage({ who }: { who?: string }) {
  return <div data-testid="good-page">{who ?? "loaded"}</div>;
}
function OtherPage() {
  return <div data-testid="other-page">other</div>;
}
function HostPage() {
  return <div data-testid="host-page">host</div>;
}
function HostDetailPage() {
  return <div data-testid="host-detail-page">host detail</div>;
}
const LocationProbe = () => {
  const location = useLocation();
  const navigate = useNavigate();
  return (
    <>
      <div data-testid="loc">{location.pathname}</div>
      <button data-testid="go-two" onClick={() => navigate("/two")}>
        two
      </button>
      <button data-testid="go-one" onClick={() => navigate("/one")}>
        one
      </button>
      <button data-testid="go-detail" onClick={() => navigate("/hosts/h1")}>
        detail
      </button>
      <button data-testid="go-hosts" onClick={() => navigate("/hosts")}>
        hosts
      </button>
      <button data-testid="go-ok" onClick={() => navigate("/ok")}>
        ok
      </button>
    </>
  );
};

describe("createLazyRoute rejection + retry", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("shows the in-shell error panel when the import rejects, then re-imports on 重试", async () => {
    const user = userEvent.setup();
    let attempts = 0;
    const loader = vi.fn(async () => {
      attempts += 1;
      if (attempts === 1) throw new Error("Failed to fetch dynamically imported module");
      return { GoodPage };
    });
    const Page = createLazyRoute(loader, "GoodPage");

    render(
      <MemoryRouter>
        <Page />
      </MemoryRouter>,
    );

    await screen.findByTestId("route-load-error");
    expect(loader).toHaveBeenCalledTimes(1);
    expect(screen.queryByTestId("good-page")).toBeNull();

    await user.click(screen.getByTestId("route-load-retry"));
    await waitFor(() => expect(screen.getByTestId("good-page").textContent).toBe("loaded"));
    expect(loader).toHaveBeenCalledTimes(2);
    expect(screen.queryByTestId("route-load-error")).toBeNull();
  });

  it("keeps the error panel when every import attempt keeps rejecting", async () => {
    const user = userEvent.setup();
    const loader = vi.fn(async () => {
      throw new Error("offline");
    });
    const Page = createLazyRoute(loader, "GoodPage");

    render(
      <MemoryRouter>
        <Page />
      </MemoryRouter>,
    );
    await screen.findByTestId("route-load-error");

    await user.click(screen.getByTestId("route-load-retry"));
    await screen.findByTestId("route-load-retry");
    expect(loader).toHaveBeenCalledTimes(2);
    expect(screen.getByTestId("route-load-error")).toBeTruthy();
  });

  it("offers a document reload button", async () => {
    const reload = vi.fn();
    vi.stubGlobal("location", { ...window.location, reload });
    const loader = vi.fn(async () => {
      throw new Error("stale deploy");
    });
    const Page = createLazyRoute(loader, "GoodPage");
    render(
      <MemoryRouter>
        <Page />
      </MemoryRouter>,
    );
    await screen.findByTestId("route-load-error");
    await userEvent.setup().click(screen.getByTestId("route-load-reload"));
    expect(reload).toHaveBeenCalledTimes(1);
  });
});

describe("createLazyRoute sibling navigation (r3)", () => {
  beforeEach(() => {
    vi.resetModules();
  });

  // DISTINCT component types per page — the whole point of the factory.
  const Good = createLazyRoute(async () => ({ GoodPage }), "GoodPage");
  const Other = createLazyRoute(async () => ({ OtherPage }), "OtherPage");
  const Hosts = createLazyRoute(async () => ({ HostPage }), "HostPage");
  const HostDetail = createLazyRoute(async () => ({ HostDetailPage }), "HostDetailPage");

  function RouterWith(routes: React.ReactNode, initial = "/one") {
    return (
      <MemoryRouter initialEntries={[initial]}>
        <LocationProbe />
        <Routes>
          {routes}
          <Route path="*" element={<div data-testid="unknown" />} />
        </Routes>
      </MemoryRouter>
    );
  }

  it("two successful sibling routes each render their own page", async () => {
    render(
      RouterWith(
        <>
          <Route path="/one" element={<Good />} />
          <Route path="/two" element={<Other />} />
        </>,
      ),
    );
    await screen.findByTestId("good-page");

    // Navigate like the app does (no remount of the Router element).
    await userEvent.setup().click(screen.getByTestId("go-two"));
    await screen.findByTestId("other-page");
    expect(screen.queryByTestId("good-page")).toBeNull();
    expect(screen.getByTestId("loc").textContent).toBe("/two");

    await userEvent.setup().click(screen.getByTestId("go-one"));
    await screen.findByTestId("good-page");
    expect(screen.queryByTestId("other-page")).toBeNull();
  });

  it("lists -> detail routes both render, in both directions (hosts vs host id)", async () => {
    render(
      RouterWith(
        <>
          <Route path="/hosts" element={<Hosts />} />
          <Route path="/hosts/:hostId" element={<HostDetail />} />
        </>,
        "/hosts",
      ),
    );
    await screen.findByTestId("host-page");

    await userEvent.setup().click(screen.getByTestId("go-detail"));
    await screen.findByTestId("host-detail-page");
    expect(screen.queryByTestId("host-page")).toBeNull();

    await userEvent.setup().click(screen.getByTestId("go-hosts"));
    await screen.findByTestId("host-page");
    expect(screen.queryByTestId("host-detail-page")).toBeNull();
  });

  it("a failed route is unmounted when navigation reaches a valid route", async () => {
    const Broken = createLazyRoute(async () => {
      throw new Error("offline");
    }, "GoodPage");
    render(
      RouterWith(
        <>
          <Route path="/broken" element={<Broken />} />
          <Route path="/ok" element={<Other />} />
        </>,
        "/broken",
      ),
    );
    await screen.findByTestId("route-load-error");

    await userEvent.setup().click(screen.getByTestId("go-ok"));
    await screen.findByTestId("other-page");
    expect(screen.queryByTestId("route-load-error")).toBeNull();
  });
});

describe("RouteErrorBoundary", () => {
  it("renders children until a descendant render throws, then the panel", () => {
    const Boom = () => {
      throw new Error("render boom");
    };
    render(
      <MemoryRouter>
        <RouteErrorBoundary onRetry={() => {}}>
          <Boom />
        </RouteErrorBoundary>
      </MemoryRouter>,
    );
    expect(screen.getByTestId("route-load-error")).toBeTruthy();
  });
});
