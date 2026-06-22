import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Check, Circle, Minus, Square, Video } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { MeetingOverlayShowPayload } from "@/bindings";
import { commands } from "@/bindings";
import meetIcon from "@/assets/google-meet-icon.png";
import { syncLanguageFromSettings } from "@/i18n";

type MeetingPromptSource = "LocalDetection" | "GoogleCalendar";
type MeetingOverlayState = "idle" | "suggestion" | "recording" | "recorded";

interface MeetingPromptPayload {
  provider: string;
  title: string;
  source: MeetingPromptSource;
  start_time: string;
  join_url: string | null;
}

declare global {
  interface Window {
    __MEETING_PROMPT_TIMEOUT_MS__?: number;
  }
}

const DEFAULT_TIMEOUT_MS = 10_000;
const RECORDED_CLOSE_MS = 520;
const MEETING_OVERLAY_STATES = new Set<MeetingOverlayState>([
  "idle",
  "suggestion",
  "recording",
  "recorded",
]);

function getPromptTimeoutMs() {
  return window.__MEETING_PROMPT_TIMEOUT_MS__ ?? DEFAULT_TIMEOUT_MS;
}

function toOverlayState(value: string): MeetingOverlayState {
  return MEETING_OVERLAY_STATES.has(value as MeetingOverlayState)
    ? (value as MeetingOverlayState)
    : "idle";
}

function ProviderBadge({ provider }: { provider: string }) {
  if (provider === "Google Meet") {
    return (
      <img
        src={meetIcon}
        alt={provider}
        className="meeting-prompt-provider-icon"
      />
    );
  }

  return (
    <div className="meeting-prompt-provider-fallback" aria-hidden="true">
      <Video size={16} strokeWidth={1.8} />
    </div>
  );
}

