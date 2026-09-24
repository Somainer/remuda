import { describe, expect, it } from "vitest";
import { known, na } from "../types/wire";
import type { EndReasonInput } from "./endReason";
import { endReason, NODE_EPOCH_CHANGED } from "./endReason";

/**
 * c-endreason: every machine code an ended instance can carry maps to a short
 * human sentence, the raw code only survives as `detail`, and red
 * (tone "failed") is reserved for proven failures.
 */

function input(patch: Partial<EndReasonInput> = {}): EndReasonInput {
  return {
    lifecycle: "exited",
    lastError: null,
    exit: na(),
    ...patch,
  };
}

describe("endReason: live rows", () => {
  it("returns null for every non-terminal lifecycle", () => {
    for (const lifecycle of [
      "requested",
      "preparing",
      "starting",
      "ready",
      "running",
      "unknown",
      "reconciling",
    ] as const) {
      expect(endReason(input({ lifecycle, lastError: NODE_EPOCH_CHANGED }))).toBeNull();
    }
  });
});

describe("endReason: interrupted (neutral, never red)", () => {
  it("maps node-epoch-changed to the Node-restart sentence with raw detail", () => {
    const r = endReason(input({ lastError: "node-epoch-changed" }))!;
    expect(r).toEqual({
      label: "Node 重启，会话已中断",
      detail: "node-epoch-changed",
      tone: "interrupted",
    });
  });

  it("maps node-lost-instance and host-lost", () => {
    expect(endReason(input({ lastError: "node-lost-instance" }))).toMatchObject({
      label: "Node 已丢失该会话，会话已中断",
      tone: "interrupted",
    });
    expect(endReason(input({ lifecycle: "failed", lastError: "host-lost" }))).toMatchObject({
      label: "主机失联，会话已中断",
      tone: "interrupted",
    });
  });

  it("maps the herdr carrier family to one neutral sentence", () => {
    for (const code of ["herdr-carrier-lost", "carrier-missing", "carrier-shutdown", "startup-orphan"]) {
      const r = endReason(input({ lifecycle: "failed", lastError: code }))!;
      expect(r.tone).toBe("interrupted");
      expect(r.label).toBe("终端承载中断，会话已中断");
      expect(r.detail).toBe(code);
    }
  });

  it("maps signal and EOF exits as interrupted with the signal named", () => {
    const sig = endReason(input({ lastError: "native-exit-signal-SIGTERM" }))!;
    expect(sig).toEqual({
      label: "进程被终止（SIGTERM），会话已中断",
      detail: "native-exit-signal-SIGTERM",
      tone: "interrupted",
    });
    const eof = endReason(input({ lifecycle: "failed", lastError: "native-exit-eof" }))!;
    expect(eof.label).toBe("终端已关闭，会话已中断");
    expect(eof.tone).toBe("interrupted");
  });
});

describe("endReason: failed (the only red tone)", () => {
  it("maps model-mismatch in both the bare and suffixed wire spelling", () => {
    const bare = endReason(input({ lifecycle: "failed", lastError: "model-mismatch" }))!;
    expect(bare.label).toBe("模型与请求不一致，已停止");
    expect(bare.tone).toBe("failed");
    const suffixed = endReason(
      input({
        lifecycle: "failed",
        lastError: "model-mismatch: requested m/pin-a observed m/other",
      }),
    )!;
    expect(suffixed.label).toBe("模型与请求不一致，已停止");
    expect(suffixed.tone).toBe("failed");
    expect(suffixed.detail).toBe("model-mismatch: requested m/pin-a observed m/other");
  });

  it("maps the failed-launch and driver codes", () => {
    const cases: Record<string, string> = {
      "create-never-acknowledged": "会话启动未获确认",
      "driver-task-exited": "会话驱动已退出",
      "driver-task-panicked": "会话驱动崩溃",
      "native-observation-commit-failed": "会话状态记录失败",
    };
    for (const [code, label] of Object.entries(cases)) {
      const r = endReason(input({ lifecycle: "failed", lastError: code }))!;
      expect(r.label).toBe(label);
      expect(r.tone).toBe("failed");
      expect(r.detail).toBe(code);
    }
  });

  it("maps a non-zero native exit code as failed and names the code", () => {
    const r = endReason(input({ lifecycle: "failed", lastError: "native-exit-code-3" }))!;
    expect(r).toEqual({
      label: "会话异常退出（exit 3）",
      detail: "native-exit-code-3",
      tone: "failed",
    });
  });
});

