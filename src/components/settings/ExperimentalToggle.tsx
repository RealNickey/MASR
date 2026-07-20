import React from "react";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface ExperimentalToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const ExperimentalToggle: React.FC<ExperimentalToggleProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const enabled = getSetting("experimental_enabled") || false;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(enabled) => updateSetting("experimental_enabled", enabled)}
        isUpdating={isUpdating("experimental_enabled")}
        label={"Experimental Features"}
        description={
          "Enable experimental features that are still in development."
        }
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
