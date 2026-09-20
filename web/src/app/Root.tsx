import { useEffect } from "react";
import { BrowserRouter } from "react-router-dom";
import { hubStore } from "../lib/store";
import { startAppBadgeSync } from "../lib/push";
import { startPWA } from "../lib/pwa";
import { AppRouter } from "./router";

export function Root() {
  useEffect(() => {
    startPWA();
    void hubStore.bootstrap();
    // D-049 ui-spec §4.5: while a page is open the OS app badge follows the
    // pending interaction count (set/clear), no-op where the API is absent.
    return startAppBadgeSync();
  }, []);
  return (
    <BrowserRouter>
      <AppRouter />
    </BrowserRouter>
  );
}
