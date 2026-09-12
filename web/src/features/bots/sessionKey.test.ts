import { describe, expect, it } from "vitest";
import { BOT_CHANNELS } from "./channels";
import { FEISHU_SESSION_RULE, feishuSessionKey, telegramSessionKey } from "./sessionKey";

describe("bot session keys", () => {
  it("prefers thread_id then root_id then main", () => {
    expect(feishuSessionKey("oc_chat", "omt_t", "om_root")).toBe("feishu:oc_chat:omt_t");
    expect(feishuSessionKey("oc_chat", null, "om_root")).toBe("feishu:oc_chat:om_root");
    expect(feishuSessionKey("oc_chat")).toBe("feishu:oc_chat:main");
    expect(FEISHU_SESSION_RULE).toBe("feishu:{chat_id}:{thread_id||root_id||main}");
  });

  it("uses telegram thread 0 when absent", () => {
    expect(telegramSessionKey("99")).toBe("tg:99:0");
    expect(telegramSessionKey("99", 7)).toBe("tg:99:7");
  });

  it("binds Feishu allowlists and mention-only group policy", () => {
    const feishu = BOT_CHANNELS.find((c) => c.channelId === "feishu")!;
    expect(feishu.ownerOpenIds[0]).toMatch(/^ou_/);
    expect(feishu.chatAllowlist[0]).toMatch(/^oc_/);
    expect(feishu.groupPolicy).toBe("mention-only");
    expect(feishu.ticketTtlMin).toBe(12);
    expect(feishu.defaultKind).toBe("claude-print");
    expect(feishu.deliveries.length).toBeGreaterThan(0);
  });
});
