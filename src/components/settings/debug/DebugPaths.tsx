import React from "react";
import { SettingContainer } from "../../ui/SettingContainer";

interface DebugPathsProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const DebugPaths: React.FC<DebugPathsProps> = ({
  descriptionMode = "inline",
  grouped = false,
}) => {

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
            {"App Data:"}
          </span>{" "}
          {}
          <span className="font-mono text-xs select-text">
            %APPDATA%/com.thegai.app
          </span>
        </div>
        <div>
          <span className="font-medium">
            {"Models:"}
          </span>{" "}
          {}
          <span className="font-mono text-xs select-text">
            %APPDATA%/com.thegai.app/models
          </span>
        </div>
        <div>
          <span className="font-medium">
            {"Settings:"}
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
