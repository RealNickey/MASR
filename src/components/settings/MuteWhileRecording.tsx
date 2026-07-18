import React from "react";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface MuteWhileRecordingToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const MuteWhileRecording: React.FC<MuteWhileRecordingToggleProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const muteEnabled = getSetting("mute_while_recording") ?? false;

    return (
      <ToggleSwitch
        checked={muteEnabled}
        onChange={(enabled) => updateSetting("mute_while_recording", enabled)}
        isUpdating={isUpdating("mute_while_recording")}
        label={"Mute While Recording"}
        description={"Mute system audio during recording"}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  });
