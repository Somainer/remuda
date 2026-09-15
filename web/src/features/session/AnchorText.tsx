import type { ReactNode } from "react";
import { findAllAnchors } from "../../lib/imageAnchors";
import css from "./AnchorText.module.css";

/**
 * Render prompt text with anchor tokens turned into inline chips:
 *
 * - `[Image #n]` (2026-09-15): inline thumbnail linking the staged preview.
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
  previewUrl: string;
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
  const images = new Map<number, AnchorAttachment>();
  for (const attachment of attachments ?? []) {
    if (attachment.index) images.set(attachment.index, attachment);
  }
  const quotes = new Map<number, CodeAnchorView>();
  for (const quote of codeQuotes ?? []) quotes.set(quote.index, quote);

  const parts: ReactNode[] = [];
  let cursor = 0;
  for (const span of findAllAnchors(text)) {
    if (span.start > cursor) parts.push(text.slice(cursor, span.start));
    const label = `[${span.kind} #${span.index}]`;
    const attachment = span.kind === "Image" ? images.get(span.index) : undefined;
    const code = span.kind === "Code" ? quotes.get(span.index) : undefined;
    const key = `${span.kind}-${span.index}-${span.start}`;
    if (attachment) {
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
          data-testid={span.kind === "Image" ? "inline-image-token" : "inline-code-token"}
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
