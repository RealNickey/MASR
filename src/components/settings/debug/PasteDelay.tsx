import React from "react";
import { Slider } from "../../ui/Slider";
import { useSettings } from "../../../hooks/useSettings";

interface PasteDelayProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const PasteDelay: React.FC<PasteDelayProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { settings, updateSetting } = useSettings();

  const handleDelayChange = (value: number) => {
    updateSetting("paste_delay_ms", value);
  };

  return (
    <Slider
      value={settings?.paste_delay_ms ?? 60}
      onChange={handleDelayChange}
      min={10}
      max={200}
      step={10}
      label={"Paste Delay"}
      description={
        "Delay before sending paste keystroke (in milliseconds). Increase if wrong text is being pasted."
      }
      descriptionMode={descriptionMode}
      grouped={grouped}
      formatValue={(v) => `${v}ms`}
    />
  );
};
