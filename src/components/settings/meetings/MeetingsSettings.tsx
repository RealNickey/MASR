import React, { useCallback, useEffect, useMemo, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { readFile } from "@tauri-apps/plugin-fs";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  Check,
  Copy,
  Trash2,
  ChevronDown,
  ChevronUp,
  FileText,
  Upload,
  Mail,
  MessageSquare,
  Send,
} from "lucide-react";
import { toast } from "sonner";
import { open } from "@tauri-apps/plugin-dialog";
import {
  commands,
  events,
  type HistoryEntry,
  type HistoryUpdatePayload,
} from "@/bindings";
import { useOsType } from "@/hooks/useOsType";
import { useSettings } from "@/hooks/useSettings";
import { formatDateTime } from "@/utils/dateFormat";
import { AudioPlayer } from "../../ui/AudioPlayer";
import { LocalFileTranscriber } from "../../LocalFileTranscriber";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { Select } from "../../ui/Select";

const IconButton: React.FC<{
  onClick: () => void;
  title: string;
  disabled?: boolean;
  active?: boolean;
  children: React.ReactNode;
}> = ({ onClick, title, disabled, active, children }) => (
  <button
    onClick={onClick}
    disabled={disabled}
    className={`p-1.5 rounded-md flex items-center justify-center transition-colors cursor-pointer disabled:cursor-not-allowed disabled:text-text/20 ${
      active
        ? "text-logo-primary hover:text-logo-primary/80"
        : "text-text/50 hover:text-logo-primary"
    }`}
    title={title}
  >
    {children}
  </button>
);

