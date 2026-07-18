import React, { useState, useEffect, useRef } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { ProgressBar } from "../shared";
import { commands } from "../../bindings";
import {
  subscribeToUpdateState,
  triggerManualUpdateCheck,
  GlobalUpdateState,
} from "./GlobalUpdatePrompt";

interface UpdateCheckerProps {
  className?: string;
}

const UpdateChecker: React.FC<UpdateCheckerProps> = ({ className = "" }) => {

  const [updateState, setUpdateState] = useState<GlobalUpdateState | null>(
    null,
  );
  const [showUpToDate, setShowUpToDate] = useState(false);
  const [showPortableUpdateDialog, setShowPortableUpdateDialog] =
    useState(false);

  const wasCheckingRef = useRef(false);
  const isManualCheckRef = useRef(false);
  const upToDateTimeoutRef = useRef<ReturnType<typeof setTimeout>>();

  useEffect(() => {
    // Subscribe to global update state
    const unsubscribe = subscribeToUpdateState((state) => {
      setUpdateState(state);

      // Handle "Up to date" transient message for manual checks
      if (wasCheckingRef.current && !state.isChecking) {
        if (!state.updateAvailable && isManualCheckRef.current) {
          setShowUpToDate(true);
          if (upToDateTimeoutRef.current) {
            clearTimeout(upToDateTimeoutRef.current);
          }
          upToDateTimeoutRef.current = setTimeout(() => {
            setShowUpToDate(false);
          }, 3000);
        }
        isManualCheckRef.current = false;
      }

      wasCheckingRef.current = state.isChecking;
    });

    return () => {
      unsubscribe();
      if (upToDateTimeoutRef.current) {
        clearTimeout(upToDateTimeoutRef.current);
      }
    };
  }, []);

  const handleManualCheck = () => {
    isManualCheckRef.current = true;
    triggerManualUpdateCheck();
  };

  const handleAction = async () => {
    const portable = await commands.isPortable();
    if (portable) {
      setShowPortableUpdateDialog(true);
      return;
    }

    if (updateState?.updateReady) {
      // Trigger update choice modal again by dispatching event,
      // or we can just run triggerManualUpdateCheck which will trigger another check & show prompt.
      triggerManualUpdateCheck();
    } else {
      handleManualCheck();
    }
  };

  const getUpdateStatusText = () => {
    if (!updateState) return "Check for updates";

    if (updateState.isChecking) {
      return "Checking for updates...";
    }

    if (updateState.isDownloading) {
      const progress = updateState.downloadProgress;
      if (progress > 0 && progress < 100) {
        return `Downloading... ${(progress.toString().padStart(3))}%`;
      }
      return progress === 100 ? "Installing..." : "Preparing...";
    }

    if (showUpToDate) {
      return "Up to date";
    }

    if (updateState.updateReady) {
      return "Update available";
    }

    if (updateState.updateAvailable) {
      return "Update available";
    }

    return "Check for updates";
  };

  const isChecking = updateState?.isChecking ?? false;
  const isDownloading = updateState?.isDownloading ?? false;
  const updateAvailable = updateState?.updateAvailable ?? false;
  const updateReady = updateState?.updateReady ?? false;
  const downloadProgress = updateState?.downloadProgress ?? 0;

  const isUpdateDisabled = isChecking || isDownloading;

  // Can click if not currently checking/downloading and:
  // - we have an update ready, OR
  // - we are not checking and not currently showing "Up to date"
  const isUpdateClickable =
    !isUpdateDisabled && (updateReady || (!isChecking && !showUpToDate));

  return (
    <>
      {showPortableUpdateDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="bg-bg border border-border rounded-lg p-6 max-w-md w-full mx-4 space-y-4">
            <h2 className="text-base font-semibold">
              {"Manual update required"}
            </h2>
            <p className="text-sm text-text/70">
              {"Portable installs cannot be updated automatically. To update: download the latest NSIS installer from GitHub Releases, install it to the same folder, then copy your Data/ folder (settings, models, recordings) from the old version to the new one."}
            </p>
            <div className="flex gap-2 justify-end">
              <button
                className="px-3 py-1.5 text-sm rounded border border-border hover:bg-border/50 transition-colors cursor-pointer"
                onClick={() => setShowPortableUpdateDialog(false)}
              >
                {"Close"}
              </button>
              <button
                className="px-3 py-1.5 text-sm rounded bg-logo-primary text-white hover:bg-logo-primary/80 transition-colors cursor-pointer"
                onClick={() => {
                  openUrl("https://thegai.app");
                  setShowPortableUpdateDialog(false);
                }}
              >
                {"Open GitHub Releases"}
              </button>
            </div>
          </div>
        </div>
      )}
      <div className={`flex items-center gap-3 ${className}`}>
        {isUpdateClickable ? (
          <button
            onClick={handleAction}
            disabled={isUpdateDisabled}
            className={`transition-colors disabled:opacity-50 tabular-nums cursor-pointer ${
              updateAvailable || updateReady
                ? "text-logo-primary hover:text-logo-primary/80 font-medium"
                : "text-text/60 hover:text-text/80"
            }`}
          >
            {getUpdateStatusText()}
          </button>
        ) : (
          <span className="text-text/60 tabular-nums">
            {getUpdateStatusText()}
          </span>
        )}

        {isDownloading && downloadProgress > 0 && downloadProgress < 100 && (
          <ProgressBar
            progress={[
              {
                id: "update",
                percentage: downloadProgress,
              },
            ]}
            size="large"
          />
        )}
      </div>
    </>
  );
};

export default UpdateChecker;
