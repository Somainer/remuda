import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LazyRoute, RouteErrorBoundary } from "./routeBoundary";

/**
 * c-perffu r2 item 4: a rejected route chunk (offline first visit, or an old
 * tab whose hashed asset a deploy deleted) must not unmount the whole app.
 * The in-shell boundary shows retry/reload; retry RE-IMPORTS (React.lazy
 * itself caches a rejected promise, so a fresh lazy is required per attempt).
 */

function GoodPage() {
  return <div data-testid="good-page">loaded</div>;
}

function renderAt(element: React.ReactNode) {
  return render(<MemoryRouter>{element}</MemoryRouter>);
}

describe("LazyRoute rejected chunk", () => {
  beforeEach(() => {
    vi.resetModules();
  });
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

    renderAt(<LazyRoute loader={loader} named="GoodPage" />);

    // First attempt rejected: error panel, Shell would still be mounted around
    // this route element.
    await screen.findByTestId("route-load-error");
    expect(loader).toHaveBeenCalledTimes(1);
    expect(screen.queryByTestId("good-page")).toBeNull();

    // Retry creates a fresh lazy and calls the import factory again.
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

    renderAt(<LazyRoute loader={loader} named="GoodPage" />);
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
    renderAt(<LazyRoute loader={loader} named="GoodPage" />);
    await screen.findByTestId("route-load-error");
    await userEvent.setup().click(screen.getByTestId("route-load-reload"));
    expect(reload).toHaveBeenCalledTimes(1);
  });
});

describe("RouteErrorBoundary", () => {
  it("renders children until a descendant render throws, then the panel", () => {
    const Boom = () => {
      throw new Error("render boom");
    };
    renderAt(
      <RouteErrorBoundary onRetry={() => {}}>
        <Boom />
      </RouteErrorBoundary>,
    );
    expect(screen.getByTestId("route-load-error")).toBeTruthy();
  });
});