export const MeetingsSettings: React.FC = () => {
  const osType = useOsType();
  const { settings, updateSetting, isUpdating } = useSettings();
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [transcriberFiles, setTranscriberFiles] = useState<string[]>([]);
  const [googleStatus, setGoogleStatus] = useState<any>(null);
  const [isConnecting, setIsConnecting] = useState(false);

  const refreshGoogleStatus = useCallback(async () => {
    try {
      const result = await commands.getGoogleIntegrationStatus();
      setGoogleStatus(result);
    } catch (error) {
      console.error("Failed to load Google integration status:", error);
    }
  }, []);

  const handleConnectGoogle = async (
    features: ("gmail_tasks" | "calendar")[],
  ) => {
    setIsConnecting(true);
    try {
      const result = await commands.connectGoogleFeatures(features as any);
      if (result.status === "ok") {
        toast.success("Google Services connected successfully!");
        await refreshGoogleStatus();
      } else {
        toast.error(
          "Failed to connect Google Services.",
        );
      }
    } catch (error) {
      console.error("Failed to connect Google services:", error);
      toast.error("Failed to connect Google Services.");
    } finally {
      setIsConnecting(false);
    }
  };

  const handleDisconnectGoogle = async (
    feature: "gmail_tasks" | "calendar",
  ) => {
    try {
      const result = await commands.disconnectGoogleFeature(feature as any);
      if (result.status === "ok") {
        toast.success("Google Services disconnected.");
        await refreshGoogleStatus();
      } else {
        toast.error(
          "Failed to disconnect Google Services.",
        );
      }
    } catch (error) {
      console.error("Failed to disconnect Google services:", error);
      toast.error("Failed to disconnect Google Services.");
    }
  };

  const handleUploadClick = async () => {
    try {
      const selected = await open({
        multiple: true,
        filters: [
          {
            name: "Audio",
            extensions: ["wav", "mp3", "m4a", "flac", "ogg"],
          },
        ],
      });
      if (selected) {
        const newFiles = Array.isArray(selected) ? selected : [selected];
        setTranscriberFiles(newFiles);
      }
    } catch (error) {
      console.error("Failed to open file dialog:", error);
    }
  };

  const loadMeetings = useCallback(async () => {
    setLoading(true);
    try {
      await refreshGoogleStatus();
      // Fetch a larger page size to ensure we grab recent meetings
      const result = await commands.getHistoryEntries(null, 100);
      if (result.status === "ok") {
        const meetingEntries = result.data.entries.filter(
          (e) =>
            e.post_process_prompt === "default_meeting_summary" ||
            e.post_process_prompt === "default_meeting_notes_with_actions",
        );
        setEntries(meetingEntries);
      }
    } catch (error) {
      console.error("Failed to load meeting entries:", error);
    } finally {
      setLoading(false);
    }
  }, [refreshGoogleStatus]);

  useEffect(() => {
    loadMeetings();
  }, [loadMeetings]);

  // Listen for new meeting entries added or updated
  useEffect(() => {
    const unlisten = events.historyUpdatePayload.listen((event) => {
      const payload: HistoryUpdatePayload = event.payload;
      if (payload.action === "added") {
        if (
          payload.entry.post_process_prompt === "default_meeting_summary" ||
          payload.entry.post_process_prompt ===
            "default_meeting_notes_with_actions"
        ) {
          setEntries((prev) => [payload.entry, ...prev]);
        }
      } else if (payload.action === "updated") {
        if (
          payload.entry.post_process_prompt === "default_meeting_summary" ||
          payload.entry.post_process_prompt ===
            "default_meeting_notes_with_actions"
        ) {
          setEntries((prev) =>
            prev.map((e) => (e.id === payload.entry.id ? payload.entry : e)),
          );
        }
      } else if (payload.action === "deleted") {
        setEntries((prev) => prev.filter((e) => e.id !== payload.id));
      }
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const getAudioUrl = useCallback(
    async (fileName: string) => {
      try {
        const result = await commands.getAudioFilePath(fileName);
        if (result.status === "ok") {
          if (osType === "linux") {
            const fileData = await readFile(result.data);
            const blob = new Blob([fileData], { type: "audio/wav" });
            return URL.createObjectURL(blob);
          }
          return convertFileSrc(result.data, "asset");
        }
        return null;
      } catch (error) {
        console.error("Failed to get audio file path:", error);
        return null;
      }
    },
    [osType],
  );

  const deleteMeetingEntry = async (id: number) => {
    setEntries((prev) => prev.filter((e) => e.id !== id));
    try {
      const result = await commands.deleteHistoryEntry(id);
      if (result.status !== "ok") {
        loadMeetings();
      }
    } catch (error) {
      console.error("Failed to delete meeting entry:", error);
      loadMeetings();
    }
  };

  let content: React.ReactNode;

  if (loading) {
    content = (
      <div className="px-4 py-8 text-center text-text/60">
        {"Loading meetings..."}
      </div>
    );
  } else if (entries.length === 0) {
    content = (
      <div className="px-4 py-8 text-center text-text/60">
        {"No meetings recorded yet. Start a meeting to generate summaries!"}
      </div>
    );
  } else {
    content = (
      <div className="divide-y divide-mid-gray/20">
        {entries.map((entry) => (
          <MeetingEntryComponent
            key={entry.id}
            entry={entry}
            getAudioUrl={getAudioUrl}
            deleteMeeting={deleteMeetingEntry}
            isGoogleConnected={!!googleStatus?.gmail_tasks_connected}
          />
        ))}
      </div>
    );
  }

  const googleUnavailable =
    googleStatus && !googleStatus.oauth_client_configured;
  const leadOptions = useMemo(
    () => [
      { value: "5", label: "5 minutes" },
    ],
    [],
  );

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <div className="bg-background border border-mid-gray/20 rounded-lg p-2 space-y-1">
        <ToggleSwitch
          checked={!!settings?.meeting_detection_enabled}
          onChange={(checked) =>
            updateSetting("meeting_detection_enabled", checked)
          }
          isUpdating={isUpdating("meeting_detection_enabled")}
          label={"Meeting Assistant"}
          description={"Detect active meeting windows and show a prompt to start meeting mode."}
          descriptionMode="inline"
          grouped
        />
        <ToggleSwitch
          checked={!!settings?.meeting_calendar_prompts_enabled}
          onChange={(checked) =>
            updateSetting("meeting_calendar_prompts_enabled", checked)
          }
          isUpdating={isUpdating("meeting_calendar_prompts_enabled")}
          disabled={!googleStatus?.calendar_connected || googleUnavailable}
          label={"Calendar Prompts"}
          description={
            googleUnavailable
              ? "Google Calendar is unavailable until a desktop OAuth client ID is configured for this build."
              : "Prompt five minutes before a Google Calendar meeting with conference details."
          }
          descriptionMode="inline"
          grouped
        />
        <div className="px-4 py-2">
          <p className="text-sm font-medium text-text">
            {"Prompt Lead Time"}
          </p>
          <p className="text-sm text-mid-gray mb-2">
            {"Version 1 uses a fixed reminder time for Calendar-based prompts."}
          </p>
          <Select
            value={String(settings?.meeting_prompt_lead_minutes ?? 5)}
            options={leadOptions}
            isClearable={false}
            onChange={(value) =>
              updateSetting("meeting_prompt_lead_minutes", Number(value ?? "5"))
            }
          />
        </div>
      </div>

      <div className="bg-background border border-mid-gray/20 rounded-lg p-4 space-y-4">
        <div className="flex items-center justify-between gap-3">
          <div className="space-y-1">
            <h3 className="text-sm font-semibold text-text">
              {"Google Services"}
            </h3>
            <p className="text-xs text-mid-gray">
              {googleUnavailable
                ? "Google Calendar is unavailable until a desktop OAuth client ID is configured for this build."
                : "Connect to send meeting follow-ups via Gmail and create tasks"}
            </p>
          </div>
        </div>

        <div className="grid gap-3 sm:grid-cols-2">
          <GoogleFeatureCard
            title={"Gmail and Tasks Follow-Ups"}
            description={"Optional follow-up email sending and task creation for completed meetings."}
            connected={!!googleStatus?.gmail_tasks_connected}
            disabled={!!googleUnavailable}
            connecting={isConnecting}
            connectClassName="google-connect-btn"
            disconnectClassName="google-disconnect-btn"
            onConnect={() => handleConnectGoogle(["gmail_tasks"])}
            onDisconnect={() => handleDisconnectGoogle("gmail_tasks")}
            labels={{
              connect: "Connect",
              disconnect: "Disconnect",
              connected: "Connected to Gmail & Google Tasks",
              disconnected: "Connect to send meeting follow-ups via Gmail and create tasks",
            }}
          />
          <GoogleFeatureCard
            title={"Google Calendar Prompts"}
            description={"Optional reminders for upcoming primary-calendar meetings with a join link."}
            connected={!!googleStatus?.calendar_connected}
            disabled={!!googleUnavailable}
            connecting={isConnecting}
            onConnect={() => handleConnectGoogle(["calendar"])}
            onDisconnect={() => handleDisconnectGoogle("calendar")}
            labels={{
              connect: "Connect",
              disconnect: "Disconnect",
              connected: "Connected for meeting reminders",
              disconnected: "Connect Google Calendar for meeting reminders",
            }}
          />
        </div>
      </div>

      <div className="space-y-2">
        <div className="px-4 flex items-center justify-between">
          <h2 className="text-xs font-medium text-mid-gray uppercase tracking-wide">
            {"Meetings"}
          </h2>
          <button
            onClick={handleUploadClick}
            className="flex items-center gap-1.5 text-xs font-medium text-logo-primary hover:text-logo-primary/80 transition-colors bg-logo-primary/10 px-2 py-1 rounded-md"
          >
            <Upload className="w-3.5 h-3.5" />
            {"Upload Audio"}
          </button>
        </div>
        <div className="bg-background border border-mid-gray/20 rounded-lg overflow-visible">
          {content}
        </div>
      </div>

      {transcriberFiles.length > 0 && (
        <LocalFileTranscriber
          initialFiles={transcriberFiles}
          onClose={() => setTranscriberFiles([])}
          onSuccess={() => {
            loadMeetings();
          }}
        />
      )}
    </div>
  );
};

interface GoogleFeatureCardProps {
  title: string;
  description: string;
  connected: boolean;
  disabled: boolean;
  connecting: boolean;
  onConnect: () => void;
  onDisconnect: () => void;
  labels: {
    connect: string;
    disconnect: string;
    connected: string;
    disconnected: string;
  };
  connectClassName?: string;
  disconnectClassName?: string;
}

const GoogleFeatureCard: React.FC<GoogleFeatureCardProps> = ({
  title,
  description,
  connected,
  disabled,
  connecting,
  onConnect,
  onDisconnect,
  labels,
  connectClassName,
  disconnectClassName,
}) => (
  <div className="rounded-lg border border-mid-gray/20 p-3 space-y-3">
    <div className="space-y-1">
      <p className="text-sm font-medium text-text">{title}</p>
      <p className="text-xs text-mid-gray">{description}</p>
      <p className="text-xs text-mid-gray">
        {connected ? labels.connected : labels.disconnected}
      </p>
    </div>
    {connected ? (
      <button
        onClick={onDisconnect}
        className={`px-3 py-1.5 text-xs font-medium bg-red-600/10 text-red-500 hover:bg-red-600/20 rounded-md transition-colors cursor-pointer ${disconnectClassName ?? ""}`}
      >
        {labels.disconnect}
      </button>
    ) : (
      <button
        onClick={onConnect}
        disabled={disabled || connecting}
        className={`px-3 py-1.5 text-xs font-medium bg-logo-primary text-white hover:bg-logo-primary/95 disabled:opacity-55 rounded-md transition-colors cursor-pointer ${connectClassName ?? ""}`}
      >
        {connecting ? labels.connect : labels.connect}
      </button>
    )}
  </div>
);

export interface MeetingEntryProps {
  entry: HistoryEntry;
  getAudioUrl: (fileName: string) => Promise<string | null>;
  deleteMeeting: (id: number) => Promise<void>;
  isGoogleConnected: boolean;
}

export const MeetingEntryComponent: React.FC<MeetingEntryProps> = ({
  entry,
  getAudioUrl,
  deleteMeeting,
  isGoogleConnected,
}) => {
  const [showSummaryCopied, setShowSummaryCopied] = useState(false);
  const [showTranscriptCopied, setShowTranscriptCopied] = useState(false);
  const [expandTranscript, setExpandTranscript] = useState(false);

  const [showChat, setShowChat] = useState(false);
  const [chatQuestion, setChatQuestion] = useState("");
  const [chatAnswer, setChatAnswer] = useState("");
  const [isAsking, setIsAsking] = useState(false);

  const handleAskQuestion = async () => {
    if (!chatQuestion.trim() || isAsking) return;

    setIsAsking(true);
    try {
      const result = await commands.askMeetingQuestion(
        entry.transcription_text,
        chatQuestion,
      );
      if (result.status === "ok") {
        setChatAnswer(result.data);
      } else {
        toast.error(
          "Failed to get an answer from the AI. Please check your connection and LLM settings.",
        );
      }
    } catch (error) {
      console.error("Failed to ask meeting question:", error);
      toast.error("Failed to get an answer from the AI. Please check your connection and LLM settings.");
    } finally {
      setIsAsking(false);
    }
  };

  const [showFollowUpDialog, setShowFollowUpDialog] = useState(false);
  const [recipientsInput, setRecipientsInput] = useState("");
  const [emailsError, setEmailsError] = useState("");
  const [isSending, setIsSending] = useState(false);

  const validateEmails = (input: string): string[] | null => {
    const trimmed = input.trim();
    if (!trimmed) {
      setEmailsError(
        "Recipient email is required.",
      );
      return null;
    }
    const emails = trimmed.split(/[\s,]+/).filter(Boolean);
    const emailRegex = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
    for (const email of emails) {
      if (!emailRegex.test(email)) {
        setEmailsError(
          `Invalid email address: ${(email)}` ||
            `Invalid email address: ${email}`,
        );
        return null;
      }
    }
    setEmailsError("");
    return emails;
  };

  const handleSendFollowUp = async () => {
    const emails = validateEmails(recipientsInput);
    if (!emails) {
      return;
    }

    setIsSending(true);
    try {
      let summary = "";
      let actionItems: string[] = [];
      try {
        const parsed = JSON.parse(entry.post_processed_text || "");
        summary = parsed.summary || "";
        actionItems = parsed.action_items || [];
      } catch (e) {
        summary = entry.post_processed_text || entry.transcription_text;
        actionItems = [];
      }

      const result = await commands.sendMeetingFollowUp(
        emails,
        summary,
        actionItems,
      );

      if (result.status === "ok") {
        toast.success(
          "Follow-up email and tasks sent successfully!",
        );
        setShowFollowUpDialog(false);
        setRecipientsInput("");
      } else {
        toast.error(
          "Failed to send follow-up email/tasks.",
        );
      }
    } catch (error: any) {
      console.error("Failed to send meeting follow-up:", error);
      toast.error(
        "Failed to send follow-up email/tasks.",
      );
    } finally {
      setIsSending(false);
    }
  };

  const handleLoadAudio = useCallback(
    () => getAudioUrl(entry.file_name),
    [getAudioUrl, entry.file_name],
  );

  const copySummary = async () => {
    const text = entry.post_processed_text || entry.transcription_text;
    try {
      await navigator.clipboard.writeText(text);
      setShowSummaryCopied(true);
      setTimeout(() => setShowSummaryCopied(false), 2000);
    } catch (error) {
      console.error("Failed to copy summary:", error);
    }
  };

  const copyTranscript = async () => {
    try {
      await navigator.clipboard.writeText(entry.transcription_text);
      setShowTranscriptCopied(true);
      setTimeout(() => setShowTranscriptCopied(false), 2000);
    } catch (error) {
      console.error("Failed to copy transcript:", error);
    }
  };

  const handleDelete = async () => {
    try {
      await deleteMeeting(entry.id);
    } catch (error) {
      console.error("Failed to delete meeting:", error);
      toast.error("Failed to delete entry. Please try again.");
    }
  };

  const formattedDate = formatDateTime(String(entry.timestamp), "en");

  const displaySummary = React.useMemo(() => {
    if (
      entry.post_process_prompt === "default_meeting_notes_with_actions" &&
      entry.post_processed_text
    ) {
      try {
        const parsed = JSON.parse(entry.post_processed_text);
        let summary = parsed.summary || "";
        if (parsed.action_items && parsed.action_items.length > 0) {
          const actionMarkdown = parsed.action_items
            .map((item: string) => `- [ ] ${item}`)
            .join("\n");
          summary += `\n\n## Action Items\n${actionMarkdown}`;
        }
        return summary || entry.post_processed_text;
      } catch (e) {
        return entry.post_processed_text;
      }
    }
    return entry.post_processed_text || entry.transcription_text;
  }, [
    entry.post_process_prompt,
    entry.post_processed_text,
    entry.transcription_text,
  ]);

  return (
    <div className="px-4 py-4 flex flex-col gap-4">
      <div className="flex justify-between items-center border-b border-mid-gray/10 pb-2">
        <div>
          <p className="text-sm font-semibold text-text">{formattedDate}</p>
        </div>
        <div className="flex items-center gap-2">
          {isGoogleConnected && (
            <button
              onClick={() => setShowFollowUpDialog(true)}
              className="flex items-center gap-1.5 text-xs font-medium text-logo-primary hover:text-logo-primary/80 transition-colors bg-logo-primary/10 px-2 py-1 rounded-md cursor-pointer send-via-google-btn"
              title={"Send via Google"}
            >
              <Mail width={14} height={14} />
              {"Send via Google"}
            </button>
          )}
          <IconButton
            onClick={handleDelete}
            title={"Delete entry"}
          >
            <Trash2 width={16} height={16} />
          </IconButton>
        </div>
      </div>

      <div className="space-y-2">
        <div className="flex items-center justify-between">
          <h4 className="text-xs font-semibold uppercase tracking-wider text-mid-gray">
            {"Meeting Summary"}
          </h4>
          <IconButton
            onClick={copySummary}
            title={"Copy transcription to clipboard"}
          >
            {showSummaryCopied ? (
              <Check width={14} height={14} />
            ) : (
              <Copy width={14} height={14} />
            )}
          </IconButton>
        </div>
        <div className="p-3 bg-mid-gray/5 rounded-md border border-mid-gray/10 text-sm text-text/90 select-text markdown-summary">
          {entry.post_processed_text ? (
            <ReactMarkdown remarkPlugins={[remarkGfm]}>
              {displaySummary}
            </ReactMarkdown>
          ) : entry.transcription_text === "" ? (
            <div className="flex items-center gap-2 text-mid-gray py-1">
              <span className="w-3.5 h-3.5 border-2 border-logo-primary border-t-transparent rounded-full animate-spin"></span>
              <span>{"Processing"}</span>
            </div>
          ) : (
            "Failed to generate meeting summary."
          )}
        </div>
      </div>

      <div className="space-y-2">
        <button
          onClick={() => setExpandTranscript(!expandTranscript)}
          className="flex items-center justify-between w-full text-left cursor-pointer hover:bg-mid-gray/5 p-1 rounded transition-colors"
        >
          <div className="flex items-center gap-2">
            <FileText className="w-4 h-4 text-mid-gray" />
            <span className="text-xs font-semibold uppercase tracking-wider text-mid-gray">
              {"Full Transcript"}
            </span>
          </div>
          {expandTranscript ? (
            <ChevronUp className="w-4 h-4 text-mid-gray" />
          ) : (
            <ChevronDown className="w-4 h-4 text-mid-gray" />
          )}
        </button>

        {expandTranscript && (
          <div className="space-y-2 pt-1">
            <div className="flex justify-end">
              <IconButton
                onClick={copyTranscript}
                title={"Copy transcription to clipboard"}
              >
                {showTranscriptCopied ? (
                  <Check width={14} height={14} />
                ) : (
                  <Copy width={14} height={14} />
                )}
              </IconButton>
            </div>
            <div className="p-3 bg-mid-gray/5 rounded-md border border-mid-gray/10 text-sm text-text/80 whitespace-pre-wrap select-text">
              {entry.transcription_text}
            </div>
          </div>
        )}
      </div>

      <AudioPlayer onLoadRequest={handleLoadAudio} className="w-full mt-1" />

      <div className="space-y-2">
        <button
          onClick={() => setShowChat(!showChat)}
          className="flex items-center justify-between w-full text-left cursor-pointer hover:bg-mid-gray/5 p-1 rounded transition-colors"
        >
          <div className="flex items-center gap-2">
            <MessageSquare className="w-4 h-4 text-mid-gray" />
            <span className="text-xs font-semibold uppercase tracking-wider text-mid-gray">
              {"Chat with Meeting"}
            </span>
          </div>
          {showChat ? (
            <ChevronUp className="w-4 h-4 text-mid-gray" />
          ) : (
            <ChevronDown className="w-4 h-4 text-mid-gray" />
          )}
        </button>

        {showChat && (
          <div className="space-y-3 pt-1">
            <div className="flex gap-2">
              <input
                type="text"
                value={chatQuestion}
                onChange={(e) => setChatQuestion(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleAskQuestion();
                }}
                disabled={isAsking}
                placeholder={"Ask something about the meeting..."}
                className="flex-1 px-3 py-1.5 bg-mid-gray/5 border border-mid-gray/20 rounded-md text-sm text-text focus:outline-none focus:border-logo-primary"
              />
              <button
                onClick={handleAskQuestion}
                disabled={isAsking || !chatQuestion.trim()}
                className="px-3 py-1.5 bg-logo-primary text-white rounded-md hover:bg-logo-primary/90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
              >
                {isAsking ? (
                  <span className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin inline-block"></span>
                ) : (
                  <Send className="w-4 h-4" />
                )}
              </button>
            </div>

            {chatAnswer && (
              <div className="p-3 bg-mid-gray/5 rounded-md border border-mid-gray/10 text-sm text-text/90 select-text markdown-answer">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>
                  {chatAnswer}
                </ReactMarkdown>
              </div>
            )}
          </div>
        )}
      </div>

      {showFollowUpDialog && (
        <div className="fixed inset-0 bg-black/50 flex items-center justify-center z-50 p-4 follow-up-dialog">
          <div className="bg-background border border-mid-gray/20 rounded-lg max-w-md w-full p-6 space-y-4 shadow-xl">
            <h3 className="text-base font-semibold text-text">
              {"Send Meeting Follow-Up"}
            </h3>

            <div className="space-y-1.5">
              <label className="text-xs font-semibold uppercase tracking-wider text-mid-gray">
                {"Recipient Emails (comma or space separated)"}
              </label>
              <textarea
                value={recipientsInput}
                onChange={(e) => {
                  setRecipientsInput(e.target.value);
                  setEmailsError("");
                }}
                disabled={isSending}
                placeholder={"e.g., alex@example.com, john@example.com"}
                className="w-full h-20 px-3 py-2 bg-mid-gray/5 border border-mid-gray/20 rounded-md text-sm text-text focus:outline-none focus:border-logo-primary resize-none recipients-input"
              />
              {emailsError && (
                <p className="text-xs font-medium text-red-500 error-message">
                  {emailsError}
                </p>
              )}
            </div>

            <div className="flex justify-end gap-2 pt-2 border-t border-mid-gray/10">
              <button
                onClick={() => {
                  setShowFollowUpDialog(false);
                  setRecipientsInput("");
                  setEmailsError("");
                }}
                disabled={isSending}
                className="px-4 py-2 text-xs font-medium text-text hover:bg-mid-gray/10 rounded-md transition-colors cursor-pointer cancel-btn"
              >
                {"Cancel"}
              </button>
              <button
                onClick={handleSendFollowUp}
                disabled={isSending}
                className="px-4 py-2 text-xs font-medium bg-logo-primary text-white hover:bg-logo-primary/95 disabled:opacity-55 rounded-md transition-colors cursor-pointer flex items-center gap-1.5 send-btn"
              >
                {isSending ? (
                  <>
                    <span className="w-3.5 h-3.5 border-2 border-white border-t-transparent rounded-full animate-spin"></span>
                    {"Sending..."}
                  </>
                ) : (
                  "Send"
                )}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};
