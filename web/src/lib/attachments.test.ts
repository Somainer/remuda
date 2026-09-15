import { describe, expect, it } from "vitest";
import {
  clipboardHasUnreadableImage,
  imagesFromClipboard,
  isImageFile,
  refsOf,
  type Attachment,
} from "./attachments";

/** Minimal DataTransfer stand-in: jsdom does not implement a usable one. */
function clipboard(options: {
  items?: { kind: string; type: string; file: File | null }[];
  files?: File[];
  types?: string[];
}): DataTransfer {
  const items = (options.items ?? []).map((item) => ({
    kind: item.kind,
    type: item.type,
    getAsFile: () => item.file,
  }));
  return {
    items,
    files: options.files ?? [],
    types: options.types ?? [],
  } as unknown as DataTransfer;
}

function png(name = "shot.png"): File {
  return new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], name, { type: "image/png" });
}

describe("imagesFromClipboard", () => {
  it("reads an image out of clipboard items", () => {
    const file = png();
    const found = imagesFromClipboard(
      clipboard({ items: [{ kind: "file", type: "image/png", file }], types: ["image/png"] }),
    );
    expect(found).toHaveLength(1);
    expect(found[0].name).toBe("shot.png");
  });

  // Safari can populate `files` while `items` is unhelpful, so both are read.
  it("falls back to the files list when items are empty", () => {
    const found = imagesFromClipboard(clipboard({ files: [png()], types: ["image/png"] }));
    expect(found).toHaveLength(1);
  });

  // A single paste routinely appears in both collections.
  it("does not add the same image twice when it appears in both collections", () => {
    const file = png();
    const found = imagesFromClipboard(
      clipboard({
        items: [{ kind: "file", type: "image/png", file }],
        files: [file],
        types: ["image/png"],
      }),
    );
    expect(found).toHaveLength(1);
  });

  it("ignores pasted text", () => {
    const found = imagesFromClipboard(
      clipboard({
        items: [{ kind: "string", type: "text/plain", file: null }],
        types: ["text/plain"],
      }),
    );
    expect(found).toHaveLength(0);
  });

  it("tolerates a paste with no clipboard data at all", () => {
    expect(imagesFromClipboard(null)).toHaveLength(0);
  });
});

describe("clipboardHasUnreadableImage", () => {
  // The iOS case the explicit paste button exists for: an image is declared
  // but no file is exposed.
  it("detects a declared image with no readable file", () => {
    expect(clipboardHasUnreadableImage(clipboard({ types: ["image/png"] }))).toBe(true);
  });

  it("is false when the image is readable", () => {
    expect(
      clipboardHasUnreadableImage(clipboard({ files: [png()], types: ["image/png"] })),
    ).toBe(false);
  });

  it("is false for a plain text paste", () => {
    expect(clipboardHasUnreadableImage(clipboard({ types: ["text/plain"] }))).toBe(false);
  });
});

describe("isImageFile", () => {
  it("accepts images and rejects everything else", () => {
    expect(isImageFile(png())).toBe(true);
    expect(isImageFile(new File(["x"], "a.txt", { type: "text/plain" }))).toBe(false);
    expect(isImageFile(null)).toBe(false);
  });
});

describe("refsOf", () => {
  const base: Attachment = {
    localId: "att_1",
    name: "a.png",
    kind: "image",
    mediaType: "image/png",
    size: 10,
    previewUrl: "blob:a",
    state: "ready",
    objectId: "obj_1",
  };

  it("returns only staged attachments, in chip order", () => {
    const refs = refsOf([
      base,
      { ...base, localId: "att_2", objectId: "obj_2", name: "b.png" },
      { ...base, localId: "att_3", state: "uploading", objectId: undefined },
      { ...base, localId: "att_4", state: "failed", objectId: undefined },
    ]);
    expect(refs.map((ref) => ref.objectId)).toEqual(["obj_1", "obj_2"]);
    expect(refs[0]).toEqual({
      index: 1,
      objectId: "obj_1",
      kind: "image",
      mediaType: "image/png",
      name: "a.png",
      size: 10,
    });
  });

  it("is empty when nothing has been staged", () => {
    expect(refsOf([])).toEqual([]);
    expect(refsOf([{ ...base, state: "uploading", objectId: undefined }])).toEqual([]);
  });

  it("orders the manifest by first token appearance when given the prompt text", () => {
    const chips = [
      base,
      { ...base, localId: "att_2", objectId: "obj_2", name: "b.png" },
      { ...base, localId: "att_3", objectId: "obj_3", name: "c.png" },
    ];
    // Prompt references #2 before #1; #3's token was edited away so #3 is
    // 未引用 but still sent — it trails the referenced ones.
    const refs = refsOf(chips, "start [Image #2] then [Image #1] end");
    expect(refs.map((ref) => [ref.objectId, ref.index])).toEqual([
      ["obj_2", 2],
      ["obj_1", 1],
      ["obj_3", 3],
    ]);
  });

  it("keeps unreferenced-but-sent attachments last, still carrying their index", () => {
    const chips = [
      base,
      { ...base, localId: "att_2", objectId: "obj_2", name: "b.png" },
    ];
    const refs = refsOf(chips, "only the second one: [Image #2]");
    expect(refs.map((ref) => [ref.objectId, ref.index])).toEqual([
      ["obj_2", 2],
      ["obj_1", 1],
    ]);
  });

  it("orders mixed [Image #n] and [File #n] tokens by appearance", () => {
    const file: Attachment = {
      ...base,
      localId: "att_pdf",
      objectId: "obj_pdf",
      name: "report.pdf",
      kind: "file",
      mediaType: "application/pdf",
    };
    const image: Attachment = {
      ...base,
      localId: "att_png",
      objectId: "obj_png",
      name: "shot.png",
      kind: "image",
    };
    // Chip positions: file=#1, image=#2. Prompt quotes #2 before #1.
    const refs = refsOf([file, image], "see [Image #2] and read [File #1]");
    expect(refs.map((ref) => [ref.objectId, ref.kind, ref.index])).toEqual([
      ["obj_png", "image", 2],
      ["obj_pdf", "file", 1],
    ]);
  });
});
