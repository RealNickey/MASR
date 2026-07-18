import React, { useCallback, useMemo } from "react";
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

    const options = useMemo(
      () => [
        { value: "parakeet-tdt-0.6b-v3", label: "English" },
        { value: "thegav1", label: "Malayalam" },
      ],
      [],
    );

    const currentValue = settings?.selected_model || "parakeet-tdt-0.6b-v3";

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
        description={"Select the language for basic transcription. Continuous meeting transcription will always use Malayalam."}
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
