import React, { useCallback, useEffect, useMemo, useState } from "react";
import { commands, type ModelInfo } from "@/bindings";
import { SettingContainer } from "../../ui/SettingContainer";
import { Select } from "../../ui/Select";
import { useSettings } from "../../../hooks/useSettings";

interface TranscriptLanguageSelectorProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const TranscriptLanguageSelector: React.FC<TranscriptLanguageSelectorProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { settings, updateSetting } = useSettings();
    const [models, setModels] = useState<ModelInfo[]>([]);

    useEffect(() => {
      let cancelled = false;
      void commands.getAvailableModels().then((result) => {
        if (!cancelled && result.status === "ok") {
          setModels(result.data);
        }
      });

      return () => {
        cancelled = true;
      };
    }, []);

    const englishModelId = useMemo(() => {
      const installedWhisperTurbo = models.find(
        (model) =>
          model.is_downloaded &&
          model.engine_type === "Whisper" &&
          (model.id === "turbo" || model.id === "whisper-turbo"),
      );
      if (installedWhisperTurbo) return installedWhisperTurbo.id;

      const installedParakeet = models.find(
        (model) => model.is_downloaded && model.id === "parakeet-tdt-0.6b-v3",
      );
      return installedParakeet?.id || "parakeet-tdt-0.6b-v3";
    }, [models]);

    const options = useMemo(
      () => [
        { value: englishModelId, label: "English" },
        { value: "thegav1", label: "Malayalam" },
      ],
      [englishModelId],
    );

    const currentValue =
      settings?.selected_model === "thegav1" ? "thegav1" : englishModelId;

    const handleChange = useCallback(
      (value: string | null) => {
        if (value) {
          updateSetting("selected_model", value);
        }
      },
      [updateSetting],
    );

    return (
      <SettingContainer
        title={"Transcript Language"}
        description={
          "Select the language for basic transcription. Continuous meeting transcription will always use Malayalam."
        }
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="w-56">
          <Select
            value={currentValue}
            options={options}
            onChange={(val) => handleChange(val)}
            isClearable={false}
          />
        </div>
      </SettingContainer>
    );
  });

TranscriptLanguageSelector.displayName = "TranscriptLanguageSelector";
