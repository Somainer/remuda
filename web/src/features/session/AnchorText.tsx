import type { ReactNode } from "react";
import { findAllAnchors } from "../../lib/imageAnchors";
import { formatSize } from "../../lib/attachments";
import { objectUrl } from "./AttachmentChips";
import css from "./AnchorText.module.css";

/**
 * Render prompt text with anchor tokens turned into inline chips:
 *
 * - `[Image #n]` (2026-09-15): inline thumbnail linking the staged preview.
 * - `[File #n]` (D-027b, 2026-09-15): an inline file chip (type glyph + name),
 *   linking the Hub object download.
 * - `[Code #n]` (workbench-code-2): a quoted-code chip (no thumbnail), with
 *   the same number/token pairing.
 *
 * A token with no matching attachment is rendered as a plain token rather
 * than dropped, so the text never silently loses words.
 *
 * Kept dependency-free of transcript state; batch E adopted it for journaled
 * user messages once attachments are echoed on the journal.
 */

export type AnchorAttachment = {
  objectId: string;
  name: string;
  /** A viewable URL: blob URL for a staged image, Hub object URL otherwise. */
  previewUrl: string;
  /** Image renders a thumbnail; file renders a type chip. */
  kind: "image" | "file";
  mediaType?: string;
  size?: number;
  /** 1-based number matching the token. */
  index?: number;
};

export type CodeAnchorView = {
  index: number;
  /** Heading shown on the chip: path or language block. */
  title: string;
  /** One-line preview (already truncated if wanted). */
  preview?: string;
};

function fileGlyph(name: string, mediaType?: string): string {
  const ext = name
    .slice(name.lastIndexOf(".") + 1)
    .toLowerCase();
  if (mediaType?.startsWith("audio/") || ["mp3", "wav", "ogg"].includes(ext)) return "🎵";
  if (mediaType?.startsWith("video/") || ["mp4", "webm", "mov"].includes(ext)) return "🎬";
  if (["zip", "gz", "tar", "7z"].includes(ext)) return "🗜️";
  if (["pdf"].includes(ext)) return "📕";
  if (["txt", "md", "log"].includes(ext) || mediaType?.startsWith("text/")) return "📄";
  return "📎";
}

export function AnchorText({
  text,
  attachments,
  codeQuotes,
  className,
}: {
  text: string;
  attachments?: readonly AnchorAttachment[];
  codeQuotes?: readonly CodeAnchorView[];
  className?: string;
}) {
  // Images and files share one numbering space, keyed by position.
  const media = new Map<number, AnchorAttachment>();
  for (const attachment of attachments ?? []) {
    if (attachment.index) media.set(attachment.index, attachment);
  }
  const quotes = new Map<number, CodeAnchorView>();
  for (const quote of codeQuotes ?? []) quotes.set(quote.index, quote);

  const parts: ReactNode[] = [];
  let cursor = 0;
  for (const span of findAllAnchors(text)) {
    if (span.start > cursor) parts.push(text.slice(cursor, span.start));
    const label = `[${span.kind} #${span.index}]`;
    const attachment = span.kind === "Image" || span.kind === "File"
      ? media.get(span.index)
      : undefined;
    const code = span.kind === "Code" ? quotes.get(span.index) : undefined;
    const key = `${span.kind}-${span.index}-${span.start}`;
    if (attachment?.kind === "image") {
      parts.push(
        <a
          key={key}
          className={css.anchor}
          data-testid="inline-image-anchor"
          data-index={span.index}
          href={attachment.previewUrl}
          target="_blank"
          rel="noreferrer"
          title={attachment.name}
        >
          <img className={css.thumb} src={attachment.previewUrl} alt={attachment.name} />
          <span className={css.label}>{label}</span>
        </a>,
      );
    } else if (attachment) {
      // A non-image file: a compact chip linking the Hub object.
      parts.push(
        <a
          key={key}
          className={css.fileAnchor}
          data-testid="inline-file-anchor"
          data-index={span.index}
          href={objectUrl(attachment.objectId)}
          target="_blank"
          rel="noreferrer"
          download={attachment.name}
          title={attachment.size ? `${attachment.name} · ${formatSize(attachment.size)}` : attachment.name}
        >
          <span className={css.fileMark} aria-hidden>
            {fileGlyph(attachment.name, attachment.mediaType)}
          </span>
          <span className={css.label}>{label}</span>
        </a>,
      );
    } else if (code) {
      parts.push(
        <span
          key={key}
          className={css.codeAnchor}
          data-testid="inline-code-anchor"
          data-index={span.index}
          title={code.preview}
        >
          <span className={css.codeMark} aria-hidden>
            {"</>"}
          </span>
          <span className={css.label}>{label}</span>
        </span>,
      );
    } else {
      parts.push(
        <span
          key={key}
          className={css.token}
          data-testid={
            span.kind === "Image"
              ? "inline-image-token"
              : span.kind === "File"
                ? "inline-file-token"
                : "inline-code-token"
          }
          data-index={span.index}
        >
          {label}
        </span>,
      );
    }
    cursor = span.end;
  }
  if (cursor < text.length) parts.push(text.slice(cursor));
  return (
    <p className={className} data-testid="anchor-text">
      {parts}
    </p>
  );
}
