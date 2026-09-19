import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  SpeechInput,
  readVoiceInputEnabled,
  speechRecognitionSupported,
  writeVoiceInputEnabled,
  type SpeechRecognitionEventLike,
  type SpeechRecognitionLike,
  type SpeechResultLike,
} from "./speech";

/** Minimal SpeechRecognition standing in for Chrome's webkit constructor. */
class FakeRecognition implements SpeechRecognitionLike {
  static instances: FakeRecognition[] = [];

  lang = "";
  interimResults = false;
  continuous = true;
  start = vi.fn();
  stop = vi.fn();
  abort = vi.fn();
  onresult: SpeechRecognitionLike["onresult"] = null;
  onerror: SpeechRecognitionLike["onerror"] = null;
  onend: SpeechRecognitionLike["onend"] = null;

  constructor() {
    FakeRecognition.instances.push(this);
  }

  emit(event: SpeechRecognitionEventLike) {
    this.onresult?.(event);
  }

  fail(code: string) {
    this.onerror?.({ error: code });
  }

  end() {
    this.onend?.();
  }
}

function spoken(transcript: string, isFinal: boolean): SpeechResultLike {
  return { isFinal, 0: { transcript } };
}

function eventWith(resultIndex: number, results: SpeechResultLike[]): SpeechRecognitionEventLike {
  return { resultIndex, results };
}

beforeEach(() => {
  localStorage.clear();
  FakeRecognition.instances.length = 0;
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("speechRecognitionSupported (§4.8 detection)", () => {
  it("is false in a browser with neither constructor (the iOS Safari case)", () => {
    expect("SpeechRecognition" in window).toBe(false);
    expect("webkitSpeechRecognition" in window).toBe(false);
    expect(speechRecognitionSupported()).toBe(false);
  });

  it("is true for the webkit-prefixed constructor", () => {
    vi.stubGlobal("webkitSpeechRecognition", FakeRecognition);
    expect("webkitSpeechRecognition" in window).toBe(true);
    expect(speechRecognitionSupported()).toBe(true);
  });

  it("is true for the unprefixed constructor", () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    expect("SpeechRecognition" in window).toBe(true);
    expect(speechRecognitionSupported()).toBe(true);
  });
});

describe("voice input pref (default off, per device)", () => {
  it("defaults off", () => {
    expect(readVoiceInputEnabled()).toBe(false);
  });

  it("persists an explicit opt-in and can be switched back off", () => {
    writeVoiceInputEnabled(true);
    expect(localStorage.getItem("runtime.voice-input.v1")).toBe("1");
    expect(readVoiceInputEnabled()).toBe(true);
    writeVoiceInputEnabled(false);
    expect(readVoiceInputEnabled()).toBe(false);
  });

  it("treats a junk value as off", () => {
    localStorage.setItem("runtime.voice-input.v1", "yes");
    expect(readVoiceInputEnabled()).toBe(false);
  });
});

describe("SpeechInput wrapper", () => {
  it("reports unavailable and refuses to start without a constructor", () => {
    const input = new SpeechInput({ onTranscript: vi.fn() });
    expect(input.available).toBe(false);
    expect(input.listening).toBe(false);
    expect(() => input.start()).toThrow(/not available/);
    // stop/abort are harmless no-ops on the iOS Safari path.
    expect(() => {
      input.stop();
      input.abort();
    }).not.toThrow();
  });

  it("configures lang, interim results and a non-continuous session on start", () => {
    document.documentElement.lang = "zh-CN";
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    const input = new SpeechInput({ onTranscript: vi.fn() });
    expect(input.available).toBe(true);
    input.start();
    const rec = FakeRecognition.instances[0];
    expect(rec.start).toHaveBeenCalledTimes(1);
    expect(rec.lang).toBe("zh-CN");
    expect(rec.interimResults).toBe(true);
    expect(rec.continuous).toBe(false);
    expect(input.listening).toBe(true);
    document.documentElement.removeAttribute("lang");
  });

  it("falls back to the browser language when the page declares none", () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    new SpeechInput({ onTranscript: vi.fn() }).start();
    expect(FakeRecognition.instances[0].lang).toBe(navigator.language);
  });

  it("ignores a second start while a session is active", () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    const input = new SpeechInput({ onTranscript: vi.fn() });
    input.start();
    input.start();
    expect(FakeRecognition.instances[0].start).toHaveBeenCalledTimes(1);
  });

  it("routes cumulative interim+final transcripts to the callback", () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    const onTranscript = vi.fn();
    const input = new SpeechInput({ onTranscript });
    input.start();
    const rec = FakeRecognition.instances[0];

    rec.emit(eventWith(0, [spoken("hel", false)]));
    expect(onTranscript).toHaveBeenLastCalledWith("hel");
    // An updated interim for the same result replaces, not appends.
    rec.emit(eventWith(0, [spoken("hello", false)]));
    expect(onTranscript).toHaveBeenLastCalledWith("hello");
    rec.emit(eventWith(0, [spoken("hello", true)]));
    expect(onTranscript).toHaveBeenLastCalledWith("hello");
    // A later phrase carries the committed final at index 0.
    rec.emit(eventWith(1, [spoken("hello", true), spoken(" world", false)]));
    expect(onTranscript).toHaveBeenLastCalledWith("hello world");
    rec.emit(eventWith(1, [spoken("hello", true), spoken(" world", true)]));
    expect(onTranscript).toHaveBeenLastCalledWith("hello world");
  });

  it("resets accumulated finals for a fresh session", () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    const onTranscript = vi.fn();
    const input = new SpeechInput({ onTranscript });
    input.start();
    FakeRecognition.instances[0].emit(eventWith(0, [spoken("first", true)]));
    expect(onTranscript).toHaveBeenLastCalledWith("first");
    FakeRecognition.instances[0].end();
    expect(input.listening).toBe(false);

    input.start();
    FakeRecognition.instances[0].emit(eventWith(0, [spoken("second", true)]));
    expect(onTranscript).toHaveBeenLastCalledWith("second");
  });

  it("stop() ends the active session through the browser and onend flips listening", () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    const onEnd = vi.fn();
    const input = new SpeechInput({ onTranscript: vi.fn(), onEnd });
    input.start();
    const rec = FakeRecognition.instances[0];

    input.stop();
    expect(rec.stop).toHaveBeenCalledTimes(1);
    // The browser fires end asynchronously after the final flush.
    expect(input.listening).toBe(true);
    rec.end();
    expect(input.listening).toBe(false);
    expect(onEnd).toHaveBeenCalledTimes(1);
    // A stop after the session ended is a no-op.
    input.stop();
    expect(rec.stop).toHaveBeenCalledTimes(1);
  });

  it("abort() cancels immediately", () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    const input = new SpeechInput({ onTranscript: vi.fn() });
    input.start();
    input.abort();
    expect(FakeRecognition.instances[0].abort).toHaveBeenCalledTimes(1);
    expect(input.listening).toBe(false);
  });

  it("routes the browser error code to onError and leaves the session inactive", () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    const onError = vi.fn();
    const input = new SpeechInput({ onTranscript: vi.fn(), onError });
    input.start();
    FakeRecognition.instances[0].fail("not-allowed");
    expect(onError).toHaveBeenCalledWith("not-allowed");
    expect(input.listening).toBe(false);
  });
});
