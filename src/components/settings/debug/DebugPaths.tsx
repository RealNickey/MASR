import React from "react";
import { useTranslation } from "react-i18next";
import { SettingContainer } from "../../ui/SettingContainer";

interface DebugPathsProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const DebugPaths: React.FC<DebugPathsProps> = ({
  descriptionMode = "inline",
  grouped = false,
}) => {
  const { t } = useTranslation();

  return (
    <SettingContainer
      title="Debug Paths"
      description="Display internal file paths and directories for debugging purposes"
      descriptionMode={descriptionMode}
      grouped={grouped}
    >
      <div className="text-sm text-gray-600 space-y-2">
        <div>
          <span className="font-medium">
            {t("settings.debug.paths.appData")}
          </span>{" "}
          {}
          <span className="font-mono text-xs select-text">
            %APPDATA%/com.thegai.app
          </span>
        </div>
        <div>
          <span className="font-medium">
            {t("settings.debug.paths.models")}
          </span>{" "}
          {}
          <span className="font-mono text-xs select-text">
            %APPDATA%/com.thegai.app/models
          </span>
        </div>
        <div>
          <span className="font-medium">
            {t("settings.debug.paths.settings")}
          </span>{" "}
          {}
          <span className="font-mono text-xs select-text">
            %APPDATA%/com.thegai.app/settings_store.json
          </span>
        </div>
      </div>
    </SettingContainer>
  );
};
