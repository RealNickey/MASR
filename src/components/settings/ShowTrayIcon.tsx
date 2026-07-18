import React from "react";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface ShowTrayIconProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const ShowTrayIcon: React.FC<ShowTrayIconProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const showTrayIcon = getSetting("show_tray_icon") ?? true;

    return (
      <ToggleSwitch
        checked={showTrayIcon}
        onChange={(enabled) => updateSetting("show_tray_icon", enabled)}
        isUpdating={isUpdating("show_tray_icon")}
        label={"Show Tray Icon"}
        description={"Display the ThegAi icon in the system tray."}
        descriptionMode={descriptionMode}
        grouped={grouped}
        tooltipPosition="bottom"
      />
    );
  },
);
