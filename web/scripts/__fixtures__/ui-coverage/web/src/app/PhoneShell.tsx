import { Outlet } from "react-router-dom";
import { PhoneNav } from "../features/shell/PhoneNav";

export function PhoneShell() {
  return (
    <div>
      <PhoneNav />
      <Outlet />
    </div>
  );
}
