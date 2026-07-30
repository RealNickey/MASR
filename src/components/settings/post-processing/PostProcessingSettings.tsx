import React, { useEffect, useState } from "react";
import { RefreshCcw } from "lucide-react";
import { commands } from "@/bindings";

import { Alert } from "../../ui/Alert";
import {
  Dropdown,
  SettingContainer,
  SettingsGroup,
  Textarea,
} from "@/components/ui";
import { Button } from "../../ui/Button";
import { ResetButton } from "../../ui/ResetButton";
import { Input } from "../../ui/Input";

import { ProviderSelect } from "../PostProcessingSettingsApi/ProviderSelect";
import { BaseUrlField } from "../PostProcessingSettingsApi/BaseUrlField";
import { ApiKeyField } from "../PostProcessingSettingsApi/ApiKeyField";
import { ModelSelect } from "../PostProcessingSettingsApi/ModelSelect";
import { usePostProcessProviderState } from "../PostProcessingSettingsApi/usePostProcessProviderState";
import { useSettings } from "../../../hooks/useSettings";

const PostProcessingSettingsApiComponent: React.FC = () => {
  const state = usePostProcessProviderState();
  const [testResult, setTestResult] = useState<{
    status: "success" | "error";
    message: string;
  } | null>(null);
  const [isTesting, setIsTesting] = useState(false);

  const handleTestApiKey = async () => {
    setIsTesting(true);
    setTestResult(null);
    try {
      const result = await commands.testPostProcessApiKey(
        state.selectedProviderId,
        state.apiKey,
      );
      if (result.status === "ok") {
        setTestResult({
          status: "success",
          message: "Connection successful!",
        });
      } else {
        setTestResult({
          status: "error",
          message: result.error || "Connection failed",
        });
      }
    } catch (e: any) {
      setTestResult({
        status: "error",
        message: e?.message || String(e) || "Connection failed",
      });
    } finally {
      setIsTesting(false);
    }
  };

  // New state for Ollama connection status
  const [ollamaStatus, setOllamaStatus] = useState<{
    connected: boolean;
    modelCount: number;
    error: string | null;
  } | null>(null);
  const [isCheckingOllama, setIsCheckingOllama] = useState(false);

  const checkOllama = React.useCallback(async () => {
    setIsCheckingOllama(true);
    try {
      const result = await (commands as any).checkOllamaStatus();
      if (result.status === "ok") {
        setOllamaStatus({
          connected: result.data.connected,
          modelCount: result.data.model_count,
          error: result.data.error,
        });
      } else {
        setOllamaStatus({
          connected: false,
          modelCount: 0,
          error: result.error || null,
        });
      }
    } catch (e: any) {
      setOllamaStatus({
        connected: false,
        modelCount: 0,
        error: e?.message || String(e),
      });
    } finally {
      setIsCheckingOllama(false);
    }
  }, []);

  useEffect(() => {
    setTestResult(null);
    if (state.selectedProviderId === "ollama") {
      void checkOllama();
    } else {
      setOllamaStatus(null);
    }
  }, [state.selectedProviderId, state.baseUrl, checkOllama]);

  return (
    <>
      <SettingContainer
        title={"AI Provider"}
        description={"Choose a cloud provider, a local Ollama instance, or a custom OpenAI-compatible endpoint."}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped={true}
      >
        <div className="flex items-center gap-2">
          <ProviderSelect
            options={state.providerOptions}
            value={state.selectedProviderId}
            onChange={state.handleProviderSelect}
          />
        </div>
      </SettingContainer>

      {state.isAppleProvider ? (
        state.appleIntelligenceUnavailable ? (
          <Alert variant="error" contained>
            {
              "Apple Intelligence is not available on this device. Requires an Apple Silicon Mac running macOS Tahoe (26.0) or later with Apple Intelligence enabled in System Settings."
            }
          </Alert>
        ) : null
      ) : (
        <>
          {state.selectedProvider?.allow_base_url_edit && (
            <SettingContainer
              title={"Base URL"}
              description={
                "API base URL for the selected provider. Only custom and Ollama providers can be edited."
              }
              descriptionMode="tooltip"
              layout="horizontal"
              grouped={true}
            >
              <div className="flex items-center gap-2">
                <BaseUrlField
                  value={state.baseUrl}
                  onBlur={state.handleBaseUrlChange}
                  placeholder={"https://api.openai.com/v1"}
                  disabled={state.isBaseUrlUpdating}
                  className="min-w-[380px]"
                />
              </div>
            </SettingContainer>
          )}

          {state.isOllamaProvider && (
            <SettingContainer
              title={"Ollama Status"}
              description={
                "Connection status and model count for your local Ollama instance."
              }
              descriptionMode="tooltip"
              layout="horizontal"
              grouped={true}
            >
              <div className="flex flex-col gap-2 w-full">
                <div className="flex items-center gap-2">
                  <div className="flex items-center gap-2 min-w-[320px]">
                    <span className="text-sm text-neutral-400">
                      {"Connection"}:
                    </span>
                    {isCheckingOllama ? (
                      <span className="text-sm font-medium text-blue-400 animate-pulse">
                        {"Checking..."}
                      </span>
                    ) : ollamaStatus?.connected ? (
                      <span className="text-sm font-medium text-emerald-500 flex items-center gap-1.5">
                        <span className="w-2.5 h-2.5 rounded-full bg-emerald-500 animate-pulse" />
                        {"Connected"}
                      </span>
                    ) : (
                      <span className="text-sm font-medium text-rose-500 flex items-center gap-1.5">
                        <span className="w-2.5 h-2.5 rounded-full bg-rose-500" />
                        {"Disconnected"}
                      </span>
                    )}
                  </div>
                  <Button
                    onClick={checkOllama}
                    disabled={isCheckingOllama}
                    variant="secondary"
                    size="md"
                  >
                    <RefreshCcw
                      className={`w-4 h-4 ${isCheckingOllama ? "animate-spin" : ""}`}
                    />
                  </Button>
                </div>
                {ollamaStatus && (
                  <div className="mt-1">
                    {ollamaStatus.connected ? (
                      <Alert variant="success" contained>
                        {`Successfully connected to Ollama! Found ${ollamaStatus.modelCount} available models.`}
                      </Alert>
                    ) : (
                      <Alert variant="error" contained>
                        {ollamaStatus.error ||
                          "Failed to connect to Ollama. Make sure Ollama is running locally."}
                      </Alert>
                    )}
                  </div>
                )}
              </div>
            </SettingContainer>
          )}

          {!state.isOllamaProvider && (
            <SettingContainer
              title={"API Key"}
              description={"Leave this blank to use the app default. Keys you save are stored in your system credential vault on installed builds."}
              descriptionMode="tooltip"
              layout="horizontal"
              grouped={true}
            >
              <div className="flex flex-col gap-2 w-full">
                <div className="flex items-center gap-2">
                  <ApiKeyField
                    value={state.apiKey}
                    onBlur={state.handleApiKeyChange}
                    placeholder={"sk-..."}
                    disabled={state.isApiKeyUpdating}
                    className="min-w-[320px]"
                  />
                  <Button
                    onClick={handleTestApiKey}
                    disabled={isTesting}
                    variant="secondary"
                    size="md"
                  >
                    {isTesting ? "Testing..." : "Test Connection"}
                  </Button>
                </div>
                {testResult && (
                  <div className="mt-1">
                    <Alert variant={testResult.status} contained>
                      {testResult.message}
                    </Alert>
                  </div>
                )}
              </div>
            </SettingContainer>
          )}
        </>
      )}

      {!state.isAppleProvider && (
        <SettingContainer
          title={"Model"}
          description={
            state.isCustomProvider
              ? "Provide the model identifier expected by your custom endpoint."
              : state.isOllamaProvider
                ? "Choose a local Ollama model from the auto-detected list."
                : "Choose a model exposed by the selected provider."
          }
          descriptionMode="tooltip"
          layout="stacked"
          grouped={true}
        >
          <div className="flex items-center gap-2">
            <ModelSelect
              value={state.model}
              options={state.modelOptions}
              disabled={state.isModelUpdating}
              isLoading={state.isFetchingModels}
              placeholder={
                state.modelOptions.length > 0
                  ? "Search or select a model"
                  : "Type a model name"
              }
              onSelect={state.handleModelSelect}
              onCreate={state.handleModelCreate}
              onBlur={() => {}}
              className="flex-1 min-w-[380px]"
            />
            <ResetButton
              onClick={state.handleRefreshModels}
              disabled={state.isFetchingModels}
              ariaLabel={"Refresh models"}
              className="flex h-10 w-10 items-center justify-center"
            >
              <RefreshCcw
                className={`h-4 w-4 ${state.isFetchingModels ? "animate-spin" : ""}`}
              />
            </ResetButton>
          </div>
        </SettingContainer>
      )}
    </>
  );
};

