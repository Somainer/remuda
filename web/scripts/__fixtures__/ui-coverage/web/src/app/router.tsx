import { Navigate, Outlet, Route, Routes } from "react-router-dom";
import { AuthGate } from "./AuthGate";
import { Shell } from "./Shell";
import { PhoneShell } from "./PhoneShell";
import { BarRoute, FooRoute, HomeRoute } from "./lazyRoutes";
import { LoginPage } from "../pages/LoginPage";
import { DetailPage } from "../pages/DetailPage";

function Gate() {
  return <Outlet />;
}

function RootLanding() {
  return <Navigate to="/foo" replace />;
}

export function AppRouter() {
  return (
    <Routes>
      <Route path="/login" element={<LoginPage />} />
      <Route path="/" element={<RootLanding />} />
      <Route element={<AuthGate />}>
        <Route element={<Shell />}>
          <Route path="/foo" element={<FooRoute />} />
          <Route path="/bar" element={<BarRoute />} />
          <Route path="/detail/:id" element={<DetailPage />} />
          <Route path="/s/:id" element={<BarRoute />} />
          <Route path="/s/:id/files" element={<BarRoute />} />
          <Route element={<Gate />}>
            <Route path="/gated" element={<BarRoute />} />
          </Route>
        </Route>
        <Route path="/m" element={<PhoneShell />}>
          <Route index element={<HomeRoute />} />
          <Route path="inbox" element={<Navigate to="/m" replace />} />
        </Route>
      </Route>
      <Route path="*" element={<Navigate to="/foo" replace />} />
    </Routes>
  );
}
