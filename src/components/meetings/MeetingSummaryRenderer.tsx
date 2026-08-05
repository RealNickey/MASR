import React, { useMemo, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Check } from "lucide-react";
import type {
  HistoryEntry,
  SpeakerSegment,
  TranscriptSegment,
} from "@/bindings";

const CITATION_PATTERN = /\[\[cite:([A-Za-z0-9_-]+)\]\]/g;

export type MeetingActionItem = {
  text: string;
  owner?: string;
  dueDate?: string;
  checked: boolean;
  evidenceIds: string[];
};

type JsonRecord = Record<string, unknown>;

type Citation = {
  id: string;
  text: string;
  startMs: number;
  endMs: number;
  speaker?: string;
  source?: string;
};

function asRecord(value: unknown): JsonRecord | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as JsonRecord)
    : null;
}

function firstString(value: JsonRecord, keys: string[]): string | undefined {
  for (const key of keys) {
    const candidate = value[key];
    if (typeof candidate === "string" && candidate.trim()) {
      return candidate.trim();
    }
    if (typeof candidate === "number" || typeof candidate === "boolean") {
      return String(candidate);
    }
  }
  return undefined;
}

function citationIds(value: unknown): string[] {
  if (typeof value === "string") {
    return [...value.matchAll(CITATION_PATTERN)].map((match) => match[1]);
  }
  if (!Array.isArray(value)) return [];
  return value
    .flatMap((item) => {
      if (typeof item !== "string") return [];
      const marker = item.match(/^\[\[cite:([A-Za-z0-9_-]+)\]\]$/);
      return marker
        ? [marker[1]]
        : [...item.matchAll(CITATION_PATTERN)].map((m) => m[1]);
    })
    .filter((id, index, ids) => ids.indexOf(id) === index);
}

function citationTokens(ids: string[]): string {
  return ids.length > 0
    ? ` ${ids.map((id) => `[[cite:${id}]]`).join(" ")}`
    : "";
}

function parseJsonSummary(text: string): JsonRecord | null {
  const trimmed = text.trim();
  if (!trimmed) return null;

  const candidates = [trimmed];
  const fenced = trimmed.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/i);
  if (fenced) candidates.unshift(fenced[1]);

  for (const candidate of candidates) {
    try {
      const parsed = JSON.parse(candidate);
      const record = asRecord(parsed);
      if (record) return record;
    } catch {
      // Markdown and plain-text summaries are valid output choices.
    }
  }

  return null;
}

function normalizeActionItem(value: unknown): MeetingActionItem | null {
  if (typeof value === "string" && value.trim()) {
    return {
      text: value.trim().replace(CITATION_PATTERN, "").trim(),
      checked: false,
      evidenceIds: citationIds(value),
    };
  }

  const record = asRecord(value);
  if (!record) return null;

  const text = firstString(record, [
    "task",
    "text",
    "action",
    "description",
    "title",
  ]);
  if (!text) return null;

  return {
    text: text.replace(CITATION_PATTERN, "").trim(),
    owner: firstString(record, ["owner", "assignee", "responsible"]),
    dueDate: firstString(record, ["due_date", "dueDate", "deadline", "when"]),
    checked: record.status === "completed" || record.completed === true,
    evidenceIds: citationIds(
      record.evidence ?? record.citations ?? record.sources,
    ),
  };
}

function jsonArray(record: JsonRecord, keys: string[]): unknown[] {
  for (const key of keys) {
    if (Array.isArray(record[key])) return record[key];
  }
  return [];
}

function renderJsonItem(value: unknown, idsFromItem = true): string {
  if (typeof value === "string")
    return `- ${value}${citationTokens(idsFromItem ? citationIds(value) : [])}`;

  const record = asRecord(value);
  if (!record) return "";

  const label = firstString(record, [
    "title",
    "topic",
    "name",
    "question",
    "risk",
    "text",
    "description",
  ]);
  const details = firstString(record, [
    "summary",
    "details",
    "context",
    "rationale",
    "answer",
    "impact",
  ]);
  const ids = citationIds(
    record.evidence ?? record.citations ?? record.sources,
  );
  if (!label && !details) return "";

  const metadata = [
    firstString(record, ["owner", "assignee", "responsible"]),
    firstString(record, ["due_date", "dueDate", "deadline"]),
    firstString(record, ["status"]),
  ]
    .filter(Boolean)
    .join(" · ");

  return `- ${label ?? details}${metadata ? ` (${metadata})` : ""}${citationTokens(ids)}${
    label && details ? `\n  ${details}` : ""
  }`;
}

