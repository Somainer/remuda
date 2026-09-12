/** rendered-ansi reset: prefix ESC c on the first write of a new stream (protocol.md §7.4). */

export const RIS = new Uint8Array([0x1b, 0x63]);

export function payloadForStreamWrite(payload: Uint8Array, resetStream: boolean): Uint8Array {
  if (!resetStream) return payload;
  const out = new Uint8Array(payload.byteLength + RIS.byteLength);
  out.set(RIS, 0);
  out.set(payload, RIS.byteLength);
  return out;
}

/** Strip CSI / OSC for the lab preview and tests. Not a VT parser. */
export function stripAnsi(bytes: Uint8Array): string {
  const text = new TextDecoder().decode(bytes);
  const esc = String.fromCharCode(27);
  const bel = String.fromCharCode(7);
  let out = "";
  for (let i = 0; i < text.length; i++) {
    const ch = text[i];
    if (ch === "\r") continue;
    if (ch !== esc) {
      out += ch;
      continue;
    }
    const next = text[i + 1];
    if (next === "]") {
      i += 2;
      while (i < text.length && text[i] !== bel && !(text[i] === esc && text[i + 1] === "\\")) i++;
      if (text[i] === esc) i++;
      continue;
    }
    if (next === "[") {
      i += 2;
      while (i < text.length && !/[A-Za-z@-~]/.test(text[i])) i++;
      continue;
    }
    i++;
  }
  return out;
}
