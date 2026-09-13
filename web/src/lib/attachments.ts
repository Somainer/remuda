/**
 * D-027: turn a pasted, dropped, or picked image into a staged Hub object.
 *
 * Every image is re-encoded through a canvas before it leaves the browser.
 * That is not only for size: re-encoding drops EXIF — including GPS — and the
 * ICC profile, and it converts formats (notably iOS HEIC) that the Hub's
 * allowlist would otherwise reject. Doing it here means the Hub never decodes
 * an image and so never links an image library.
 */

/** Media types the Hub will accept. Anything else must be converted first. */
export const ACCEPTED_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"] as const;

/** Longest edge after downscaling. Matches what vision models actually use. */
const MAX_EDGE = 1568;
/** Hub's per-attachment ceiling. */
export const MAX_ATTACHMENT_BYTES = 5 * 1024 * 1024;
/** Hub's per-message ceiling. */
export const MAX_ATTACHMENTS = 4;

/** A local image being prepared or already staged on the Hub. */
export type Attachment = {
  /** Client-side identity, stable across the upload. */
  localId: string;
  /** Hub object id once staged. */
  objectId?: string;
  /** Name shown in the chip. Never sent as a path. */
  name: string;
  mediaType: string;
  size: number;
  /** `blob:` URL for the thumbnail; revoked when the chip goes away. */
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

/** True when the browser handed us something we can treat as an image. */
export function isImageFile(file: File | null | undefined): file is File {
  return Boolean(file && file.type.startsWith("image/"));
}

/**
 * Pull image files out of a paste.
 *
 * Reads both `items` and `files`: iOS can report an image in `types` while
 * `files` is empty, and the two collections do not always agree. Duplicates
 * are collapsed by name and size, since a single paste often appears in both.
 */
export function imagesFromClipboard(data: DataTransfer | null): File[] {
  if (!data) return [];
  const found: File[] = [];
  const seen = new Set<string>();
  const push = (file: File | null) => {
    if (!isImageFile(file)) return;
    const key = `${file.name}:${file.size}:${file.type}`;
    if (seen.has(key)) return;
    seen.add(key);
    found.push(file);
  };
  for (const item of Array.from(data.items ?? [])) {
    if (item.kind === "file" && item.type.startsWith("image/")) push(item.getAsFile());
  }
  for (const file of Array.from(data.files ?? [])) push(file);
  return found;
}

/** True when a paste carries an image but exposed no readable file (iOS). */
export function clipboardHasUnreadableImage(data: DataTransfer | null): boolean {
  if (!data) return false;
  const declares = Array.from(data.types ?? []).some((type) => type.startsWith("image/"));
  return declares && imagesFromClipboard(data).length === 0;
}

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
  return {
    localId: localId(),
    name: file.name || "image",
    mediaType: file.type || "image/png",
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
export type AttachmentRef = { objectId: string; mediaType: string; name: string; size: number };

/** The staged references for a set of chips, in chip order. */
export function refsOf(attachments: Attachment[]): AttachmentRef[] {
  return attachments
    .filter((attachment) => attachment.state === "ready" && attachment.objectId)
    .map((attachment) => ({
      objectId: attachment.objectId as string,
      mediaType: attachment.mediaType,
      name: attachment.name,
      size: attachment.size,
    }));
}
