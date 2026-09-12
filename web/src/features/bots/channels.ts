import { FEISHU_SESSION_RULE, TELEGRAM_SESSION_RULE } from "./sessionKey";

export type GroupPolicy = "mention-only" | "allow-unaddressed";

export type BotDelivery = {
  at: string;
  actor: string;
  sessionKey: string;
  instanceId: string;
  state: "queued" | "accepted" | "settled";
};

export type BotChannel = {
  channelId: string;
  label: string;
  kind: "feishu" | "telegram";
  profile: string;
  transport: "long-poll" | "polling";
  online: boolean;
  sessionKey: string;
  ownerOpenIds: string[];
  chatAllowlist: string[];
  defaultHost: string;
  defaultProject: string;
  defaultKind: string;
  sessionTtl: string;
  ticketTtlMin: number;
  groupPolicy: GroupPolicy;
  deliveries: BotDelivery[];
};

export const BOT_CHANNELS: BotChannel[] = [
  {
    channelId: "feishu",
    label: "飞书",
    kind: "feishu",
    profile: "lark-cli oncall-helper",
    transport: "long-poll",
    online: true,
    sessionKey: FEISHU_SESSION_RULE,
    ownerOpenIds: ["ou_owner_aaaaaaaaaaaaaaaaaaaaaaaaaa"],
    chatAllowlist: ["oc_allow_bbbbbbbbbbbbbbbbbbbbbbbbbb"],
    defaultHost: "devbox-sg",
    defaultProject: "sfe-root",
    defaultKind: "claude-print",
    sessionTtl: "4h",
    ticketTtlMin: 12,
    groupPolicy: "mention-only",
    deliveries: [
      {
        at: "12:04",
        actor: "甘露寺",
        sessionKey: "feishu:oc_allow_bbbbbbbbbbbbbbbbbbbbbbbbbb:main",
        instanceId: "ins_01993ab0-0000-7000-8000-000000000002",
        state: "accepted",
      },
      {
        at: "11:51",
        actor: "甘露寺",
        sessionKey: "feishu:oc_allow_bbbbbbbbbbbbbbbbbbbbbbbbbb:omt_topic",
        instanceId: "ins_01993ab0-0000-7000-8000-000000000001",
        state: "settled",
      },
    ],
  },
  {
    channelId: "telegram",
    label: "Telegram",
    kind: "telegram",
    profile: "@runtime",
    transport: "polling",
    online: true,
    sessionKey: TELEGRAM_SESSION_RULE,
    ownerOpenIds: [],
    chatAllowlist: ["123456789"],
    defaultHost: "devbox-sg",
    defaultProject: "sfe-root",
    defaultKind: "claude-print",
    sessionTtl: "4h",
    ticketTtlMin: 12,
    groupPolicy: "mention-only",
    deliveries: [],
  },
];
