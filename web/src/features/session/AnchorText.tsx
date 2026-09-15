import type { ReactNode } from "react";
import { findAnchors } from "../../lib/imageAnchors";
import css from "./AnchorText.module.css";

/**
 * Render prompt text with `[Image #n]` tokens turned into inline thumbnail
 * chips (2026-09-15).
 *
 * Each chip links to the attachment's local preview (`blob:` in the
 * composer/sent bubble, or whatever URL the caller hands over). A token with
 * no matching attachment is rendered as a plain token rather than dropped, so
 * the text never silently loses words.
 *
 * Kept dependency-free of transcript state: batch E adopts it directly for
 * journaled user messages; today it backs the optimistic local bubble.
 */

export type AnchorAttachment = {
  objectId: string;
  name: string;
  previewUrl: string;
  /** 1-based number matching the token. */
  index?: number;
};

export function AnchorText({
  text,
  attachments,
  className,
}: {
  text: string;
  attachments?: readonly AnchorAttachment[];
  className?: string;
}) {
  const byIndex = new Map<number, AnchorAttachment>();
  for (const attachment of attachments ?? []) {
    if (attachment.index) byIndex.set(attachment.index, attachment);
  }
  const parts: ReactNode[] = [];
  let cursor = 0;
  for (const span of findAnchors(text)) {
    if (span.start > cursor) parts.push(text.slice(cursor, span.start));
    const attachment = byIndex.get(span.index);
    const label = `[Image #${span.index}]`;
    parts.push(
      attachment ? (
        <a
          key={`${span.index}-${span.start}`}
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
        </a>
      ) : (
        <span
          key={`${span.index}-${span.start}`}
          className={css.token}
          data-testid="inline-image-token"
          data-index={span.index}
        >
          {label}
        </span>
      ),
    );
    cursor = span.end;
  }
  if (cursor < text.length) parts.push(text.slice(cursor));
  return (
    <p className={className} data-testid="anchor-text">
      {parts}
    </p>
  );
}
