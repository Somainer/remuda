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
      objectId: "obj_1",
      mediaType: "image/png",
      name: "a.png",
      size: 10,
    });
  });

  it("is empty when nothing has been staged", () => {
    expect(refsOf([])).toEqual([]);
    expect(refsOf([{ ...base, state: "uploading", objectId: undefined }])).toEqual([]);
  });
});
