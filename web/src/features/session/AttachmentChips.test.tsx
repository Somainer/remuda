import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { AttachmentChips, SentAttachments, type SentAttachment } from "./AttachmentChips";
import type { Attachment } from "../../lib/attachments";

function chip(overrides: Partial<Attachment> = {}): Attachment {
  return {
    localId: "att_1",
    objectId: "obj_1",
    name: "file.bin",
    kind: "file",
    mediaType: "application/octet-stream",
    size: 1024,
    previewUrl: "blob:x",
    state: "ready",
    ...overrides,
  };
}

describe("AttachmentChips", () => {
  it("renders a non-image file with a type glyph and a human size, no thumbnail", () => {
    const { container } = render(
      <AttachmentChips
        attachments={[
          chip({ name: "Q3 report.pdf", mediaType: "application/pdf", size: 24 * 1024 * 1024 }),
        ]}
        onRemove={vi.fn()}
      />,
    );
    expect(screen.getByText("Q3 report.pdf")).toBeTruthy();
    expect(screen.getByText("24.0 MB")).toBeTruthy();
    expect(container.querySelector("img")).toBeNull();
  });

  it("renders an image with a thumbnail", () => {
    const { container } = render(
      <AttachmentChips
        attachments={[
          chip({
            localId: "att_img",
            name: "shot.png",
            kind: "image",
            mediaType: "image/png",
          }),
        ]}
        onRemove={vi.fn()}
      />,
    );
    const img = container.querySelector("img");
    expect(img).not.toBeNull();
    expect(img?.getAttribute("src")).toBe("blob:x");
  });

  it("marks an unreferenced chip and keeps the remove control wired", () => {
    const onRemove = vi.fn();
    render(
      <AttachmentChips
        attachments={[chip()]}
        unreferenced={new Set(["att_1"])}
        onRemove={onRemove}
      />,
    );
    expect(screen.getByTestId("attachment-chip").getAttribute("data-unreferenced")).toBe("1");
    screen.getByTestId("attachment-remove").click();
    expect(onRemove).toHaveBeenCalledWith("att_1");
  });
});

describe("SentAttachments", () => {
  it("links a sent file to the Hub object with a download attribute", () => {
    const files: SentAttachment[] = [
      {
        objectId: "obj_pdf",
        name: "report.pdf",
        previewUrl: "blob:x",
        kind: "file",
        mediaType: "application/pdf",
        size: 50_331_648,
        index: 1,
      },
    ];
    render(<SentAttachments attachments={files} />);
    const link = screen.getByTestId("sent-file") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("/v1/objects/obj_pdf");
    expect(link.getAttribute("download")).toBe("report.pdf");
    expect(link.textContent).toContain("report.pdf");
    expect(link.textContent).toContain("48.0 MB");
  });

  it("renders a sent image as a thumbnail link", () => {
    const { container } = render(
      <SentAttachments
        attachments={[
          {
            objectId: "obj_png",
            name: "shot.png",
            previewUrl: "blob:thumb",
            kind: "image",
            mediaType: "image/png",
            size: 12,
            index: 2,
          },
        ]}
      />,
    );
    const img = container.querySelector("img");
    expect(img?.getAttribute("src")).toBe("blob:thumb");
    expect(screen.queryAllByTestId("sent-file")).toHaveLength(0);
    expect(screen.getByTestId("sent-attachment-index").textContent).toBe("2");
  });
});
