import type { Attachment } from "../../lib/attachments";
import type { DraftCodeQuote } from "../../lib/codeAnchors";
import { quotePreview } from "../../lib/codeAnchors";
import css from "./AttachmentChips.module.css";

/**
 * Thumbnails for images staged on the current draft (D-027).
 *
 * A failed chip stays in place with its message rather than disappearing, so
 * the upload can be retried without re-picking the file.
 *
 * Each chip carries its 1-based anchor number (2026-09-15): the same number
 * is in the prompt's `[Image #n]` token. A chip whose token was edited out of
 * the text is marked "未引用" — the image is still sent — rather than removed.
 */
export function AttachmentChips({
  attachments,
  unreferenced,
  onRemove,
  onRetry,
}: {
  attachments: Attachment[];
  /** localIds whose `[Image #n]` token no longer appears in the draft text. */
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
            data-unreferenced={orphan ? "1" : "0"}
            data-index={index}
            data-testid="attachment-chip"
          >
            <span className={css.thumbWrap}>
              <img className={css.thumb} src={attachment.previewUrl} alt={attachment.name} />
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
              aria-label={`移除 ${attachment.name}（图片 ${index}）`}
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

/** Thumbnails shown beneath a message that was sent with images. */
export function SentAttachments({
  attachments,
}: {
  attachments: { objectId: string; name: string; previewUrl: string; index?: number }[];
}) {
  if (attachments.length === 0) return null;
  return (
    <div className={css.sent} data-testid="sent-attachments">
      {attachments.map((attachment) => (
        <span key={attachment.objectId} className={css.sentWrap}>
          <img
            className={css.sentThumb}
            src={attachment.previewUrl}
            alt={attachment.name}
            data-index={attachment.index ?? ""}
          />
          {attachment.index ? (
            <span className={css.indexBadge} data-testid="sent-attachment-index" aria-hidden>
              {attachment.index}
            </span>
          ) : null}
        </span>
      ))}
    </div>
  );
}

/**
 * Draft chips for quoted code blocks (workbench-code-2). Same visual language
 * as the image chips: numbered badge, title row, a one-line preview, and the
 * × that strips the `[Code #n]` token. A quote whose token was edited out is
 * dashed + "未引用（仍会发送）", exactly like image chips.
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

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * The three ways to attach an image.
 *
 * Three, not one, because no single entry point covers every platform: the
 * file picker is the reliable path on mobile, the camera entry is a
 * convenience, and the explicit paste button is the only escape hatch when
 * iOS declares a clipboard image but exposes no file, or when the on-screen
 * keyboard offers no paste affordance at all.
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
    input.accept = "image/*";
    input.multiple = true;
    if (capture) input.capture = capture;
    input.onchange = () => {
      const files = Array.from(input.files ?? []).filter((file) => file.type.startsWith("image/"));
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
        aria-label="添加图片"
        title="添加图片"
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
        aria-label="粘贴图片"
        title="粘贴图片"
        disabled={disabled}
        onClick={onPasteClick}
      >
        粘贴图片
      </button>
    </>
  );
}
