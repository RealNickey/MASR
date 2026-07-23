import React from "react";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface LazyStreamCloseProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const LazyStreamClose: React.FC<LazyStreamCloseProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const enabled = getSetting("lazy_stream_close") ?? false;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(enabled) => updateSetting("lazy_stream_close", enabled)}
        isUpdating={isUpdating("lazy_stream_close")}
        label={"Keep Mic Open Between Transcriptions"}
        description={
          "Keeps the microphone stream open for 30 seconds after recording stops, reducing latency for back-to-back transcriptions. May degrade Bluetooth audio quality while active."
        }
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
