import React, { useEffect, useState } from "react";
import { commands } from "@/bindings";
import { SettingContainer } from "../../ui/SettingContainer";
import { PathDisplay } from "../../ui/PathDisplay";

interface LogDirectoryProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const LogDirectory: React.FC<LogDirectoryProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const [logDir, setLogDir] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const loadLogDirectory = async () => {
      try {
        const result = await commands.getLogDirPath();
        if (result.status === "ok") {
          setLogDir(result.data);
        } else {
          setError(result.error);
        }
      } catch (err) {
        const errorMessage =
          err && typeof err === "object" && "message" in err
            ? String(err.message)
            : "Failed to load log directory";
        setError(errorMessage);
      } finally {
        setLoading(false);
      }
    };

    loadLogDirectory();
  }, []);

  const handleOpen = async () => {
    if (!logDir) return;
    try {
      await commands.openLogDir();
    } catch (openError) {
      console.error("Failed to open log directory:", openError);
    }
  };

  return (
    <SettingContainer
      title={"Log Directory"}
      description={"Location where log files are stored"}
      descriptionMode={descriptionMode}
      grouped={grouped}
      layout="stacked"
    >
      {loading ? (
        <div className="animate-pulse">
          <div className="h-8 bg-gray-100 rounded" />
        </div>
      ) : error ? (
        <div className="p-3 bg-red-50 border border-red-200 rounded text-xs text-red-600">
          {`Error loading directory: ${(error)}`}
        </div>
      ) : (
        <PathDisplay path={logDir} onOpen={handleOpen} disabled={!logDir} />
      )}
    </SettingContainer>
  );
};
