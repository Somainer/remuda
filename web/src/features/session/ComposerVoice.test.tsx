import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import type {
  SpeechRecognitionEventLike,
  SpeechRecognitionLike,
  SpeechResultLike,
} from "../../lib/speech";

const PREF_KEY = "runtime.voice-input.v1";

/** Stands in for Chrome's webkitSpeechRecognition; the wrapper drives it. */
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

  end() {
    this.onend?.();
  }
}

function spoken(transcript: string, isFinal: boolean): SpeechResultLike {
  return { isFinal, 0: { transcript } };
}

beforeEach(() => {
  localStorage.clear();
  FakeRecognition.instances.length = 0;
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("Composer voice input (ui-spec §4.8, m-voice)", () => {
  it("renders no microphone when SpeechRecognition is missing, even if the pref is on", () => {
    localStorage.setItem(PREF_KEY, "1");
    render(<Composer instanceId="ins_voice_ios" mobile onSend={vi.fn()} />);
    expect(screen.queryByTestId("composer-voice")).toBeNull();
  });

  it("renders no microphone when the capability exists but the pref is off (default)", () => {
    vi.stubGlobal("webkitSpeechRecognition", FakeRecognition);
    render(<Composer instanceId="ins_voice_off" mobile onSend={vi.fn()} />);
    expect(screen.queryByTestId("composer-voice")).toBeNull();
  });

  it("never renders the microphone on desktop, capability and pref notwithstanding", () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    localStorage.setItem(PREF_KEY, "1");
    render(<Composer instanceId="ins_voice_desktop" mobile={false} onSend={vi.fn()} />);
    // Zero desktop DOM change: the enhancement is phone-only.
    expect(screen.queryByTestId("composer-voice")).toBeNull();
  });

  it("writes a recognised phrase into the textarea and never calls onSend", async () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    localStorage.setItem(PREF_KEY, "1");
    const onSend = vi.fn();
    render(<Composer instanceId="ins_voice_dictation" mobile onSend={onSend} />);

    const mic = screen.getByTestId("composer-voice");
    expect(mic).toHaveAttribute("data-listening", "0");
    const area = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    // Start mid-draft: "ab", caret between the two chars.
    await userEvent.click(area);
    await userEvent.keyboard("ab");
    area.setSelectionRange(1, 1);

    await userEvent.click(mic);
    expect(FakeRecognition.instances[0].start).toHaveBeenCalledTimes(1);
    expect(FakeRecognition.instances[0].interimResults).toBe(true);
    expect(mic).toHaveAttribute("data-listening", "1");
    expect(screen.getByTestId("composer-voice-hint")).toBeInTheDocument();

    const rec = FakeRecognition.instances[0];
    rec.emit({ resultIndex: 0, results: [spoken("帮", false)] });
    await waitFor(() => expect(area).toHaveValue("a帮b"));
    rec.emit({ resultIndex: 0, results: [spoken("帮我总结", false)] });
    await waitFor(() => expect(area).toHaveValue("a帮我总结b"));
    // The final result replaces the interim span, it does not append.
    rec.emit({ resultIndex: 0, results: [spoken("帮我总结", true)] });
    await waitFor(() => expect(area).toHaveValue("a帮我总结b"));

    // §4.8: no path from recognition to send — the draft is the only output.
    expect(onSend).not.toHaveBeenCalled();
    expect(screen.getByTestId("composer-send")).toBeEnabled();

    // Stopping lets the browser flush its final result and rests the button.
    await userEvent.click(mic);
    expect(rec.stop).toHaveBeenCalledTimes(1);
    rec.end();
    await waitFor(() => expect(mic).toHaveAttribute("data-listening", "0"));
    expect(screen.queryByTestId("composer-voice-hint")).toBeNull();
    expect(onSend).not.toHaveBeenCalled();
  });

  it("sends nothing when recognition ends on an empty draft", async () => {
    vi.stubGlobal("SpeechRecognition", FakeRecognition);
    localStorage.setItem(PREF_KEY, "1");
    const onSend = vi.fn();
    render(<Composer instanceId="ins_voice_empty" mobile onSend={onSend} />);

    await userEvent.click(screen.getByTestId("composer-voice"));
    FakeRecognition.instances[0].end();
    expect(onSend).not.toHaveBeenCalled();
    expect(screen.getByTestId("composer-input")).toHaveValue("");
  });
});
