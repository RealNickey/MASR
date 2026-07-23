import React from "react";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { LanguageSelector } from "../LanguageSelector";
import { TranslateToEnglish } from "../TranslateToEnglish";
import { useModelStore } from "../../../stores/modelStore";
import type { ModelInfo } from "@/bindings";

export const ModelSettingsCard: React.FC = () => {
  const { currentModel, models } = useModelStore();

  const currentModelInfo = models.find((m: ModelInfo) => m.id === currentModel);

  const supportsLanguageSelection =
    currentModelInfo?.supports_language_selection ?? false;
  const supportsTranslation = currentModelInfo?.supports_translation ?? false;
  const hasAnySettings = supportsLanguageSelection || supportsTranslation;

  // Don't render anything if no model is selected or no settings available
  if (!currentModel || !currentModelInfo || !hasAnySettings) {
    return null;
  }

  return (
    <SettingsGroup title={`${currentModelInfo.name} Settings`}>
      {supportsLanguageSelection && (
        <LanguageSelector
          descriptionMode="tooltip"
          grouped={true}
          supportedLanguages={currentModelInfo.supported_languages}
        />
      )}
      {supportsTranslation && (
        <TranslateToEnglish descriptionMode="tooltip" grouped={true} />
      )}
    </SettingsGroup>
  );
};
