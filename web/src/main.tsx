import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { Root } from "./app/Root";
import { applyTheme, readTheme } from "./features/settings/theme";
import "@fontsource/ibm-plex-mono/latin-400.css";
import "@fontsource/ibm-plex-mono/latin-500.css";
import "@fontsource/ibm-plex-sans/latin-400.css";
import "@fontsource/ibm-plex-sans/latin-500.css";
import "@fontsource/ibm-plex-sans/latin-600.css";
import "./styles/tokens.css";

// Apply the stored theme once, before React mounts: the choice then holds on
// the first frame of every route, without having to visit the settings page.
applyTheme(readTheme());

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Root />
  </StrictMode>,
);
