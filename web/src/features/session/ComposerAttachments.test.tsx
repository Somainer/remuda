import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";

/**
 * D-027 composer behaviour: pasting an image stages it, and sending hands the
 * staged reference on rather than the bytes.
 */

const upload = vi.fn();
vi.mock("../../lib/api", () => ({
  api: {
    objectUpload: (...args: unknown[]) => upload(...args),
  },
}));

// jsdom has no canvas, so normalizeImage's re-encode cannot run here. The
// encode itself is exercised in the browser; this asserts the wiring around it.
vi.mock("../../lib/attachments", async () => {
  const actual = await vi.importActual<typeof import("../../lib/attachments")>(
    "../../lib/attachments",
  );
  return {
    ...actual,
    normalizeImage: async (file: File) => ({ blob: file, mediaType: "image/png" }),
  };
});

function png(name = "shot.png"): File {
  return new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], name, { type: "image/png" });
}

function pasteEvent(files: File[], types = ["image/png"]) {
  return {
    clipboardData: {
      items: files.map((file) => ({
        kind: "file",
        type: file.type,
        getAsFile: () => file,
      })),
      files,
      types,
    },
  };
}

beforeEach(() => {
  upload.mockReset();
  upload.mockResolvedValue({ objectId: "obj_pasted", size: 4 });
  globalThis.URL.createObjectURL = vi.fn(() => "blob:preview");
  globalThis.URL.revokeObjectURL = vi.fn();
});

describe("composer attachments", () => {
  it("stages a pasted image as a chip and uploads it", async () => {
    render(<Composer instanceId="ins_paste" mobile={false} onSend={vi.fn()} />);
    fireEvent.paste(screen.getByTestId("composer-input"), pasteEvent([png()]));

    expect(await screen.findByTestId("attachment-chip")).toBeTruthy();
    await waitFor(() => expect(upload).toHaveBeenCalledTimes(1));
    expect(upload.mock.calls[0][0]).toBe("ins_paste");
    expect(upload.mock.calls[0][2]).toBe("image/png");
  });

  it("sends the staged reference, never the bytes", async () => {
    const onSend = vi.fn();
    render(<Composer instanceId="ins_send" mobile={false} onSend={onSend} />);
    const input = screen.getByTestId("composer-input");
    fireEvent.change(input, { target: { value: "what colour is this?" } });
    fireEvent.paste(input, pasteEvent([png()]));
    await waitFor(() => expect(upload).toHaveBeenCalled());
    await waitFor(() =>
      expect(screen.getByTestId("composer-send").hasAttribute("disabled")).toBe(false),
    );

    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(onSend).toHaveBeenCalled());
    const [text, refs] = onSend.mock.calls[0];
    expect(text).toBe("what colour is this?");
    expect(refs).toEqual([
      { objectId: "obj_pasted", mediaType: "image/png", name: "shot.png", size: 4 },
    ]);
  });

  // A half-uploaded reference would not resolve on the Hub.
  it("blocks sending while an upload is still in flight", async () => {
    let release: (value: { objectId: string; size: number }) => void = () => {};
    upload.mockImplementation(
      () => new Promise<{ objectId: string; size: number }>((resolve) => (release = resolve)),
    );
    render(<Composer instanceId="ins_wait" mobile={false} onSend={vi.fn()} />);
    const input = screen.getByTestId("composer-input");
    fireEvent.change(input, { target: { value: "hi" } });
    fireEvent.paste(input, pasteEvent([png()]));

    await waitFor(() =>
      expect(screen.getByTestId("composer-send").hasAttribute("disabled")).toBe(true),
    );
    release({ objectId: "obj_late", size: 4 });
    await waitFor(() =>
      expect(screen.getByTestId("composer-send").hasAttribute("disabled")).toBe(false),
    );
  });

  it("keeps a failed upload on screen so it can be retried", async () => {
    upload.mockRejectedValueOnce(new Error("图片太大（上限 5 MB）"));
    render(<Composer instanceId="ins_fail" mobile={false} onSend={vi.fn()} />);
    fireEvent.paste(screen.getByTestId("composer-input"), pasteEvent([png()]));

    const chip = await screen.findByTestId("attachment-chip");
    await waitFor(() => expect(chip.getAttribute("data-state")).toBe("failed"));
    expect(screen.getByText("图片太大（上限 5 MB）")).toBeTruthy();

    upload.mockResolvedValue({ objectId: "obj_retried", size: 4 });
    fireEvent.click(screen.getByTestId("attachment-retry"));
    await waitFor(() => expect(chip.getAttribute("data-state")).toBe("ready"));
  });

  it("removes a chip on request", async () => {
    render(<Composer instanceId="ins_remove" mobile={false} onSend={vi.fn()} />);
    fireEvent.paste(screen.getByTestId("composer-input"), pasteEvent([png()]));
    await screen.findByTestId("attachment-chip");

    fireEvent.click(screen.getByTestId("attachment-remove"));
    await waitFor(() => expect(screen.queryByTestId("attachment-chip")).toBeNull());
    expect(globalThis.URL.revokeObjectURL).toHaveBeenCalledWith("blob:preview");
  });

  // iOS can declare an image and expose no file; the user needs to be told.
  it("guides the user when a paste declares an image it cannot read", async () => {
    render(<Composer instanceId="ins_ios" mobile onSend={vi.fn()} />);
    fireEvent.paste(screen.getByTestId("composer-input"), pasteEvent([], ["image/png"]));

    expect(await screen.findByTestId("attachment-notice")).toBeTruthy();
    expect(screen.queryByTestId("attachment-chip")).toBeNull();
    expect(upload).not.toHaveBeenCalled();
  });

  it("does not swallow a plain text paste", async () => {
    render(<Composer instanceId="ins_text" mobile={false} onSend={vi.fn()} />);
    const event = pasteEvent([], ["text/plain"]);
    fireEvent.paste(screen.getByTestId("composer-input"), event);

    expect(screen.queryByTestId("attachment-chip")).toBeNull();
    expect(screen.queryByTestId("attachment-notice")).toBeNull();
    expect(upload).not.toHaveBeenCalled();
  });

  it("offers a file picker and an explicit paste button", () => {
    render(<Composer instanceId="ins_buttons" mobile onSend={vi.fn()} />);
    expect(screen.getByTestId("attach-file")).toBeTruthy();
    expect(screen.getByTestId("attach-camera")).toBeTruthy();
    expect(screen.getByTestId("attach-paste")).toBeTruthy();
  });
});
