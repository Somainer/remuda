import type { Attachment, AttachmentKind } from "../../lib/attachments";
import { formatSize } from "../../lib/attachments";
import type { DraftCodeQuote } from "../../lib/codeAnchors";
import { quotePreview } from "../../lib/codeAnchors";
import css from "./AttachmentChips.module.css";

/**
 * Thumbnails/type chips for files staged on the current draft (D-027/D-027b).
 *
 * A failed chip stays in place with its message rather than disappearing, so
 * the upload can be retried without re-picking the file.
 *
 * Each chip carries its 1-based anchor number (2026-09-15): the same number
 * is in the prompt's `[Image #n]`/`[File #n]` token. Images show a thumbnail;
 * every other file shows a type glyph (D-027b, 2026-09-15). A chip whose
 * token was edited out of the text is marked "未引用" — the file is still sent
 * — rather than removed.
 */
export function AttachmentChips({
  attachments,
  unreferenced,
  onRemove,
  onRetry,
}: {
  attachments: Attachment[];
  /** localIds whose anchor token no longer appears in the draft text. */
  unreferenced?: ReadonlySet<string>;
  onRemove: (localId: string) => void;
  onRetry?: (localId: string) => void;
}) {
  if (attachments.length === 0) return null;
  return (
    <div className={css.row} data-testid="attachment-chips">
      {attachments.map((attachment, position) => {
        const index = position + 1;
        const orphan = unreferenced?.has(attachment.localId) === true;
        return (
          <div
            key={attachment.localId}
            className={css.chip}
            data-state={attachment.state}
            data-kind={attachment.kind}
            data-unreferenced={orphan ? "1" : "0"}
            data-index={index}
            data-testid="attachment-chip"
          >
            <span className={css.thumbWrap}>
              {attachment.kind === "image" ? (
                <img
                  className={css.thumb}
                  src={attachment.previewUrl}
                  alt={attachment.name}
                />
              ) : (
                <span className={css.fileGlyph} aria-hidden>
                  {fileGlyph(attachment.name, attachment.mediaType)}
                </span>
              )}
              <span className={css.indexBadge} data-testid="attachment-index" aria-hidden>
                {index}
              </span>
            </span>
            <span className={css.meta}>
              <span className={css.name} title={attachment.name}>
                {attachment.name}
              </span>
              <span className={css.status}>
                {attachment.state === "uploading"
                  ? "上传中…"
                  : attachment.state === "failed"
                    ? (attachment.error ?? "上传失败")
                    : orphan
                      ? "未引用（仍会发送）"
                      : formatSize(attachment.size)}
              </span>
            </span>
            {attachment.state === "failed" && onRetry ? (
              <button
                type="button"
                className={css.remove}
                aria-label={`重试 ${attachment.name}`}
                data-testid="attachment-retry"
                onClick={() => onRetry(attachment.localId)}
              >
                ↻
              </button>
            ) : null}
            <button
              type="button"
              className={css.remove}
              aria-label={`移除 ${attachment.name}（附件 ${index}）`}
              data-testid="attachment-remove"
              onClick={() => onRemove(attachment.localId)}
            >
              ×
            </button>
          </div>
        );
      })}
    </div>
  );
}

/** Short extension label for a non-image chip badge. */
function extensionOf(name: string): string {
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return "";
  return name.slice(dot + 1).slice(0, 4).toUpperCase();
}

/** Type glyph for a non-image file. */
function fileGlyph(name: string, mediaType: string): string {
  const ext = extensionOf(name).toLowerCase();
  if (mediaType.startsWith("audio/") || ["mp3", "wav", "ogg", "flac"].includes(ext)) return "🎵";
  if (mediaType.startsWith("video/") || ["mp4", "webm", "mov"].includes(ext)) return "🎬";
  if (["zip", "gz", "tar", "bz2", "7z", "xz", "rar"].includes(ext)) return "🗜️";
  if (["pdf"].includes(ext)) return "📕";
  if (["doc", "docx"].includes(ext)) return "📘";
  if (["xls", "xlsx", "csv"].includes(ext)) return "📗";
  if (["ppt", "pptx"].includes(ext)) return "📙";
  if (["txt", "md", "log"].includes(ext) || mediaType.startsWith("text/")) return "📄";
  if (["json", "js", "ts", "tsx", "rs", "py", "go", "java", "c", "cpp", "sh"].includes(ext)) {
    return "📜";
  }
  return "📎";
}

/** Relative Hub object URL — same-origin in prod and through the Vite proxy. */
export function objectUrl(objectId: string): string {
  return `/v1/objects/${encodeURIComponent(objectId)}`;
}

