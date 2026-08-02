import React, { useCallback, useEffect, useState } from "react";
import {
  commands,
  type McpConnectionInfo,
  type RagStatusSnapshot,
} from "@/bindings";
import { useSettings } from "../../../hooks/useSettings";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { SettingContainer } from "../../ui/SettingContainer";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { ToggleSwitch } from "../../ui/ToggleSwitch";

const COPY = {
  group: "Integrations",
  vector: {
    label: "Vector memory",
    description:
      "Index meeting transcripts and summaries for semantic Q&A. Requires your Gemini API key.",
    disabled: "Disabled",
    needsKey: "Gemini BYOK key required",
    ready: "Ready",
    indexing: "Indexing",
    error: "Index error",
    reindex: "Reindex",
    clear: "Clear index",
    chunks: (count: number) => `${count} indexed chunks`,
  },
  mcp: {
    label: "Local MCP server",
    description:
      "Expose models, transcripts, summaries, memory search, and transcription jobs to local MCP clients.",
    port: "Port",
    portDescription: "Loopback HTTP endpoint (1024–65535)",
    showConnection: "Show connection",
    rotateToken: "Rotate token",
    endpoint: "Endpoint",
    token: "Bearer token",
  },
} as const;

export const MemoryAndMcpSettings: React.FC = () => {
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const ragEnabled = getSetting("rag_enabled") ?? false;
  const mcpEnabled = getSetting("mcp_server_enabled") ?? false;
  const port = getSetting("mcp_server_port") ?? 8787;
  const [ragStatus, setRagStatus] = useState<RagStatusSnapshot | null>(null);
  const [connection, setConnection] = useState<McpConnectionInfo | null>(null);

  const refreshRagStatus = useCallback(async () => {
    const result = await commands.getRagStatus();
    if (result.status === "ok") setRagStatus(result.data);
  }, []);

  useEffect(() => {
    void refreshRagStatus();
  }, [refreshRagStatus, ragEnabled]);

  const reindex = async () => {
    const result = await commands.reindexRag();
    if (result.status === "ok") await refreshRagStatus();
  };

  const clearIndex = async () => {
    const result = await commands.clearRagIndex();
    if (result.status === "ok") await refreshRagStatus();
  };

  const showConnection = async () => {
    setConnection(await commands.getMcpConnectionInfo());
  };

  const rotateToken = async () => {
    setConnection(await commands.rotateMcpToken());
  };

  const statusLabel = ragStatus?.status ?? "disabled";
  const statusText =
    statusLabel === "needs_byok_key"
      ? COPY.vector.needsKey
      : statusLabel === "ready"
        ? COPY.vector.ready
        : statusLabel === "indexing"
          ? COPY.vector.indexing
          : statusLabel === "error"
            ? COPY.vector.error
            : COPY.vector.disabled;

  return (
    <SettingsGroup title={COPY.group}>
      <ToggleSwitch
        checked={ragEnabled}
        onChange={(enabled) => updateSetting("rag_enabled", enabled)}
        isUpdating={isUpdating("rag_enabled")}
        label={COPY.vector.label}
        description={COPY.vector.description}
        descriptionMode="inline"
        grouped
      />
      <div className="flex items-center justify-between gap-3 px-4 py-2 text-xs text-bark-grey">
        <span>
          {statusText}
          {ragStatus && ` · ${COPY.vector.chunks(ragStatus.indexed_chunks)}`}
        </span>
        <div className="flex gap-2">
          <Button
            variant="secondary"
            size="sm"
            onClick={reindex}
            disabled={!ragEnabled}
          >
            {COPY.vector.reindex}
          </Button>
          <Button variant="ghost" size="sm" onClick={clearIndex}>
            {COPY.vector.clear}
          </Button>
        </div>
      </div>

      <ToggleSwitch
        checked={mcpEnabled}
        onChange={(enabled) => updateSetting("mcp_server_enabled", enabled)}
        isUpdating={isUpdating("mcp_server_enabled")}
        label={COPY.mcp.label}
        description={COPY.mcp.description}
        descriptionMode="inline"
        grouped
      />
      <SettingContainer
        title={COPY.mcp.port}
        description={COPY.mcp.portDescription}
        descriptionMode="inline"
        grouped
      >
        <Input
          type="number"
          min={1024}
          max={65535}
          value={port}
          onChange={(event) => {
            const next = Number(event.target.value);
            if (Number.isInteger(next) && next >= 1024 && next <= 65535) {
              void updateSetting("mcp_server_port", next);
            }
          }}
          disabled={isUpdating("mcp_server_port")}
          className="w-24"
          variant="compact"
        />
      </SettingContainer>
      <div className="flex flex-wrap items-center gap-2 px-4 py-2">
        <Button
          variant="secondary"
          size="sm"
          onClick={showConnection}
          disabled={!mcpEnabled}
        >
          {COPY.mcp.showConnection}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          onClick={rotateToken}
          disabled={!mcpEnabled}
        >
          {COPY.mcp.rotateToken}
        </Button>
        {connection && (
          <div className="w-full rounded-inputs bg-orange-off-white p-3 text-xs text-charcoal">
            <div>
              {COPY.mcp.endpoint}: {connection.endpoint}
            </div>
            <div className="break-all">
              {COPY.mcp.token}: {connection.bearer_token}
            </div>
          </div>
        )}
      </div>
    </SettingsGroup>
  );
};
