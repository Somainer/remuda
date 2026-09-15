/**
 * D-027 / D-027b: turn a pasted, dropped, or picked file into a staged Hub
 * object.
 *
 * Images are re-encoded through a canvas before they leave the browser. That
 * is not only for size: re-encoding drops EXIF — including GPS — and the ICC
 * profile, and it converts formats (notably iOS HEIC) that the Hub's image
 * allowlist would otherwise reject. Doing it here means the Hub never decodes
 * an image and so never links an image library.
 *
 * Non-image files (D-027b, 2026-09-15) pass through untouched: the bytes are
 * uploaded as-is, never executed, and delivered to the agent as a path
 * reference rather than an inline block.
 */

import {
  findAnchorsFor,
  type AnchorKind,
} from "./imageAnchors";

/** Media types the Hub sniffs and accepts as native images. */
export const ACCEPTED_IMAGE_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"] as const;

/** Hub default per-attachment ceiling (D-027b: 25 MiB). */
export const MAX_ATTACHMENT_BYTES = 25 * 1024 * 1024;
/** Hub's per-message ceiling (D-027b: 8 attachments). */
export const MAX_ATTACHMENTS = 8;

/** Attachment kind; images keep native delivery, files get a path reference. */
export type AttachmentKind = "image" | "file";

/** A local file being prepared or already staged on the Hub. */
export type Attachment = {
  /** Client-side identity, stable across the upload. */
  localId: string;
  /** Hub object id once staged. */
  objectId?: string;
  /** Original filename (the Hub sanitises it again). */
  name: string;
  /** Image vs. file — decides the chip, the token family and delivery. */
  kind: AttachmentKind;
  mediaType: string;
  size: number;
  /**
   * `blob:` URL for the chip. For an image it backs the thumbnail; for a file
   * it is unused as an image but kept for lifecycle symmetry. Revoked when
   * the chip goes away.
   */
  previewUrl: string;
  state: "uploading" | "ready" | "failed";
  /** Message shown on a failed chip, which stays retryable. */
  error?: string;
};

let counter = 0;
function localId(): string {
  counter += 1;
  return `att_${Date.now().toString(36)}_${counter}`;
}

/** True when the browser handed us something it classifies as an image. */
export function isImageFile(file: File | null | undefined): file is File {
  return Boolean(file && file.type.startsWith("image/"));
}

/** Classify a picked/dropped/pasted file for chip + token purposes. */
export function kindOfFile(file: File): AttachmentKind {
  return isImageFile(file) ? "image" : "file";
}

/** Token family for a chip. */
export function anchorKindOf(attachment: Attachment): AnchorKind {
  return attachment.kind === "image" ? "Image" : "File";
}

/**
 * Pull files of any kind out of a paste.
 *
 * Reads both `items` and `files`: iOS can report a file in `types` while
 * `files` is empty, and the two collections do not always agree. Duplicates
 * are collapsed by name and size, since a single paste often appears in both.
 */
export function filesFromClipboard(data: DataTransfer | null): File[] {
  if (!data) return [];
  const found: File[] = [];
  const seen = new Set<string>();
  const push = (file: File | null) => {
    if (!file) return;
    const key = `${file.name}:${file.size}:${file.type}`;
    if (seen.has(key)) return;
    seen.add(key);
    found.push(file);
  };
  for (const item of Array.from(data.items ?? [])) {
    if (item.kind === "file") push(item.getAsFile());
  }
  for (const file of Array.from(data.files ?? [])) push(file);
  return found;
}

/** Back-compat shim for the old image-only name. */
export const imagesFromClipboard = (data: DataTransfer | null): File[] =>
  filesFromClipboard(data).filter(isImageFile);

/** True when a paste carries an image but exposed no readable file (iOS). */
export function clipboardHasUnreadableImage(data: DataTransfer | null): boolean {
  if (!data) return false;
  const declares = Array.from(data.types ?? []).some((type) => type.startsWith("image/"));
  return declares && filesFromClipboard(data).filter(isImageFile).length === 0;
}

/** Longest edge after downscaling. Matches what vision models actually use. */
const MAX_EDGE = 1568;

/** Normalized bytes plus the type the Hub will see. */
export type NormalizedImage = { blob: Blob; mediaType: string };

/**
 * Downscale and re-encode an image, stripping EXIF and ICC along the way.
 *
 * GIFs are passed through untouched: re-encoding one through a canvas would
 * silently flatten it to a single frame, which is worse than leaving it alone,
 * and a GIF carries no EXIF to strip.
 */
export async function normalizeImage(file: File): Promise<NormalizedImage> {
  if (file.type === "image/gif") {
    return { blob: file, mediaType: "image/gif" };
  }
  const bitmap = await decode(file);
  const scale = Math.min(1, MAX_EDGE / Math.max(bitmap.width, bitmap.height));
  const width = Math.max(1, Math.round(bitmap.width * scale));
  const height = Math.max(1, Math.round(bitmap.height * scale));
  // PNG for screenshots (sharp edges, text); JPEG for photographs.
  const mediaType = file.type === "image/png" ? "image/png" : "image/jpeg";
  const blob = await draw(bitmap, width, height, mediaType);
  if ("close" in bitmap) bitmap.close();
  return { blob, mediaType };
}

