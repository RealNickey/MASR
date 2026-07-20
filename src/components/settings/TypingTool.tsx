import React, { useEffect, useState } from "react";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";
import { commands } from "@/bindings";
import type { TypingTool } from "@/bindings";

interface TypingToolProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

const allToolLabels: Record<string, string> = {
  wtype: "wtype",
  kwtype: "kwtype",
  dotool: "dotool",
  ydotool: "ydotool",
  xdotool: "xdotool",
};

export const TypingToolSetting: React.FC<TypingToolProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const osType = useOsType();
    const [availableTools, setAvailableTools] = useState<string[] | null>(null);

    useEffect(() => {
      if (osType !== "linux") return;
      commands
        .getAvailableTypingTools()
        .then(setAvailableTools)
        .catch(() => {
          setAvailableTools(["auto"]);
        });
    }, [osType]);

    // Only show this setting on Linux
    if (osType !== "linux") {
      return null;
    }

    // Only show if paste method is "direct"
    const pasteMethod = getSetting("paste_method");
    if (pasteMethod !== "direct") {
      return null;
    }

    const tools = availableTools ?? ["auto"];
    const typingToolOptions = tools.map((tool) =>
      tool === "auto"
        ? {
            value: "auto",
            label: "Auto (Recommended)",
          }
        : { value: tool, label: allToolLabels[tool] ?? tool },
    );

    const selectedTool = (getSetting("typing_tool") || "auto") as TypingTool;

    return (
      <SettingContainer
        title={"Typing Tool"}
        description={
          "Choose which Linux typing tool to use for Direct paste method. Auto will automatically detect and use the best available tool for your system."
        }
        descriptionMode={descriptionMode}
        grouped={grouped}
        tooltipPosition="bottom"
      >
        <Dropdown
          options={typingToolOptions}
          selectedValue={selectedTool}
          onSelect={(value) =>
            updateSetting("typing_tool", value as TypingTool)
          }
          disabled={isUpdating("typing_tool")}
        />
      </SettingContainer>
    );
  },
);
