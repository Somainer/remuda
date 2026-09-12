import { useEffect } from "react";
import { BrowserRouter } from "react-router-dom";
import { hubStore } from "../lib/store";
import { startPWA } from "../lib/pwa";
import { AppRouter } from "./router";

export function Root() {
  useEffect(() => {
    startPWA();
    void hubStore.bootstrap();
  }, []);
  return (
    <BrowserRouter>
      <AppRouter />
    </BrowserRouter>
  );
}
