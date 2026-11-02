import { Outlet } from "react-router-dom";
import { ChromeThing } from "../features/shell/ChromeThing";
import { FooRoute } from "./lazyRoutes";

export function Shell() {
  return (
    <div>
      <ChromeThing />
      <FooRoute dimmed />
      <Outlet />
    </div>
  );
}
