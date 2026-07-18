import React from "react";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import { RecordingRetentionPeriod } from "@/bindings";

interface RecordingRetentionPeriodProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const RecordingRetentionPeriodSelector: React.FC<RecordingRetentionPeriodProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const selectedRetentionPeriod =
      getSetting("recording_retention_period") || "never";
    const historyLimit = getSetting("history_limit") || 5;

    const handleRetentionPeriodSelect = async (period: string) => {
      await updateSetting(
        "recording_retention_period",
        period as RecordingRetentionPeriod,
      );
    };

    const retentionOptions = [
      { value: "never", label: "Never" },
      {
        value: "preserve_limit",
        label: `Keep latest ${(Number(historyLimit))}`,
      },
      { value: "days3", label: "After 3 days" },
      { value: "weeks2", label: "After 2 weeks" },
      {
        value: "months3",
        label: "After 3 months",
      },
    ];

    return (
      <SettingContainer
        title={"Auto-Delete Recordings"}
        description={"Automatically delete old recordings to save space"}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <Dropdown
          options={retentionOptions}
          selectedValue={selectedRetentionPeriod}
          onSelect={handleRetentionPeriodSelect}
          placeholder={"Select retention period..."}
          disabled={isUpdating("recording_retention_period")}
        />
      </SettingContainer>
    );
  });

RecordingRetentionPeriodSelector.displayName =
  "RecordingRetentionPeriodSelector";