function renderJsonSection(
  output: string[],
  title: string,
  values: unknown[],
): void {
  const rendered = values.map((value) => renderJsonItem(value)).filter(Boolean);
  if (rendered.length > 0) output.push(`## ${title}\n${rendered.join("\n")}`);
}

function jsonToMarkdown(record: JsonRecord): string {
  const output: string[] = [];
  const title = firstString(record, ["title"]);
  const summary = firstString(record, [
    "summary",
    "overview",
    "executive_summary",
    "executiveSummary",
  ]);

  if (title) output.push(`# ${title}`);
  if (summary) output.push(`## Overview\n${summary}`);

  renderJsonSection(
    output,
    "Topics Discussed",
    jsonArray(record, ["topics", "discussion_topics", "discussion_points"]),
  );
  renderJsonSection(
    output,
    "Decisions",
    jsonArray(record, ["decisions", "agreements", "outcomes"]),
  );

  const actions = jsonArray(record, [
    "action_items",
    "actionItems",
    "tasks",
    "follow_ups",
  ])
    .map(normalizeActionItem)
    .filter((item): item is MeetingActionItem => item !== null);
  if (actions.length > 0) {
    output.push(
      `## Action Items\n${actions
        .map((item) => {
          const metadata = [
            item.owner && `Owner: ${item.owner}`,
            item.dueDate && `Due: ${item.dueDate}`,
          ]
            .filter(Boolean)
            .join(" · ");
          return `- [${item.checked ? "x" : " "}] ${item.text}${metadata ? ` (${metadata})` : ""}${citationTokens(item.evidenceIds)}`;
        })
        .join("\n")}`,
    );
  }

  renderJsonSection(
    output,
    "Open Questions",
    jsonArray(record, ["open_questions", "openQuestions", "questions"]),
  );
  renderJsonSection(
    output,
    "Risks and Blockers",
    jsonArray(record, ["risks", "blockers", "risks_and_blockers"]),
  );
  renderJsonSection(
    output,
    "Important Details",
    jsonArray(record, [
      "important_details",
      "importantDetails",
      "constraints",
      "key_facts",
    ]),
  );
  renderJsonSection(
    output,
    "Next Steps",
    jsonArray(record, ["next_steps", "nextSteps"]),
  );

  if (output.length === 0) {
    const fallback = firstString(record, ["markdown", "content", "text"]);
    if (fallback) return fallback;
  }

  return output.join("\n\n");
}

