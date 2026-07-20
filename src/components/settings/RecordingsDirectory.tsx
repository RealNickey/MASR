import React from "react";
import { commands } from "@/bindings";
import { SettingContainer } from "../ui/SettingContainer";
import { Button } from "../ui/Button";

interface RecordingsDirectoryProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const RecordingsDirectory: React.FC<RecordingsDirectoryProps> = ({
  descriptionMode = "inline",
  grouped = false,
}) => {
  const handleOpen = async () => {
    try {
      await commands.openRecordingsFolder();
    } catch (openError) {
      console.error("Failed to open recordings directory:", openError);
    }
  };

  return (
    <SettingContainer
      title={"Recordings Folder"}
      description={"Location where meeting recordings are saved"}
      descriptionMode={descriptionMode}
      grouped={grouped}
    >
      <Button
        onClick={handleOpen}
        variant="secondary"
        size="sm"
        className="px-3 py-2 shrink-0"
      >
        {"Open"}
      </Button>
    </SettingContainer>
  );
};
