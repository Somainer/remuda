import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { Root } from "./app/Root";
import { applyAppearance, readAppearance } from "./features/settings/appearance";
import "@fontsource/ibm-plex-mono/latin-400.css";
import "@fontsource/ibm-plex-mono/latin-500.css";
import "./styles/tokens.css";
import "./styles/keyboardCompact.css";

// Apply the stored appearance once, before React mounts. "system" needs no
// attribute — tokens.css resolves it in CSS, so nothing here blocks the first
// frame; an explicit choice is stamped before any route renders.
applyAppearance(readAppearance());

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Root />
  </StrictMode>,
);
