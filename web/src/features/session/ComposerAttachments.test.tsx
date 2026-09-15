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
  // Echo the uploaded blob's size/type/name so the chip settles with the same
  // metadata the real Hub would resolve.
  upload.mockImplementation(
    (
      _instanceId: string,
      blob: Blob,
      mediaType: string,
      fileName?: string,
    ) =>
      Promise.resolve({
        objectId: "obj_pasted",
        size: blob.size,
        kind: mediaType.startsWith("image/") ? "image" : "file",
        name: fileName ?? null,
        mediaType,
      }),
  );
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
    const { container } = render(
      <Composer instanceId="ins_send" mobile={false} onSend={onSend} />,
    );
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "what colour is this?" } });
    input.setSelectionRange(20, 20);
    fireEvent.paste(input, pasteEvent([png()]));
    await waitFor(() => expect(upload).toHaveBeenCalled());
    await waitFor(() =>
      expect(screen.getByTestId("composer-send").hasAttribute("disabled")).toBe(false),
    );
    expect(input.value).toBe("what colour is this? [Image #1]");
    expect(container.querySelector("[data-index='1']")).toBeTruthy();

    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(onSend).toHaveBeenCalled());
    const [text, refs] = onSend.mock.calls[0];
    expect(text).toBe("what colour is this? [Image #1]");
    expect(refs).toEqual([
      {
        index: 1,
        objectId: "obj_pasted",
        kind: "image",
        mediaType: "image/png",
        name: "shot.png",
        size: 4,
      },
    ]);
  });

  it("inserts tokens 1 and 2, and removing the first chip renumbers the rest", async () => {
    render(<Composer instanceId="ins_renumber" mobile={false} onSend={vi.fn()} />);
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    fireEvent.paste(input, pasteEvent([png("a.png"), png("b.png")]));

    const chips = await screen.findAllByTestId("attachment-chip");
    expect(chips).toHaveLength(2);
    expect(input.value).toBe("[Image #1] [Image #2]");
    await waitFor(() => expect(upload).toHaveBeenCalledTimes(2));

    fireEvent.click(screen.getAllByTestId("attachment-remove")[0]);
    await waitFor(() =>
      expect(screen.queryAllByTestId("attachment-chip")).toHaveLength(1),
    );
    expect(input.value).toBe("[Image #1]");
    expect(screen.getByTestId("attachment-index").textContent).toBe("1");
  });

  it("marks a chip 未引用 when its token is edited out, but still sends it", async () => {
    const onSend = vi.fn();
    render(<Composer instanceId="ins_orphan" mobile={false} onSend={onSend} />);
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    fireEvent.paste(input, pasteEvent([png()]));
    await waitFor(() => expect(upload).toHaveBeenCalled());
    expect(input.value).toBe("[Image #1]");

    // User deletes the token but keeps the chip staged.
    fireEvent.change(input, { target: { value: "no token here" } });
    const chip = await screen.findByTestId("attachment-chip");
    expect(chip.getAttribute("data-unreferenced")).toBe("1");
    expect(screen.getByText("未引用（仍会发送）")).toBeTruthy();

    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(onSend).toHaveBeenCalled());
    const [, refs] = onSend.mock.calls[0];
    expect(refs).toHaveLength(1);
    expect(refs[0]).toMatchObject({ index: 1, objectId: "obj_pasted" });
  });

  // A half-uploaded reference would not resolve on the Hub.
  it("blocks sending while an upload is still in flight", async () => {
    let release: (value: {
      objectId: string;
      size: number;
      kind: "image" | "file";
      name: string | null;
      mediaType: string;
    }) => void = () => {};
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
    release({
      objectId: "obj_late",
      size: 4,
      kind: "image",
      name: "shot.png",
      mediaType: "image/png",
    });
    await waitFor(() =>
      expect(screen.getByTestId("composer-send").hasAttribute("disabled")).toBe(false),
    );
  });

  it("keeps a failed upload on screen so it can be retried", async () => {
    upload.mockRejectedValueOnce(new Error("文件太大（上限 25.0 MB）"));
    render(<Composer instanceId="ins_fail" mobile={false} onSend={vi.fn()} />);
    fireEvent.paste(screen.getByTestId("composer-input"), pasteEvent([png()]));

    const chip = await screen.findByTestId("attachment-chip");
    await waitFor(() => expect(chip.getAttribute("data-state")).toBe("failed"));
    expect(screen.getByText("文件太大（上限 25.0 MB）")).toBeTruthy();

    upload.mockResolvedValue({
      objectId: "obj_retried",
      size: 4,
      kind: "image",
      name: "shot.png",
      mediaType: "image/png",
    });
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
    // D-027b: labels say attachment, not image.
    expect(screen.getByTestId("attach-file").getAttribute("aria-label")).toBe("添加附件");
    expect(screen.getByTestId("attach-paste").textContent).toBe("粘贴附件");
  });

  // D-027b: a non-image paste stages a file chip, inserts [File #1] (not an
  // image token), and the manifest carries kind=file.
  it("stages a pasted PDF as a file chip with a [File #1] token", async () => {
    const onSend = vi.fn();
    const { container } = render(
      <Composer instanceId="ins_file" mobile={false} onSend={onSend} />,
    );
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    const pdf = new File([new Uint8Array(1200)], "Q3 report.pdf", {
      type: "application/pdf",
    });
    fireEvent.paste(input, pasteEvent([pdf], ["application/pdf"]));

    const chip = await screen.findByTestId("attachment-chip");
    expect(chip.getAttribute("data-kind")).toBe("file");
    expect(screen.getByText("Q3 report.pdf")).toBeTruthy();
    expect(input.value).toBe("[File #1]");
    expect(container.querySelector("img")).toBeNull();

    await waitFor(() =>
      expect(screen.getByTestId("composer-send").hasAttribute("disabled")).toBe(false),
    );
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(onSend).toHaveBeenCalled());
    const [text, refs] = onSend.mock.calls[0];
    expect(text).toBe("[File #1]");
    expect(refs).toEqual([
      {
        index: 1,
        objectId: "obj_pasted",
        kind: "file",
        mediaType: "application/pdf",
        name: "Q3 report.pdf",
        size: 1200,
      },
    ]);
  });
});
