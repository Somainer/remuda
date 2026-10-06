import { useEffect } from "react";
import { questionAlertWatcher } from "./questionAlertStore";

/**
 * Starts the process-wide question-alert watcher (toast + document.title
 * badge) for the app's lifetime. Mounted once in Shell AND PhoneShell. The
 * watcher is a global singleton, so mounting here only starts it; it is NOT
 * reset on mount/unmount (the desktop Shell and compact PhoneShell mount
 * different roots as the route crosses /s/:id ↔ /m, and resetting would
 * re-baseline a question that arrived during the transition). Tests reset
 * explicitly.
 */
export function QuestionAlertHost() {
  useEffect(() => {
    questionAlertWatcher.start();
  }, []);
  return null;
}