describe("endReason: ordinary endings (neutral)", () => {
  it("maps explicit close, code 0, bare native-exit and operator delete", () => {
    expect(endReason(input({ lastError: "explicit-close" }))).toEqual({
      label: "已结束",
      detail: "explicit-close",
      tone: "ended",
    });
    expect(endReason(input({ lastError: "native-exit-code-0" }))).toMatchObject({
      label: "已结束",
      tone: "ended",
    });
    expect(endReason(input({ lastError: "native-exit" }))).toMatchObject({ tone: "ended" });
    expect(endReason(input({ lastError: "deleted-by-operator" }))).toEqual({
      label: "会话已删除",
      detail: "deleted-by-operator",
      tone: "ended",
    });
  });

  it("maps a clean structured exit (code 0) with no lastError", () => {
    const r = endReason(
      input({ exit: known({ code: 0, signal: null, observedAt: "2026-09-24T00:00:00Z" }) }),
    )!;
    expect(r.label).toBe("已结束");
    expect(r.tone).toBe("ended");
    expect(r.detail).toBeNull();
  });

  it("uses structured exit code/signal evidence when lastError is absent", () => {
    const crash = endReason(
      input({ exit: known({ code: 137, signal: null, observedAt: "2026-09-24T00:00:00Z" }) }),
    )!;
    expect(crash.label).toBe("会话异常退出（exit 137）");
    expect(crash.tone).toBe("failed");

    const killed = endReason(
      input({ exit: known({ code: null, signal: "SIGKILL", observedAt: "2026-09-24T00:00:00Z" }) }),
    )!;
    expect(killed.label).toBe("进程被终止（SIGKILL），会话已中断");
    expect(killed.tone).toBe("interrupted");
  });

  it("treats closing as an ended row", () => {
    expect(endReason(input({ lifecycle: "closing" }))).toEqual({
      label: "已结束",
      detail: null,
      tone: "ended",
    });
  });
});

describe("endReason: unknown codes are neutral, raw text only in detail", () => {
  it("falls back to 已结束 for an arbitrary error string, even on lifecycle failed", () => {
    const r = endReason(
      input({ lifecycle: "failed", lastError: "API Error: MHOME_EXIT_SENTINEL (429)" }),
    )!;
    expect(r.label).toBe("已结束");
    expect(r.tone).toBe("ended");
    expect(r.detail).toBe("API Error: MHOME_EXIT_SENTINEL (429)");
  });

  it("maps the bare word failed and other unknown codes as neutral", () => {
    for (const code of ["failed", "turn failed: upstream timeout", "driver exited"]) {
      const r = endReason(input({ lifecycle: "failed", lastError: code }))!;
      expect(r.tone).toBe("ended");
      expect(r.label).toBe("已结束");
      expect(r.detail).toBe(code);
    }
  });

  it("classifies on the first line and keeps the full trimmed text as detail", () => {
    const r = endReason(input({ lastError: "  model-mismatch: requested A\nmore context\n" }))!;
    expect(r.label).toBe("模型与请求不一致，已停止");
    expect(r.detail).toBe("model-mismatch: requested A\nmore context");
  });

  it("returns a neutral 已结束 with no detail for an exited row with no reason", () => {
    expect(endReason(input())).toEqual({ label: "已结束", detail: null, tone: "ended" });
  });
});