function stripLegacyDecorators(markdown: string): string {
  return markdown
    .replace(/^(#\s+[^\n]+\r?\nTags:\s*[^\n]+\r?\n?)/i, "")
    .replace(/^[•*\-\s]*✅\s*/gm, "- [x] ")
    .trim();
}

function extractMarkdownActionItems(markdown: string): MeetingActionItem[] {
  return [...markdown.matchAll(/^\s*[-*]\s+\[([ xX])\]\s+(.+)$/gm)]
    .map((match) => ({
      text: match[2].replace(CITATION_PATTERN, "").trim(),
      checked: match[1].toLowerCase() === "x",
      evidenceIds: citationIds(match[2]),
    }))
    .filter((item) => item.text.length > 0);
}

export function getMeetingSummaryMarkdown(entry: HistoryEntry): string {
  const raw = entry.post_processed_text || entry.transcription_text || "";
  const parsed = parseJsonSummary(raw);
  const markdown = parsed ? jsonToMarkdown(parsed) : raw;
  return stripLegacyDecorators(markdown || raw);
}

export function getMeetingFollowUpSummary(entry: HistoryEntry): string {
  const raw = entry.post_processed_text || "";
  const parsed = parseJsonSummary(raw);
  if (parsed) {
    return (
      firstString(parsed, [
        "summary",
        "overview",
        "executive_summary",
        "executiveSummary",
      ]) ?? getMeetingSummaryMarkdown(entry)
    );
  }
  return getMeetingSummaryMarkdown(entry);
}

export function getMeetingActionItems(
  entry: HistoryEntry,
): MeetingActionItem[] {
  const raw = entry.post_processed_text || "";
  const parsed = parseJsonSummary(raw);
  if (parsed) {
    const actions = jsonArray(parsed, [
      "action_items",
      "actionItems",
      "tasks",
      "follow_ups",
    ])
      .map(normalizeActionItem)
      .filter((item): item is MeetingActionItem => item !== null);
    if (actions.length > 0) return actions;
  }
  return extractMarkdownActionItems(getMeetingSummaryMarkdown(entry));
}

export function mergeTranscriptSegments(
  segments: TranscriptSegment[],
  joinText: (target: string, next: string) => string = (target, next) =>
    `${target} ${next}`,
): TranscriptSegment[] {
  const merged: TranscriptSegment[] = [];
  for (const segment of [...segments].sort(
    (left, right) =>
      left.start_ms - right.start_ms || left.end_ms - right.end_ms,
  )) {
    const text = segment.text.trim();
    if (!text) continue;
    const previous = merged[merged.length - 1];
    if (
      previous &&
      previous.source === segment.source &&
      segment.start_ms - previous.end_ms <= 750
    ) {
      previous.end_ms = Math.max(previous.end_ms, segment.end_ms);
      previous.text = joinText(previous.text, text);
      continue;
    }
    merged.push({ ...segment, text });
  }
  return merged;
}

function getCitationMap(entry: HistoryEntry): Map<string, Citation> {
  const transcriptSegments = entry.transcript_segments ?? [];
  const fallbackSegments: TranscriptSegment[] = (
    entry.speaker_segments ?? []
  ).map((segment: SpeakerSegment) => ({
    start_ms: segment.start_ms,
    end_ms: segment.end_ms,
    source: segment.source,
    text: segment.text,
    confidence: segment.confidence,
  }));
  const segments = mergeTranscriptSegments(
    transcriptSegments.length > 0 ? transcriptSegments : fallbackSegments,
  );
  const speakers = entry.speaker_segments ?? [];
  const citations = new Map<string, Citation>();

  segments.forEach((segment, index) => {
    const speaker = speakers.find(
      (candidate) =>
        candidate.start_ms < segment.end_ms &&
        candidate.end_ms > segment.start_ms,
    );
    const id = `SEG-${String(index).padStart(3, "0")}`;
    citations.set(id, {
      id,
      text: segment.text,
      startMs: segment.start_ms,
      endMs: segment.end_ms,
      speaker: speaker?.speaker,
      source: segment.source,
    });
  });

  return citations;
}

function formatOffset(milliseconds: number): string {
  const totalSeconds = Math.max(0, Math.floor(milliseconds / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
}

function prepareCitationLinks(
  markdown: string,
  citations: Map<string, Citation>,
): string {
  return markdown.replace(CITATION_PATTERN, (_marker, id: string) =>
    citations.has(id) ? `[⌁](#meeting-citation-${id})` : "",
  );
}

const CitationPill: React.FC<{ citation: Citation }> = ({ citation }) => (
  <span className="group relative inline-flex align-baseline">
    <span
      tabIndex={0}
      title={citation.text}
      aria-label={`Transcript evidence ${citation.id}`}
      className="mx-0.5 inline-flex h-4 min-w-4 cursor-help items-center justify-center rounded-full border border-tide-teal/40 bg-tide-teal/10 px-1 text-[10px] font-semibold leading-none text-tide-teal outline-none transition-colors hover:bg-tide-teal/20 focus-visible:ring-2 focus-visible:ring-tide-teal/50"
    >
      ⌁
    </span>
    <span className="pointer-events-none invisible absolute bottom-full left-1/2 z-50 mb-2 w-72 -translate-x-1/2 rounded-xl border border-stone-mist bg-orange-off-white p-3 text-left opacity-0 shadow-2xl transition-opacity duration-150 group-hover:visible group-hover:opacity-100 group-focus-within:visible group-focus-within:opacity-100">
      <span className="mb-1 block text-[10px] font-semibold uppercase tracking-wider text-tide-teal">
        {citation.id} · {formatOffset(citation.startMs)}
        {citation.speaker ? ` · ${citation.speaker}` : ""}
      </span>
      <span className="block whitespace-pre-wrap text-xs leading-relaxed text-charcoal">
        {citation.text}
      </span>
    </span>
  </span>
);

function childText(children: React.ReactNode): string {
  return React.Children.toArray(children)
    .map((child) => {
      if (typeof child === "string" || typeof child === "number") {
        return String(child);
      }
      if (React.isValidElement(child)) {
        return childText(child.props.children);
      }
      return "";
    })
    .join("");
}

function removeAlertMarker(children: React.ReactNode): React.ReactNode {
  let removed = false;
  return React.Children.map(children, (child) => {
    if (typeof child === "string") {
      if (removed) return child;
      const cleaned = child.replace(
        /^\s*\[!(NOTE|IMPORTANT|WARNING)\]\s*/i,
        () => {
          removed = true;
          return "";
        },
      );
      return cleaned;
    }
    if (React.isValidElement(child) && child.props.children) {
      return React.cloneElement(
        child as React.ReactElement<{ children?: React.ReactNode }>,
        {
          children: removeAlertMarker(child.props.children),
        },
      );
    }
    return child;
  });
}

const InteractiveTaskListItem: React.FC<{
  children?: React.ReactNode;
  className?: string;
}> = ({ children, className }) => {
  const childrenArray = React.Children.toArray(children);
  const checkbox = childrenArray.find(
    (child) =>
      React.isValidElement(child) &&
      child.type === "input" &&
      child.props.type === "checkbox",
  );
  const initialChecked =
    React.isValidElement(checkbox) &&
    Boolean(checkbox.props.checked ?? checkbox.props.defaultChecked);
  const [checked, setChecked] = useState(initialChecked);
  const content = childrenArray.filter((child) => child !== checkbox);

  return (
    <li
      className={`my-2 flex list-none items-start gap-2.5 ${className ?? ""}`}
    >
      <button
        type="button"
        aria-label={
          checked ? "Mark action item incomplete" : "Mark action item complete"
        }
        aria-pressed={checked}
        onClick={() => setChecked((current) => !current)}
        className={`mt-1 flex h-4 w-4 shrink-0 items-center justify-center rounded-full border transition-colors duration-150 ${
          checked
            ? "border-forest-green bg-forest-green text-orange-off-white"
            : "border-bark-grey/60 bg-transparent hover:border-forest-green"
        }`}
      >
        {checked && <Check className="h-2.5 w-2.5 stroke-[3.5]" />}
      </button>
      <span
        className={`text-sm leading-relaxed transition-colors duration-150 ${
          checked ? "text-bark-grey/60 line-through" : "text-charcoal"
        }`}
      >
        {content}
      </span>
    </li>
  );
};

const AlertBlockquote: React.FC<{ children?: React.ReactNode }> = ({
  children,
}) => {
  const text = childText(children).trim();
  const match = text.match(/^\[!(NOTE|IMPORTANT|WARNING)\]/i);
  if (!match) {
    return (
      <blockquote className="border-l-4 border-stone-mist pl-4 italic text-bark-grey">
        {children}
      </blockquote>
    );
  }

  const type = match[1].toLowerCase();
  const label =
    type === "important"
      ? "Important"
      : type === "warning"
        ? "Warning"
        : "Note";
  const color =
    type === "important"
      ? "text-terracotta"
      : type === "warning"
        ? "text-alarm-red"
        : "text-lichen-green";
  const containerColor =
    type === "important"
      ? "border-terracotta bg-terracotta/5"
      : type === "warning"
        ? "border-alarm-red bg-alarm-red/5"
        : "border-lichen-green bg-lichen-green/5";
  const cleaned = removeAlertMarker(children);

  return (
    <div className={`my-4 rounded-r-xl border-l-4 p-4 ${containerColor}`}>
      <div
        className={`mb-1 text-xs font-bold uppercase tracking-wider ${color}`}
      >
        {label}
      </div>
      <div className="text-sm leading-relaxed text-bark-grey">{cleaned}</div>
    </div>
  );
};

export const MeetingSummaryRenderer: React.FC<{
  entry: HistoryEntry;
  className?: string;
}> = ({ entry, className }) => {
  const markdown = useMemo(() => getMeetingSummaryMarkdown(entry), [entry]);
  const citations = useMemo(() => getCitationMap(entry), [entry]);
  const preparedMarkdown = useMemo(
    () => prepareCitationLinks(markdown, citations),
    [citations, markdown],
  );

  if (!preparedMarkdown) return null;

  return (
    <div className={className}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          li: ({ node, children, ...props }) => {
            if (props.className?.includes("task-list-item")) {
              return (
                <InteractiveTaskListItem>{children}</InteractiveTaskListItem>
              );
            }
            return <li {...props}>{children}</li>;
          },
          blockquote: AlertBlockquote,
          a: ({ node, href, children, ...props }) => {
            if (href?.startsWith("#meeting-citation-")) {
              const id = href.slice("#meeting-citation-".length);
              const citation = citations.get(id);
              return citation ? <CitationPill citation={citation} /> : null;
            }
            return (
              <a href={href} target="_blank" rel="noreferrer" {...props}>
                {children}
              </a>
            );
          },
          table: ({ node, children, ...props }) => (
            <div className="my-4 overflow-x-auto rounded-xl border border-stone-mist/60">
              <table {...props} className="min-w-full text-left text-sm">
                {children}
              </table>
            </div>
          ),
          th: ({ node, children, ...props }) => (
            <th
              {...props}
              className="border-b border-stone-mist bg-stone-mist/20 px-3 py-2 font-semibold text-charcoal"
            >
              {children}
            </th>
          ),
          td: ({ node, children, ...props }) => (
            <td
              {...props}
              className="border-b border-stone-mist/40 px-3 py-2 align-top text-bark-grey"
            >
              {children}
            </td>
          ),
        }}
      >
        {preparedMarkdown}
      </ReactMarkdown>
    </div>
  );
};
