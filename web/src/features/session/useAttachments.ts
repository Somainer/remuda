import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../../lib/api";
import {
  MAX_ATTACHMENTS,
  MAX_ATTACHMENT_BYTES,
  clipboardHasUnreadableImage,
  imagesFromClipboard,
  normalizeImage,
  pendingAttachment,
  refsOf,
  releaseAttachment,
  type Attachment,
} from "../../lib/attachments";

/**
 * Composer attachment state (D-027): normalize, upload, and hand back the
 * staged refs at send time.
 *
 * Lives apart from the Composer so the composer itself gains only a few lines.
 */
export function useAttachments(instanceId: string) {
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  // Files are kept so a failed upload can be retried without re-picking.
  const files = useRef(new Map<string, File>());
  const live = useRef(true);

  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  // Switching sessions drops any draft images; bytes must not leak across.
  useEffect(() => {
    // Capture the map now: by cleanup time `files.current` may already point
    // at the next session's state.
    const pending = files.current;
    return () => {
      setAttachments((current) => {
        current.forEach(releaseAttachment);
        return [];
      });
      pending.clear();
    };
  }, [instanceId]);

  const upload = useCallback(
    async (localId: string, file: File) => {
      try {
        const { blob, mediaType } = await normalizeImage(file);
        if (blob.size > MAX_ATTACHMENT_BYTES) {
          throw new Error("图片太大（上限 5 MB）");
        }
        const staged = await api.objectUpload(instanceId, blob, mediaType);
        if (!live.current) return;
        setAttachments((current) =>
          current.map((attachment) =>
            attachment.localId === localId
              ? {
                  ...attachment,
                  state: "ready",
                  objectId: staged.objectId,
                  mediaType,
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

  /** Stage a batch of picked, pasted, or dropped images. */
  const add = useCallback(
    (incoming: File[]) => {
      if (incoming.length === 0) return;
      setNotice(null);
      setAttachments((current) => {
        const room = MAX_ATTACHMENTS - current.length;
        if (room <= 0) {
          setNotice(`一条消息最多 ${MAX_ATTACHMENTS} 张图片`);
          return current;
        }
        const accepted = incoming.slice(0, room);
        if (accepted.length < incoming.length) {
          setNotice(`一条消息最多 ${MAX_ATTACHMENTS} 张图片`);
        }
        const staged = accepted.map((file) => {
          const attachment = pendingAttachment(file);
          files.current.set(attachment.localId, file);
          void upload(attachment.localId, file);
          return attachment;
        });
        return [...current, ...staged];
      });
    },
    [upload],
  );

  const remove = useCallback((localId: string) => {
    setAttachments((current) => {
      const target = current.find((attachment) => attachment.localId === localId);
      if (target) releaseAttachment(target);
      files.current.delete(localId);
      return current.filter((attachment) => attachment.localId !== localId);
    });
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
   * Handle a paste. Returns true when images were consumed, so the caller
   * only calls `preventDefault` then — swallowing every paste would break
   * plain-text pasting and the iOS caret.
   */
  const onPaste = useCallback(
    (data: DataTransfer | null): boolean => {
      const images = imagesFromClipboard(data);
      if (images.length > 0) {
        add(images);
        return true;
      }
      if (clipboardHasUnreadableImage(data)) {
        // iOS can declare an image while exposing no file; point at the
        // explicit button, which goes through the async clipboard API.
        setNotice("没能读到剪贴板里的图片，试试「粘贴图片」按钮或用 📎 选择文件");
      }
      return false;
    },
    [add],
  );

  /**
   * Read an image from the async clipboard API. Must be called inside a user
   * gesture: Chromium wants transient activation, and WebKit shows a platform
   * confirmation that a later click would cancel.
   */
  const pasteFromClipboard = useCallback(async () => {
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
        return;
      }
      add(picked);
    } catch {
      setNotice("浏览器不允许读取剪贴板，请用 📎 选择文件");
    }
  }, [add]);

  /** Drop every chip, after a successful send or an explicit discard. */
  const clear = useCallback(() => {
    setAttachments((current) => {
      current.forEach(releaseAttachment);
      return [];
    });
    files.current.clear();
    setNotice(null);
  }, []);

  /**
   * Detach the chips without revoking their object URLs, so the sent bubble
   * can keep showing the thumbnails. The bubble owns them from here.
   */
  const handOff = useCallback(() => {
    setAttachments((current) => {
      files.current.clear();
      void current;
      return [];
    });
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
    refs: () => refsOf(attachments),
  };
}
