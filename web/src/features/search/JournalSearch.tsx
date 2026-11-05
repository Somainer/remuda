import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { hubStore } from "../../lib/store";
import { hitHref, hitKindLabel, searchJournal, type JournalSearchResult } from "./journalSearch";
import css from "./journalSearch.module.css";

type State =
  | { kind: "idle" }
  | { kind: "loading"; query: string }
  | { kind: "done"; query: string; result: JournalSearchResult }
  | { kind: "failed"; query: string };

/**
 * cb-search (main-agent.md §12.7): the record search under the session
 * list's filter. The list filter matches names and metadata as you type;
 * this asks the Hub for the words inside the sessions' journals, once, on
 * request — never per keystroke. A hit opens `/s/:id/events?seq=N`.
 */
export function JournalSearch({ query }: { query: string }) {
  const [shown, setState] = useState<State>({ kind: "idle" });
  const abort = useRef<AbortController | null>(null);
  const trimmed = query.trim();
  // Hits for other words than the filter's are stale: show none of them.
  const state: State = shown.kind !== "idle" && shown.query === trimmed ? shown : { kind: "idle" };
  useEffect(() => () => abort.current?.abort(), []);

  if (!trimmed) return null;

  function run() {
    abort.current?.abort();
    const controller = new AbortController();
    abort.current = controller;
    setState({ kind: "loading", query: trimmed });
    searchJournal(trimmed, controller.signal).then(
      (result) => {
        if (!controller.signal.aborted) setState({ kind: "done", query: trimmed, result });
      },
      () => {
        if (!controller.signal.aborted) setState({ kind: "failed", query: trimmed });
      },
    );
  }

  return (
    <section className={css.section} data-testid="journal-search" aria-label="会话记录">
      <header className={css.head}>
        <span className={css.heading}>会话记录</span>
        <button
          type="button"
          className={css.run}
          data-testid="journal-search-run"
          title={`在会话记录里搜索「${trimmed}」`}
          disabled={state.kind === "loading"}
          onClick={run}
        >
          {state.kind === "loading" ? "搜索中…" : `搜索记录「${trimmed}」`}
        </button>
      </header>
      {state.kind === "failed" ? (
        <p className={css.note} role="alert" data-testid="journal-search-failed">
          搜索失败，请重试。
        </p>
      ) : null}
      {state.kind === "done" ? (
        <>
          {state.result.hits.length === 0 ? (
            <p className={css.note} data-testid="journal-search-empty">
              没有匹配的记录。
            </p>
          ) : (
            <ul className={css.list}>
              {state.result.hits.map((hit) => (
                <li key={`${hit.instanceId}:${hit.seq}`} className={css.item}>
                  <Link className={css.open} to={hitHref(hit)} data-testid="journal-search-hit">
                    <span className={css.meta}>
                      <span className={css.title}>{hubStore.titleOf(hit.instanceId)}</span>
                      <span>seq {hit.seq}</span>
                      <span>{hitKindLabel(hit)}</span>
                    </span>
                    <span className={css.snippet}>{hit.snippet}</span>
                  </Link>
                </li>
              ))}
            </ul>
          )}
          {state.result.partial ? (
            <p className={css.note} data-testid="journal-search-partial">
              搜索超时，只列出了部分结果。
            </p>
          ) : null}
          {!state.result.indexComplete ? (
            <p className={css.note} data-testid="journal-search-indexing">
              较早的记录仍在建索引，可能还搜不到。
            </p>
          ) : null}
        </>
      ) : null}
    </section>
  );
}
