/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_MOCK?: string;
  readonly VITE_API_BASE?: string;
  readonly VITE_HUB_URL?: string;
  readonly VITE_DEV_TTY?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
