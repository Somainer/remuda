import type { Attachment } from "../../lib/attachments";
import css from "./AttachmentChips.module.css";

/**
 * Thumbnails for images staged on the current draft (D-027).
 *
 * A failed chip stays in place with its message rather than disappearing, so
 * the upload can be retried without re-picking the file.
 */
export function AttachmentChips({
  attachments,
  onRemove,
  onRetry,
}: {
  attachments: Attachment[];
  onRemove: (localId: string) => void;
  onRetry?: (localId: string) => void;
}) {
  if (attachments.length === 0) return null;
  return (
    <div className={css.row} data-testid="attachment-chips">
      {attachments.map((attachment) => (
        <div
          key={attachment.localId}
          className={css.chip}
          data-state={attachment.state}
          data-testid="attachment-chip"
        >
          <img className={css.thumb} src={attachment.previewUrl} alt={attachment.name} />
          <span className={css.meta}>
            <span className={css.name} title={attachment.name}>
              {attachment.name}
            </span>
            <span className={css.status}>
              {attachment.state === "uploading"
                ? "上传中…"
                : attachment.state === "failed"
                  ? (attachment.error ?? "上传失败")
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
            aria-label={`移除 ${attachment.name}`}
            data-testid="attachment-remove"
            onClick={() => onRemove(attachment.localId)}
          >
            ×
          </button>
        </div>
      ))}
    </div>
  );
}

/** Thumbnails shown beneath a message that was sent with images. */
export function SentAttachments({
  attachments,
}: {
  attachments: { objectId: string; name: string; previewUrl: string }[];
}) {
  if (attachments.length === 0) return null;
  return (
    <div className={css.sent} data-testid="sent-attachments">
      {attachments.map((attachment) => (
        <img
          key={attachment.objectId}
          className={css.sentThumb}
          src={attachment.previewUrl}
          alt={attachment.name}
        />
      ))}
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