/** Files/images shown beneath a message that was sent with attachments. */
export function SentAttachments({
  attachments,
}: {
  attachments: SentAttachment[];
}) {
  if (attachments.length === 0) return null;
  return (
    <div className={css.sent} data-testid="sent-attachments">
      {attachments.map((attachment) => {
        const indexLabel = attachment.index ? ` #${attachment.index}` : "";
        if (attachment.kind === "image") {
          return (
            <span key={attachment.objectId} className={css.sentWrap}>
              <a href={objectUrl(attachment.objectId)} target="_blank" rel="noreferrer">
                <img
                  className={css.sentThumb}
                  // For a freshly sent bubble this is the local blob URL; a
                  // reloaded view would use the Hub object URL instead.
                  src={attachment.previewUrl}
                  alt={attachment.name}
                  data-index={attachment.index ?? ""}
                />
              </a>
              {attachment.index ? (
                <span className={css.indexBadge} data-testid="sent-attachment-index" aria-hidden>
                  {attachment.index}
                </span>
              ) : null}
            </span>
          );
        }
        return (
          <a
            key={attachment.objectId}
            className={css.sentFile}
            href={objectUrl(attachment.objectId)}
            target="_blank"
            rel="noreferrer"
            download={attachment.name}
            data-testid="sent-file"
            data-index={attachment.index ?? ""}
            title={`下载 ${attachment.name}`}
          >
            <span className={css.sentFileGlyph} aria-hidden>
              {fileGlyph(attachment.name, attachment.mediaType)}
            </span>
            <span className={css.sentFileMeta}>
              <span className={css.sentFileName}>{attachment.name}</span>
              <span className={css.sentFileSize}>
                {formatSize(attachment.size)}
                {indexLabel}
              </span>
            </span>
          </a>
        );
      })}
    </div>
  );
}

/** A file/image shown under a sent bubble. */
export type SentAttachment = {
  objectId: string;
  name: string;
  /** Blob URL for an optimistic image; unused for files (they link to Hub). */
  previewUrl: string;
  kind: AttachmentKind;
  mediaType: string;
  size: number;
  /** 1-based number matching the prompt token. */
  index?: number;
};

/**
 * Draft chips for quoted code blocks (workbench-code-2). Same visual language
 * as the file chips: numbered badge, title row, a one-line preview, and the
 * × that strips the `[Code #n]` token. A quote whose token was edited out is
 * dashed + "未引用（仍会发送）", exactly like file chips.
 */
export function CodeQuoteChips({
  quotes,
  unreferenced,
  onRemove,
}: {
  quotes: readonly DraftCodeQuote[];
  /** localIds whose `[Code #n]` token no longer appears in the draft text. */
  unreferenced?: ReadonlySet<string>;
  onRemove: (index: number) => void;
}) {
  if (quotes.length === 0) return null;
  return (
    <div className={css.row} data-testid="code-quote-chips">
      {quotes.map((quote, position) => {
        const index = position + 1;
        const orphan = unreferenced?.has(quote.localId) === true;
        const heading = quote.path || (quote.lang ? `${quote.lang} block` : "code block");
        return (
          <div
            key={quote.localId}
            className={css.chip}
            data-unreferenced={orphan ? "1" : "0"}
            data-index={index}
            data-testid="code-quote-chip"
          >
            <span className={css.thumbWrap}>
              <span className={css.codeGlyph} aria-hidden>
                {"</>"}
              </span>
              <span className={css.indexBadge} data-testid="code-quote-index" aria-hidden>
                {index}
              </span>
            </span>
            <span className={css.meta}>
              <span className={css.name} title={heading}>
                {heading}
              </span>
              <span className={css.status} title={quotePreview(quote)}>
                {orphan ? "未引用（仍会发送）" : quotePreview(quote)}
              </span>
            </span>
            <button
              type="button"
              className={css.remove}
              aria-label={`移除引用（代码 ${index}）`}
              data-testid="code-quote-remove"
              onClick={() => onRemove(index)}
            >
              ×
            </button>
          </div>
        );
      })}
    </div>
  );
}

/**
 * The ways to attach a file.
 *
 * The picker accepts any file type (D-027b); the camera entry stays
 * image-only. The explicit paste button reads images from the async
 * clipboard API — the escape hatch when iOS declares a clipboard image but
 * exposes no file.
 */
export function AttachButtons({
  disabled,
  mobile,
  onFiles,
  onPasteClick,
  className,
}: {
  disabled?: boolean;
  mobile?: boolean;
  onFiles: (files: File[]) => void;
  onPasteClick: () => void;
  className?: string;
}) {
  const pick = (capture?: "environment") => {
    const input = document.createElement("input");
    input.type = "file";
    // Any file type; images are normalised in the browser, everything else
    // passes through to the Hub unchanged (D-027b).
    if (!capture) input.accept = "*/*";
    else input.accept = "image/*";
    input.multiple = true;
    if (capture) input.capture = capture;
    input.onchange = () => {
      const files = Array.from(input.files ?? []);
      if (files.length) onFiles(files);
    };
    input.click();
  };
  return (
    <>
      <button
        type="button"
        className={className}
        data-testid="attach-file"
        aria-label="添加附件"
        title="添加附件"
        disabled={disabled}
        onClick={() => pick()}
      >
        📎
      </button>
      {mobile ? (
        <button
          type="button"
          className={className}
          data-testid="attach-camera"
          aria-label="拍照"
          title="拍照"
          disabled={disabled}
          onClick={() => pick("environment")}
        >
          📷
        </button>
      ) : null}
      <button
        type="button"
        className={className}
        data-testid="attach-paste"
        aria-label="粘贴附件"
        title="粘贴附件"
        disabled={disabled}
        onClick={onPasteClick}
      >
        粘贴附件
      </button>
    </>
  );
}