/**
 * Decode to a bitmap, honouring the EXIF orientation so the re-encode does not
 * rotate the picture. `createImageBitmap` cannot decode HEIC everywhere, so
 * fall back to an `<img>`, which uses the platform decoder.
 */
async function decode(file: File): Promise<ImageBitmap | HTMLImageElement> {
  if (typeof createImageBitmap === "function") {
    try {
      return await createImageBitmap(file, { imageOrientation: "from-image" });
    } catch {
      /* fall through to the element decoder */
    }
  }
  const url = URL.createObjectURL(file);
  try {
    return await new Promise<HTMLImageElement>((resolve, reject) => {
      const image = new Image();
      image.onload = () => resolve(image);
      image.onerror = () => reject(new Error("这张图片无法解码，请先导出成 JPEG 或 PNG 再试"));
      image.src = url;
    });
  } finally {
    URL.revokeObjectURL(url);
  }
}

async function draw(
  source: ImageBitmap | HTMLImageElement,
  width: number,
  height: number,
  mediaType: string,
): Promise<Blob> {
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  const context = canvas.getContext("2d");
  if (!context) throw new Error("浏览器无法处理这张图片");
  context.drawImage(source as CanvasImageSource, 0, 0, width, height);
  const blob = await new Promise<Blob | null>((resolve) =>
    canvas.toBlob(resolve, mediaType, 0.85),
  );
  if (!blob) throw new Error("图片转码失败");
  return blob;
}

/** Build the pending chip shown while an upload is in flight. */
export function pendingAttachment(file: File): Attachment {
  const image = isImageFile(file);
  return {
    localId: localId(),
    name: file.name || (image ? "image" : "file"),
    kind: image ? "image" : "file",
    // A non-image File without a declared type is an unknown binary; the Hub
    // stores it as application/octet-stream.
    mediaType: file.type || (image ? "image/png" : "application/octet-stream"),
    size: file.size,
    previewUrl: URL.createObjectURL(file),
    state: "uploading",
  };
}

/** Release a chip's object URL. Safe to call more than once. */
export function releaseAttachment(attachment: Attachment): void {
  if (attachment.previewUrl.startsWith("blob:")) URL.revokeObjectURL(attachment.previewUrl);
}

/** Metadata put on the send command. Bytes never travel with it. */
export type AttachmentRef = {
  /**
   * 1-based anchor number, equal to the chip's position and the
   * `[Image #n]`/`[File #n]` token the model sees in the prompt. The array
   * itself is ordered by the tokens' first appearance (see {@link refsOf}).
   */
  index: number;
  objectId: string;
  kind: AttachmentKind;
  mediaType: string;
  name: string;
  size: number;
};

/** Resolve a chip's token family against a position, for ordering refs. */
function tokenIndexAt(kind: AnchorKind, tokenText: string): Map<number, number> {
  const firstAt = new Map<number, number>();
  for (const span of findAnchorsFor(kind, tokenText)) {
    if (!firstAt.has(span.index)) firstAt.set(span.index, span.start);
  }
  return firstAt;
}

/**
 * The staged references for a set of chips.
 *
 * Without `tokenText`, refs come out in chip order. With it, chips whose
 * `[Image #n]`/`[File #n]` token still appears are ordered by that token's
 * first appearance, so the manifest the agent resolves reads in the same
 * order as the prompt; chips whose token was edited away ("未引用" — they are
 * still sent) follow in chip order. Images and files share one numbering
 * space (chip position).
 */
export function refsOf(attachments: Attachment[], tokenText?: string): AttachmentRef[] {
  const ready = attachments
    .map((attachment, position) => ({ attachment, index: position + 1 }))
    .filter(
      (entry): entry is { attachment: Attachment; index: number } =>
        entry.attachment.state === "ready" && Boolean(entry.attachment.objectId),
    )
    .map(({ attachment, index }) => ({
      index,
      objectId: attachment.objectId as string,
      kind: attachment.kind,
      mediaType: attachment.mediaType,
      name: attachment.name,
      size: attachment.size,
    }));
  if (tokenText === undefined) return ready;
  const imageAt = tokenIndexAt("Image", tokenText);
  const fileAt = tokenIndexAt("File", tokenText);
  const referenced = ready
    .filter((ref) => imageAt.has(ref.index) || fileAt.has(ref.index))
    .sort((a, b) => {
      const at = (ref: typeof a) =>
        (ref.kind === "image" ? imageAt.get(ref.index) : fileAt.get(ref.index)) ?? Number.MAX_SAFE_INTEGER;
      return at(a) - at(b);
    });
  const dangling = ready.filter((ref) => !imageAt.has(ref.index) && !fileAt.has(ref.index));
  return [...referenced, ...dangling];
}

/** Compact human size, shared with chip rendering. */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}
