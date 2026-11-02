import { createLazyRoute } from "./routeBoundary";

export const FooRoute = createLazyRoute(
  () => import("../pages/FooPage"),
  "FooPage",
);
export const BarRoute = createLazyRoute<{ dimmed?: boolean }>(
  () => import("../pages/BarPage"),
  "BarPage",
);
export const HomeRoute = createLazyRoute(() => import("../pages/HomePage"), "HomePage");