export default function MeetingPrompt() {
  const { t } = useTranslation();
  const [payload, setPayload] = useState<MeetingPromptPayload | null>(null);
  const [overlayState, setOverlayState] = useState<MeetingOverlayState>("idle");
  const [isVisible, setIsVisible] = useState(false);
  const [progress, setProgress] = useState(1);
  const [levels, setLevels] = useState<number[]>(Array(9).fill(0));
  const smoothedLevelsRef = useRef<number[]>(Array(9).fill(0));
  const deadlineRef = useRef<number | null>(null);
  const timeoutIdRef = useRef<number | null>(null);
  const intervalIdRef = useRef<number | null>(null);
  const clearStateTimerRef = useRef<number | null>(null);
  const recordedCloseTimerRef = useRef<number | null>(null);

  const clearPromptTimer = useCallback(() => {
    if (timeoutIdRef.current !== null) {
      window.clearTimeout(timeoutIdRef.current);
      timeoutIdRef.current = null;
    }
    if (intervalIdRef.current !== null) {
      window.clearInterval(intervalIdRef.current);
      intervalIdRef.current = null;
    }
    deadlineRef.current = null;
  }, []);

  const clearRecordedTimer = useCallback(() => {
    if (recordedCloseTimerRef.current !== null) {
      window.clearTimeout(recordedCloseTimerRef.current);
      recordedCloseTimerRef.current = null;
    }
  }, []);

  const clearStateResetTimer = useCallback(() => {
    if (clearStateTimerRef.current !== null) {
      window.clearTimeout(clearStateTimerRef.current);
      clearStateTimerRef.current = null;
    }
  }, []);

  const resetOverlay = useCallback(() => {
    clearPromptTimer();
    clearRecordedTimer();
    clearStateResetTimer();
    setIsVisible(false);
    setOverlayState("idle");
    setPayload(null);
    setProgress(1);
    setLevels(Array(9).fill(0));
    smoothedLevelsRef.current = Array(9).fill(0);
  }, [clearPromptTimer, clearRecordedTimer, clearStateResetTimer]);

  const dismissPrompt = useCallback(
    async (activePayload: MeetingPromptPayload | null) => {
      clearPromptTimer();
      if (activePayload) {
        await commands.dismissMeetingPrompt(activePayload);
      } else {
        await commands.closeMeetingPrompt();
      }
    },
    [clearPromptTimer],
  );

  useEffect(() => {
    let mounted = true;
    let cleanupFns: Array<() => void> = [];

    void (async () => {
      try {
        const unlistenShow = await listen<MeetingOverlayShowPayload>(
          "meeting-overlay-show",
          (event) => {
            clearStateResetTimer();
            clearRecordedTimer();
            setPayload(event.payload.prompt);
            setOverlayState(toOverlayState(event.payload.state));
            setIsVisible(true);
          },
        );

        const unlistenPrompt = await listen<MeetingPromptPayload>(
          "meeting-prompt-show",
          (event) => {
            clearStateResetTimer();
            clearRecordedTimer();
            setPayload(event.payload);
            setOverlayState("suggestion");
            setIsVisible(true);
          },
        );

        const unlistenHide = await listen("hide-meeting-overlay", () => {
          setIsVisible(false);
          clearPromptTimer();
          clearRecordedTimer();
          clearStateResetTimer();
          clearStateTimerRef.current = window.setTimeout(() => {
            setOverlayState("idle");
            setPayload(null);
            setProgress(1);
          }, 80);
        });

        const unlistenLevels = await listen<number[]>("mic-level", (event) => {
          const next = event.payload.slice(0, 9);
          const smoothed = smoothedLevelsRef.current.map((prev, index) => {
            const target = next[index] || 0;
            return prev * 0.7 + target * 0.3;
          });
          smoothedLevelsRef.current = smoothed;
          setLevels(smoothed);
        });

        cleanupFns = [
          unlistenShow,
          unlistenPrompt,
          unlistenHide,
          unlistenLevels,
        ];

        if (!mounted) {
          cleanupFns.forEach((fn) => fn());
          return;
        }

        await syncLanguageFromSettings();
        await commands.meetingOverlayReady();

        const activeOverlay = await commands.getActiveMeetingOverlay();
        if (mounted && activeOverlay) {
          setPayload(activeOverlay.prompt);
          setOverlayState(toOverlayState(activeOverlay.state));
          setIsVisible(true);
          return;
        }

        const activePrompt = await commands.getActiveMeetingPrompt();
        if (mounted && activePrompt) {
          setPayload(activePrompt);
          setOverlayState("suggestion");
          setIsVisible(true);
        }
      } catch (error) {
        console.error("Failed to initialize meeting overlay:", error);
      }
    })();

    return () => {
      mounted = false;
      clearPromptTimer();
      clearRecordedTimer();
      clearStateResetTimer();
      cleanupFns.forEach((fn) => fn());
    };
  }, [clearPromptTimer, clearRecordedTimer, clearStateResetTimer]);

  useEffect(() => {
    clearPromptTimer();

    if (overlayState !== "suggestion" || !payload) {
      setProgress(1);
      return;
    }

    const timeoutMs = getPromptTimeoutMs();
    deadlineRef.current = Date.now() + timeoutMs;
    setProgress(1);

    intervalIdRef.current = window.setInterval(() => {
      if (!deadlineRef.current) {
        return;
      }
      const remaining = Math.max(0, deadlineRef.current - Date.now());
      setProgress(remaining / timeoutMs);
    }, 50);

    timeoutIdRef.current = window.setTimeout(() => {
      void dismissPrompt(payload);
    }, timeoutMs);

    return clearPromptTimer;
  }, [clearPromptTimer, dismissPrompt, overlayState, payload]);

  useEffect(() => {
    clearRecordedTimer();

    if (overlayState !== "recorded") {
      return;
    }

    recordedCloseTimerRef.current = window.setTimeout(() => {
      void commands.closeMeetingPrompt();
    }, RECORDED_CLOSE_MS);

    return clearRecordedTimer;
  }, [clearRecordedTimer, overlayState]);

  const handleDismiss = async () => {
    await dismissPrompt(payload);
  };

  const handleRecord = async () => {
    clearPromptTimer();
    if (document.activeElement instanceof HTMLElement) {
      document.activeElement.blur();
    }
    setIsVisible(false);
    await commands.startMeetingRecordingFromPrompt();
    resetOverlay();
  };

  const handleStop = async () => {
    clearPromptTimer();
    clearRecordedTimer();
    setOverlayState("recorded");
    setIsVisible(true);
    await commands.stopMeetingRecordingFromOverlay();
  };

  const handleMinimize = async () => {
    clearPromptTimer();
    await commands.hideMeetingOverlay();
  };

  const openMeeting = async () => {
    if (!payload?.join_url) {
      return;
    }

    clearPromptTimer();
    await openUrl(payload.join_url);
    await commands.dismissMeetingPrompt(payload);
  };

  if (!payload || overlayState === "idle") {
    return null;
  }

  const showJoinAction =
    overlayState === "suggestion" &&
    payload.source === "GoogleCalendar" &&
    Boolean(payload.join_url);
  const providerLabel = payload.provider;

  return (
    <div className="meeting-prompt-root">
      <section
        className={`meeting-prompt-card meeting-state-${overlayState} ${
          isVisible ? "is-visible" : "is-leaving"
        }`}
        aria-label={providerLabel}
      >
        {overlayState === "suggestion" && (
          <button
            className="meeting-prompt-dismiss"
            onClick={handleDismiss}
            type="button"
            aria-label={t("settings.meetings.assistant.dismiss")}
            title={t("settings.meetings.assistant.dismiss")}
          >
            {t("settings.meetings.assistant.dismiss")}
          </button>
        )}

        {(overlayState === "recording" || overlayState === "recorded") && (
          <div className="meeting-recording-controls">
            {overlayState === "recording" && (
              <>
                <button
                  className="meeting-recording-control"
                  onClick={handleMinimize}
                  type="button"
                  aria-label={t("settings.meetings.assistant.minimize")}
                  title={t("settings.meetings.assistant.minimize")}
                >
                  <Minus size={14} strokeWidth={2.2} />
                </button>
                <button
                  className="meeting-recording-control danger"
                  onClick={handleStop}
                  type="button"
                  aria-label={t("settings.meetings.assistant.stopRecording")}
                  title={t("settings.meetings.assistant.stopRecording")}
                >
                  <Square size={11} fill="currentColor" strokeWidth={0} />
                </button>
              </>
            )}
          </div>
        )}

        <div className="meeting-prompt-shell">
          <div className="meeting-prompt-provider-cell" aria-hidden="true">
            {overlayState === "recorded" ? (
              <div className="meeting-recorded-mark">
                <Check size={16} strokeWidth={2.2} />
              </div>
            ) : (
              <ProviderBadge provider={providerLabel} />
            )}
          </div>

          <div className="meeting-prompt-copy">
            <div className="meeting-prompt-eyebrow">
              {overlayState === "suggestion" &&
                t("settings.meetings.assistant.detectedTitle")}
              {overlayState === "recording" &&
                t("settings.meetings.activeIndicator")}
              {overlayState === "recorded" &&
                t("settings.meetings.assistant.recordedTitle")}
            </div>
            <div className="meeting-prompt-provider">{providerLabel}</div>
            {overlayState !== "suggestion" && (
              <div className="meeting-prompt-detail">
                {payload.title || t("settings.meetings.assistant.recordingSubtle")}
              </div>
            )}
          </div>

          <div className="meeting-prompt-side">
            {overlayState === "suggestion" ? (
              <div className="meeting-prompt-actions">
                {showJoinAction && (
                  <button
                    className="meeting-prompt-btn secondary"
                    onClick={openMeeting}
                    type="button"
                  >
                    {t("settings.meetings.assistant.openMeeting")}
                  </button>
                )}
                <button
                  className="meeting-prompt-btn primary"
                  onClick={handleRecord}
                  type="button"
                >
                  <Circle size={10} fill="currentColor" strokeWidth={0} />
                  <span>{t("settings.meetings.assistant.startRecording")}</span>
                </button>
              </div>
            ) : overlayState === "recording" ? (
              <div className="meeting-audio-bars" aria-hidden="true">
                {levels.map((value, index) => (
                  <span
                    key={index}
                    className="meeting-audio-bar"
                    style={{
                      height: `${Math.min(24, 6 + Math.pow(value, 0.72) * 18)}px`,
                      opacity: Math.max(0.32, value * 1.9),
                    }}
                  />
                ))}
              </div>
            ) : (
              <div className="meeting-recorded-badge">
                {t("settings.meetings.assistant.recordedShort")}
              </div>
            )}
          </div>
        </div>

        {overlayState === "suggestion" && (
          <div className="meeting-prompt-progress" aria-hidden="true">
            <div
              className="meeting-prompt-progress-bar"
              style={{ transform: `scaleX(${progress})` }}
            />
          </div>
        )}
      </section>
    </div>
  );
}
