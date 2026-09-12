export const HOST_TRANSPORTS = ["ssh-stdio", "outbound-wss", "local"] as const;

export type HostTransportName = (typeof HOST_TRANSPORTS)[number];