const PostProcessingSettingsPromptsComponent: React.FC = () => {
  const { getSetting, updateSetting, isUpdating, refreshSettings } =
    useSettings();
  const [isCreating, setIsCreating] = useState(false);
  const [draftName, setDraftName] = useState("");
  const [draftText, setDraftText] = useState("");

  const prompts = getSetting("post_process_prompts") || [];
  const selectedPromptId = getSetting("post_process_selected_prompt_id") || "";
  const selectedPrompt =
    prompts.find((prompt) => prompt.id === selectedPromptId) || null;

  useEffect(() => {
    if (isCreating) return;

    if (selectedPrompt) {
      setDraftName(selectedPrompt.name);
      setDraftText(selectedPrompt.prompt);
    } else {
      setDraftName("");
      setDraftText("");
    }
  }, [
    isCreating,
    selectedPromptId,
    selectedPrompt?.name,
    selectedPrompt?.prompt,
  ]);

  const handlePromptSelect = (promptId: string | null) => {
    if (!promptId) return;
    updateSetting("post_process_selected_prompt_id", promptId);
    setIsCreating(false);
  };

  const handleCreatePrompt = async () => {
    if (!draftName.trim() || !draftText.trim()) return;

    try {
      const result = await commands.addPostProcessPrompt(
        draftName.trim(),
        draftText.trim(),
      );
      if (result.status === "ok") {
        await refreshSettings();
        updateSetting("post_process_selected_prompt_id", result.data.id);
        setIsCreating(false);
      }
    } catch (error) {
      console.error("Failed to create prompt:", error);
    }
  };

  const handleUpdatePrompt = async () => {
    if (!selectedPromptId || !draftName.trim() || !draftText.trim()) return;

    try {
      await commands.updatePostProcessPrompt(
        selectedPromptId,
        draftName.trim(),
        draftText.trim(),
      );
      await refreshSettings();
    } catch (error) {
      console.error("Failed to update prompt:", error);
    }
  };

  const handleDeletePrompt = async (promptId: string) => {
    if (!promptId) return;

    try {
      await commands.deletePostProcessPrompt(promptId);
      await refreshSettings();
      setIsCreating(false);
    } catch (error) {
      console.error("Failed to delete prompt:", error);
    }
  };

  const handleCancelCreate = () => {
    setIsCreating(false);
    if (selectedPrompt) {
      setDraftName(selectedPrompt.name);
      setDraftText(selectedPrompt.prompt);
    } else {
      setDraftName("");
      setDraftText("");
    }
  };

  const handleStartCreate = () => {
    setIsCreating(true);
    setDraftName("");
    setDraftText("");
  };

  const hasPrompts = prompts.length > 0;
  const isDirty =
    !!selectedPrompt &&
    (draftName.trim() !== selectedPrompt.name ||
      draftText.trim() !== selectedPrompt.prompt.trim());

  return (
    <SettingContainer
      title={"Selected Prompt"}
      description="Select a template for refining transcriptions or create a new one. Use {{transcript}} inside the prompt text to reference the captured transcript."
      descriptionMode="tooltip"
      layout="stacked"
      grouped={true}
    >
      <div className="space-y-3">
        <div className="flex gap-2 min-w-0">
          <Dropdown
            selectedValue={selectedPromptId || null}
            options={prompts.map((p) => ({
              value: p.id,
              label: p.name,
            }))}
            onSelect={(value) => handlePromptSelect(value)}
            placeholder={
              prompts.length === 0 ? "No prompts available" : "Select a prompt"
            }
            disabled={
              isUpdating("post_process_selected_prompt_id") || isCreating
            }
            className="flex-1 min-w-0"
          />
          <Button
            onClick={handleStartCreate}
            variant="primary"
            size="md"
            disabled={isCreating}
            className="shrink-0"
          >
            {"Create New Prompt"}
          </Button>
        </div>

        {!isCreating && hasPrompts && selectedPrompt && (
          <div className="space-y-3">
            <div className="space-y-2 flex flex-col">
              <label className="text-sm font-semibold">{"Prompt Label"}</label>
              <Input
                type="text"
                value={draftName}
                onChange={(e) => setDraftName(e.target.value)}
                placeholder={"Enter prompt name"}
                variant="compact"
              />
            </div>

            <div className="space-y-2 flex flex-col">
              <label className="text-sm font-semibold">
                {"Prompt Instructions"}
              </label>
              <Textarea
                value={draftText}
                onChange={(e) => setDraftText(e.target.value)}
                placeholder="Write the instructions to run after transcription. Example: Improve grammar and clarity for the following text: {{transcript}}"
              />
              <p className="text-xs text-mid-gray/70">
                Use <code>{"{{transcript}}"}</code> in your prompt to insert the
                transcript.
              </p>
            </div>

            <div className="flex gap-2 pt-2">
              <Button
                onClick={handleUpdatePrompt}
                variant="primary"
                size="md"
                disabled={!draftName.trim() || !draftText.trim() || !isDirty}
              >
                {"Update Prompt"}
              </Button>
              <Button
                onClick={() => handleDeletePrompt(selectedPromptId)}
                variant="secondary"
                size="md"
                disabled={!selectedPromptId || prompts.length <= 1}
              >
                {"Delete Prompt"}
              </Button>
            </div>
          </div>
        )}

        {!isCreating && !selectedPrompt && (
          <div className="p-3 bg-mid-gray/5 rounded-md border border-mid-gray/20">
            <p className="text-sm text-mid-gray">
              {hasPrompts
                ? "Select a prompt above to view and edit its details."
                : "Click 'Create New Prompt' above to create your first post-processing prompt."}
            </p>
          </div>
        )}

        {isCreating && (
          <div className="space-y-3">
            <div className="space-y-2 block flex flex-col">
              <label className="text-sm font-semibold text-text">
                {"Prompt Label"}
              </label>
              <Input
                type="text"
                value={draftName}
                onChange={(e) => setDraftName(e.target.value)}
                placeholder={"Enter prompt name"}
                variant="compact"
              />
            </div>

            <div className="space-y-2 flex flex-col">
              <label className="text-sm font-semibold">
                {"Prompt Instructions"}
              </label>
              <Textarea
                value={draftText}
                onChange={(e) => setDraftText(e.target.value)}
                placeholder="Write the instructions to run after transcription. Example: Improve grammar and clarity for the following text: {{transcript}}"
              />
              <p className="text-xs text-mid-gray/70">
                Use <code>{"{{transcript}}"}</code> in your prompt to insert the
                transcript.
              </p>
            </div>

            <div className="flex gap-2 pt-2">
              <Button
                onClick={handleCreatePrompt}
                variant="primary"
                size="md"
                disabled={!draftName.trim() || !draftText.trim()}
              >
                {"Create Prompt"}
              </Button>
              <Button
                onClick={handleCancelCreate}
                variant="secondary"
                size="md"
              >
                {"Cancel"}
              </Button>
            </div>
          </div>
        )}
      </div>
    </SettingContainer>
  );
};

export const PostProcessingSettingsApi = React.memo(
  PostProcessingSettingsApiComponent,
);
PostProcessingSettingsApi.displayName = "PostProcessingSettingsApi";

export const PostProcessingSettingsPrompts = React.memo(
  PostProcessingSettingsPromptsComponent,
);
PostProcessingSettingsPrompts.displayName = "PostProcessingSettingsPrompts";

export const PostProcessingSettings: React.FC = () => {
  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={"AI Providers (BYOK)"}>
        <PostProcessingSettingsApi />
      </SettingsGroup>

      <SettingsGroup title={"Prompt"}>
        <PostProcessingSettingsPrompts />
      </SettingsGroup>
    </div>
  );
};
