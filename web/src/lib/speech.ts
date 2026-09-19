/**
 * §4.8 (D-049) voice input — the first-milestone「能说」.
 *
 * The default path is the platform keyboard's own dictation: that needs no
 * code from Remuda and never touches the network through us. The Web Speech
 * API is only an *enhancement*: where the browser implements
 * `SpeechRecognition` AND the user has switched it on in settings, the
 * composer gets a microphone button. Remuda never records or uploads audio
 * and does no cloud transcription itself — the vendor's browser owns the
 * audio stream — and recognition results are written into the composer draft
 * only. No path here ever sends a message.
 *
 * iOS Safari has no `SpeechRecognition` (WebKit never implemented it); on
 * iPhone the microphone button is not rendered and keyboard dictation is the
 * whole story.
 */

const VOICE_PREF_KEY = "runtime.voice-input.v1";

/**
 * Capability probe, evaluated against the live window each call. Exact shape
 * required by ui-spec §4.8: the prefixed WebKit constructor ships in Chrome
 * / Edge / Android Chrome, the unprefixed one wherever the standard landed.
 */
export function speechRecognitionSupported(): boolean {
  return (
    typeof window !== "undefined" &&
    ("webkitSpeechRecognition" in window || "SpeechRecognition" in window)
  );
}

/**
 * Per-device opt-in (localStorage, same persistence shape as the other
 * device prefs). Default OFF: absent key, storage denied and any non-"1"
 * value all read as off.
 */
export function readVoiceInputEnabled(): boolean {
  try {
    return localStorage.getItem(VOICE_PREF_KEY) === "1";
  } catch {
    return false;
  }
}

export function writeVoiceInputEnabled(enabled: boolean): void {
  try {
    if (enabled) localStorage.setItem(VOICE_PREF_KEY, "1");
    else localStorage.removeItem(VOICE_PREF_KEY);
  } catch {
    /* storage denied — callers re-read the stored value and roll the UI back */
  }
}

/** The smallest structural slice of a SpeechRecognition alternative result. */
export type SpeechAlternativeLike = { transcript: string };

export type SpeechResultLike = {
  readonly isFinal: boolean;
  readonly 0: SpeechAlternativeLike;
};

export type SpeechRecognitionEventLike = {
  readonly resultIndex: number;
  readonly results: ArrayLike<SpeechResultLike>;
};

export type SpeechRecognitionErrorEventLike = { error?: string };

/** A `SpeechRecognition` instance shaped to the surface this file touches. */
export type SpeechRecognitionLike = {
  lang: string;
  interimResults: boolean;
  continuous: boolean;
  start(): void;
  stop(): void;
  abort(): void;
  onresult: ((event: SpeechRecognitionEventLike) => void) | null;
  onerror: ((event: SpeechRecognitionErrorEventLike) => void) | null;
  onend: (() => void) | null;
};

type SpeechRecognitionCtor = new () => SpeechRecognitionLike;

function speechRecognitionCtor(): SpeechRecognitionCtor | null {
  if (typeof window === "undefined") return null;
  const scope = window as Window & {
    SpeechRecognition?: SpeechRecognitionCtor;
    webkitSpeechRecognition?: SpeechRecognitionCtor;
  };
  return scope.SpeechRecognition ?? scope.webkitSpeechRecognition ?? null;
}

export type SpeechHandlers = {
  /**
   * The full dictated text so far (committed finals plus the current interim
   * span), fired on every result event. The caller owns where it lands.
   */
  onTranscript: (transcript: string) => void;
  /** Browser-reported failure code ("not-allowed", "no-speech", …). */
  onError?: (code: string) => void;
  /** The recognition session ended (auto-stop after silence, stop(), error). */
  onEnd?: () => void;
};

/**
 * Thin wrapper around the vendor SpeechRecognition: fixes the config this
 * feature needs and folds the result list into one cumulative transcript.
 * Construct it in response to a user gesture so `start()` happens inside the
 * browser's gesture window (mic permission prompts require one).
 */
export class SpeechInput {
  private readonly recognition: SpeechRecognitionLike | null;
  private readonly handlers: SpeechHandlers;
  private finals = "";
  private active = false;

  constructor(handlers: SpeechHandlers) {
    this.handlers = handlers;
    const Ctor = speechRecognitionCtor();
    const recognition = Ctor ? new Ctor() : null;
    this.recognition = recognition;
    if (!recognition) return;
    // Interim results make the textarea fill while the user is still
    // speaking; non-continuous lets the browser end the session on its own
    // after a pause, which keeps the feature a single button.
    recognition.interimResults = true;
    recognition.continuous = false;
    recognition.onresult = (event) => {
      let interim = "";
      for (let i = event.resultIndex; i < event.results.length; i += 1) {
        const result = event.results[i];
        const transcript = result[0]?.transcript ?? "";
        if (result.isFinal) this.finals += transcript;
        else interim += transcript;
      }
      this.handlers.onTranscript(this.finals + interim);
    };
    recognition.onerror = (event) => {
      this.active = false;
      this.handlers.onError?.(event.error ?? "unknown");
    };
    recognition.onend = () => {
      this.active = false;
      this.handlers.onEnd?.();
    };
  }

  /** False on the iOS Safari path, where the constructor does not exist. */
  get available(): boolean {
    return this.recognition !== null;
  }

  get listening(): boolean {
    return this.active;
  }

  start(): void {
    if (!this.recognition) {
      throw new Error("SpeechRecognition is not available in this browser");
    }
    if (this.active) return;
    this.finals = "";
    this.recognition.start();
    this.active = true;
  }

  /** Finish the current utterance and let a final transcript flush. */
  stop(): void {
    if (!this.recognition || !this.active) return;
    this.recognition.stop();
  }

  /** Cancel immediately; no final transcript is delivered. */
  abort(): void {
    if (!this.recognition) return;
    this.recognition.abort();
    this.active = false;
  }
}
