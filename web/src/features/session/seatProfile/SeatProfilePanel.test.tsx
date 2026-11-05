import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { HubHttpError } from "../../../lib/httpError";
import * as profileApi from "./api";
import type { SeatProfile, SeatProfileView } from "./api";
import { SeatProfilePanel } from "./SeatProfilePanel";
import { bodyOf, draftOf } from "./draft";

vi.mock("./api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./api")>();
  return { ...actual, getSeatProfile: vi.fn(), putSeatProfile: vi.fn() };
});

const getSeatProfile = vi.mocked(profileApi.getSeatProfile);
const putSeatProfile = vi.mocked(profileApi.putSeatProfile);

function profile(overrides: Partial<SeatProfile> = {}): SeatProfile {
  return {
    lineageId: "ins_l",
    revision: 2,
    displayName: "Main",
    role: "keeps the loop running",
    modelCandidates: ["model-a", "model-b"],
    effort: { name: "high", ultracode: false },
    permissionMode: "manual",
    skills: [],
    createdAt: "2026-11-05T00:00:00.000Z",
    updatedAt: "2026-11-05T00:00:00.000Z",
    updatedBy: "dev_1",
    ...overrides,
  };
}

function view(overrides: Partial<SeatProfileView> = {}): SeatProfileView {
  return {
    lineageId: "ins_l",
    profile: profile(),
    liveChapter: { instanceId: "ins_c2", generation: 2, appliedRevision: 2 },
    pending: false,
    lastLaunch: null,
    ...overrides,
  };
}

function renderPanel(liveInstanceId = "ins_c2") {
  return render(
    <SeatProfilePanel lineageId="ins_l" kind="claude" liveInstanceId={liveInstanceId} currentMode="manual" />,
  );
}

afterEach(() => {
  vi.clearAllMocks();
});

describe("SeatProfilePanel", () => {
  it("shows the revision and default model, and the pending badge only while an edit waits", async () => {
    getSeatProfile.mockResolvedValueOnce(view());
    const { unmount } = renderPanel();
    expect(await screen.findByTestId("seat-profile-revision")).toHaveTextContent("修订 2");
    expect(screen.getByText(/默认模型 model-a/)).toBeInTheDocument();
    expect(screen.queryByTestId("seat-profile-pending")).toBeNull();
    unmount();

    getSeatProfile.mockResolvedValueOnce(
      view({ pending: true, liveChapter: { instanceId: "ins_c2", generation: 2, appliedRevision: 1 } }),
    );
    renderPanel();
    expect(await screen.findByTestId("seat-profile-pending")).toHaveTextContent("待下一章节生效");
  });

  it("re-reads when the live chapter changes, so the badge clears at the next chapter", async () => {
    getSeatProfile.mockResolvedValueOnce(view({ pending: true }));
    const { rerender } = renderPanel("ins_c2");
    await screen.findByTestId("seat-profile-pending");
    getSeatProfile.mockResolvedValueOnce(
      view({ pending: false, liveChapter: { instanceId: "ins_c3", generation: 3, appliedRevision: 2 } }),
    );
    rerender(<SeatProfilePanel lineageId="ins_l" kind="claude" liveInstanceId="ins_c3" currentMode="manual" />);
    await waitFor(() => expect(screen.queryByTestId("seat-profile-pending")).toBeNull());
    expect(getSeatProfile).toHaveBeenCalledTimes(2);
  });

  it("lists the last launch's unavailable items and its refusal", async () => {
    getSeatProfile.mockResolvedValueOnce(
      view({
        lastLaunch: {
          instanceId: "ins_c2",
          generation: 2,
          payload: {
            launch: "create",
            harness: "claude",
            driver: "claude-sdk",
            profileRevision: "2",
            configured: [{ class: "skill", name: "review-kit", source: "spec" }],
            resolved: [],
            skipped: [],
            unavailable: [{ class: "skill", name: "review-kit", reason: "unreadable" }],
            refusal: {
              reason: "launch-capability-unreadable",
              next: { action: "ask-owner", hint: "make it readable" },
            },
          },
        },
      }),
    );
    renderPanel();
    expect(await screen.findByTestId("seat-profile-unavailable-count")).toHaveTextContent("上次启动 1 项不可用");
    const list = screen.getByTestId("seat-profile-unavailable");
    expect(list).toHaveTextContent("技能 review-kit：无法读取");
    expect(list).toHaveTextContent("启动被拒绝：make it readable");
  });

  it("saves only the changed fields with the revision it read, and shows the badge", async () => {
    getSeatProfile.mockResolvedValueOnce(view());
    putSeatProfile.mockResolvedValueOnce({
      profile: profile({ revision: 3, effort: { name: "max", ultracode: false } }),
      pending: true,
      changed: ["effort"],
    });
    renderPanel();
    fireEvent.click(await screen.findByTestId("seat-profile-edit"));
    fireEvent.change(screen.getByTestId("seat-profile-effort"), { target: { value: "max" } });
    fireEvent.click(screen.getByTestId("seat-profile-save"));
    await waitFor(() => expect(putSeatProfile).toHaveBeenCalledTimes(1));
    expect(putSeatProfile).toHaveBeenCalledWith("ins_l", {
      expectedRevision: 2,
      effort: { name: "max", ultracode: false },
    });
    expect(await screen.findByTestId("seat-profile-pending")).toBeInTheDocument();
    expect(screen.getByTestId("seat-profile-revision")).toHaveTextContent("修订 3");
  });

  it("a stale revision shows the reload text, and reloading re-reads before another save", async () => {
    getSeatProfile.mockResolvedValueOnce(view());
    putSeatProfile.mockRejectedValueOnce(
      new HubHttpError(409, "PROFILE_REVISION_CONFLICT", "the profile changed since you read it; it is at revision 3"),
    );
    renderPanel();
    fireEvent.click(await screen.findByTestId("seat-profile-edit"));
    fireEvent.change(screen.getByTestId("seat-profile-models-input"), { target: { value: "model-c\nmodel-a" } });
    fireEvent.click(screen.getByTestId("seat-profile-save"));
    const conflict = await screen.findByTestId("seat-profile-conflict");
    expect(conflict).toHaveTextContent("配置已被其他设备修改（当前修订 3），请重新载入后再保存。");
    expect(screen.getByTestId("seat-profile-save")).toBeDisabled();

    getSeatProfile.mockResolvedValueOnce(view({ profile: profile({ revision: 3 }) }));
    fireEvent.click(screen.getByTestId("seat-profile-reload"));
    await waitFor(() => expect(screen.queryByTestId("seat-profile-conflict")).toBeNull());
    expect(screen.getByTestId("seat-profile-save")).not.toBeDisabled();
  });

  it("a first edit sends nothing it only guessed", () => {
    const initial = draftOf(null, "bypassPermissions");
    expect(bodyOf(initial, initial, 0)).toEqual({ expectedRevision: 0 });
    expect(bodyOf({ ...initial, role: "  lead  " }, initial, 0)).toEqual({ expectedRevision: 0, role: "lead" });
  });
});
