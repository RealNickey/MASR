import React from "react";
import { Slider } from "../../ui/Slider";
import { useSettings } from "../../../hooks/useSettings";

interface RecordingBufferProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const RecordingBuffer: React.FC<RecordingBufferProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { settings, updateSetting } = useSettings();

  const handleBufferChange = (value: number) => {
    updateSetting("extra_recording_buffer_ms", value);
  };

  return (
    <Slider
      value={settings?.extra_recording_buffer_ms ?? 0}
      onChange={handleBufferChange}
      min={0}
      max={1500}
      step={50}
      label={"Extra Recording Buffer"}
      description={"Extra time (in milliseconds) to keep recording after you release the key, to capture trailing audio. 0 = no extra buffer."}
      descriptionMode={descriptionMode}
      grouped={grouped}
      formatValue={(v) => `${v}ms`}
    />
  );
};
