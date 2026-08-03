import React, { useEffect, useState } from "react";
import { RefreshCcw } from "lucide-react";
import { commands } from "@/bindings";

import { Alert } from "../../ui/Alert";
import { SettingContainer, SettingsGroup } from "@/components/ui";
import { Button } from "../../ui/Button";
import { ResetButton } from "../../ui/ResetButton";

import { ProviderSelect } from "../PostProcessingSettingsApi/ProviderSelect";
import { BaseUrlField } from "../PostProcessingSettingsApi/BaseUrlField";
import { ApiKeyField } from "../PostProcessingSettingsApi/ApiKeyField";
import { ModelSelect } from "../PostProcessingSettingsApi/ModelSelect";
import { usePostProcessProviderState } from "../PostProcessingSettingsApi/usePostProcessProviderState";

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
        description={
          "Choose a cloud provider, a local Ollama instance, or a custom OpenAI-compatible endpoint."
        }
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
              description={
                "Leave this blank to use the app default. Keys you save are stored in your system credential vault on installed builds."
              }
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

export const PostProcessingSettingsApi = React.memo(
  PostProcessingSettingsApiComponent,
);
PostProcessingSettingsApi.displayName = "PostProcessingSettingsApi";

export const PostProcessingSettings: React.FC = () => {
  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={"AI Providers (BYOK)"}>
        <PostProcessingSettingsApi />
      </SettingsGroup>
    </div>
  );
};
