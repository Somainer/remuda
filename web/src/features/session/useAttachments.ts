import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../../lib/api";
import {
  MAX_ATTACHMENTS,
  MAX_ATTACHMENT_BYTES,
  clipboardHasUnreadableImage,
  filesFromClipboard,
  formatSize,
  normalizeImage,
  pendingAttachment,
  refsOf,
  releaseAttachment,
  type Attachment,
} from "../../lib/attachments";
import type { AnchorKind } from "../../lib/imageAnchors";

/** Claimed chip position plus the token family its file needs. */
export type AddedAnchor = { index: number; kind: AnchorKind };

/**
 * Composer attachment state (D-027/D-027b): normalize, upload, and hand back
 * the staged refs at send time.
 *
 * Lives apart from the Composer so the composer itself gains only a few lines.
 *
 * Anchors (2026-09-15): every chip's position in the `attachments` array is
 * its 1-based anchor number, so `add` returns the numbers it just claimed and
 * the composer can insert matching `[Image #n]`/`[File #n]` tokens at the
 * caret. Images and files share one numbering space (chip position). The list
 * ref mirrors state synchronously — React state updates are async, but two
 * pastes in the same tick must claim different numbers.
 */
export function useAttachments(instanceId: string) {
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  // Files are kept so a failed upload can be retried without re-picking.
  const files = useRef(new Map<string, File>());
  // Synchronous mirror of `attachments`, for anchor-position arithmetic.
  const listRef = useRef<Attachment[]>([]);
  const live = useRef(true);

  useEffect(() => {
    listRef.current = attachments;
  }, [attachments]);

  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  // Switching sessions drops any draft files; bytes must not leak across.
  useEffect(() => {
    // Capture the map now: by cleanup time `files.current` may already point
    // at the next session's state.
    const pending = files.current;
    return () => {
      setAttachments((current) => {
        current.forEach(releaseAttachment);
        return [];
      });
      listRef.current = [];
      pending.clear();
    };
  }, [instanceId]);

  const upload = useCallback(
    async (localId: string, file: File) => {
      try {
        const image = file.type.startsWith("image/");
        const { blob, mediaType } = image
          ? await normalizeImage(file)
          : { blob: file, mediaType: file.type || "application/octet-stream" };
        if (blob.size > MAX_ATTACHMENT_BYTES) {
          throw new Error(`文件太大（上限 ${formatSize(MAX_ATTACHMENT_BYTES)}）`);
        }
        const staged = await api.objectUpload(instanceId, blob, mediaType, file.name);
        if (!live.current) return;
        setAttachments((current) =>
          current.map((attachment) =>
            attachment.localId === localId
              ? {
                  ...attachment,
                  state: "ready",
                  objectId: staged.objectId,
                  kind: staged.kind ?? attachment.kind,
                  name: staged.name || attachment.name,
                  mediaType: staged.mediaType || mediaType,
                  size: staged.size,
                }
              : attachment,
          ),
        );
      } catch (error) {
        if (!live.current) return;
        const message = error instanceof Error ? error.message : "上传失败";
        setAttachments((current) =>
          current.map((attachment) =>
            attachment.localId === localId
              ? { ...attachment, state: "failed", error: message }
              : attachment,
          ),
        );
      }
    },
    [instanceId],
  );

  /**
   * Stage a batch of picked, pasted, or dropped files (any type, D-027b).
   *
   * Returns the 1-based anchor numbers the accepted files claimed (their
   * eventual chip positions), in input order, with the token family each
   * needs, so the caller inserts matching tokens. Over-capacity files are
   * rejected here and only the accepted count is returned.
   */
  const add = useCallback(
    (incoming: File[]): AddedAnchor[] => {
      if (incoming.length === 0) return [];
      setNotice(null);
      const base = listRef.current.length;
      const room = MAX_ATTACHMENTS - base;
      if (room <= 0) {
        setNotice(`一条消息最多 ${MAX_ATTACHMENTS} 个附件`);
        return [];
      }
      const accepted = incoming.slice(0, room);
      if (accepted.length < incoming.length) {
        setNotice(`一条消息最多 ${MAX_ATTACHMENTS} 个附件`);
      }
      const staged = accepted.map((file) => {
        const attachment = pendingAttachment(file);
        files.current.set(attachment.localId, file);
        void upload(attachment.localId, file);
        return attachment;
      });
      const next = [...listRef.current, ...staged];
      listRef.current = next;
      setAttachments(next);
      return accepted.map((file, offset) => ({
        index: base + offset + 1,
        kind: (file.type.startsWith("image/") ? "Image" : "File") as AnchorKind,
      }));
    },
    [upload],
  );

  /**
   * Remove one chip. Returns the anchor number it held, so the caller can pull
   * the token out of the prompt and renumber the rest.
   */
  const remove = useCallback((localId: string): number | null => {
    const current = listRef.current;
    const position = current.findIndex((attachment) => attachment.localId === localId);
    if (position < 0) return null;
    const target = current[position];
    releaseAttachment(target);
    files.current.delete(localId);
    const next = current.filter((attachment) => attachment.localId !== localId);
    listRef.current = next;
    setAttachments(next);
    return position + 1;
  }, []);

  const retry = useCallback(
    (localId: string) => {
      const file = files.current.get(localId);
      if (!file) return;
      setAttachments((current) =>
        current.map((attachment) =>
          attachment.localId === localId
            ? { ...attachment, state: "uploading", error: undefined }
            : attachment,
        ),
      );
      void upload(localId, file);
    },
    [upload],
  );

  /**
   * Handle a paste. Returns the anchors inserted (empty when the paste
   * carried no usable file), so the caller both knows whether to
   * `preventDefault` and which tokens to place at the caret.
   */
  const onPaste = useCallback(
    (data: DataTransfer | null): AddedAnchor[] => {
      const files = filesFromClipboard(data);
      if (files.length > 0) return add(files);
      if (clipboardHasUnreadableImage(data)) {
        // iOS can declare an image while exposing no file; point at the
        // explicit button, which goes through the async clipboard API.
        setNotice("没能读到剪贴板里的图片，试试「粘贴附件」按钮或用 📎 选择文件");
      }
      return [];
    },
    [add],
  );

  /**
   * Read an image from the async clipboard API. Must be called inside a user
   * gesture: Chromium wants transient activation, and WebKit shows a platform
   * confirmation that a later click would cancel.
   */
  const pasteFromClipboard = useCallback(async (): Promise<AddedAnchor[]> => {
    setNotice(null);
    try {
      const items = await navigator.clipboard.read();
      const picked: File[] = [];
      for (const item of items) {
        const type = item.types.find((candidate) => candidate.startsWith("image/"));
        if (!type) continue;
        const blob = await item.getType(type);
        picked.push(new File([blob], `clipboard.${type.split("/")[1] ?? "png"}`, { type }));
      }
      if (picked.length === 0) {
        setNotice("剪贴板里没有图片");
        return [];
      }
      return add(picked);
    } catch {
      setNotice("浏览器不允许读取剪贴板，请用 📎 选择文件");
      return [];
    }
  }, [add]);

  /** Drop every chip, after a successful send or an explicit discard. */
  const clear = useCallback(() => {
    const current = listRef.current;
    current.forEach(releaseAttachment);
    listRef.current = [];
    setAttachments([]);
    files.current.clear();
    setNotice(null);
  }, []);

  /**
   * Detach the chips without revoking their object URLs, so the sent bubble
   * can keep showing the thumbnails. The bubble owns them from here.
   */
  const handOff = useCallback(() => {
    files.current.clear();
    listRef.current = [];
    setAttachments([]);
    setNotice(null);
  }, []);

  const uploading = attachments.some((attachment) => attachment.state === "uploading");
  return {
    attachments,
    notice,
    uploading,
    add,
    remove,
    retry,
    onPaste,
    pasteFromClipboard,
    clear,
    handOff,
    /** Manifest refs in token order; `tokenText` is the prompt being sent. */
    refs: (tokenText?: string) => refsOf(listRef.current, tokenText),
  };
}
