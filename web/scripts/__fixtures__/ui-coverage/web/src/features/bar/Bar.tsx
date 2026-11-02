import { lazy } from "react";

const Lazy = lazy(() => import("./Lazy"));

export function Bar() {
  return <Lazy />;
}
