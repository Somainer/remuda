import { useCallback, useEffect, useRef, useState } from "react";
import type { CodeQuote, DraftCodeQuote } from "../../lib/codeAnchors";
import { listenForCodeQuotes } from "../../lib/codeQuoteBus";

/**
 * Composer-side store for `[Code #n]` quote anchors (workbench-code-2).
 *
 * Mirrors useAttachments' contract: a synchronous list ref so rapid quotes
 * claim contiguous numbers, add returns the claimed index, and remove takes
 * the index so the Composer can strip/renumber its token. There is no upload:
 * the quote payload already contains everything the harness receives.
 */
let quoteSeq = 0;

export function useCodeQuotes(onAdded?: (index: number) => void) {
  const onAddedRef = useRef(onAdded);
  onAddedRef.current = onAdded;
  const [quotes, setQuotes] = useState<DraftCodeQuote[]>([]);
  const listRef = useRef<DraftCodeQuote[]>([]);
  useEffect(() => {
    listRef.current = quotes;
  }, [quotes]);

  const add = useCallback((quote: CodeQuote): number => {
    const index = listRef.current.length + 1;
    const next = [...listRef.current, { ...quote, localId: `code_${++quoteSeq}` }];
    listRef.current = next;
    setQuotes(next);
    onAddedRef.current?.(index);
    return index;
  }, []);

  const remove = useCallback((index: number): void => {
    // Positions are contiguous 1-based; dropping one shifts the rest. The
    // quote's turn/block/line identity stays with the chip — only the token
    // number changes, handled by renumberAnchors in the draft text.
    const next = listRef.current.filter((_, position) => position + 1 !== index);
    listRef.current = next;
    setQuotes(next);
  }, []);

  const clear = useCallback(() => {
    listRef.current = [];
    setQuotes([]);
  }, []);

  // Subscribe to 评论 presses for the lifetime of the composer.
  useEffect(() => listenForCodeQuotes((quote) => add(quote)), [add]);

  return { quotes, add, remove, clear };
}
