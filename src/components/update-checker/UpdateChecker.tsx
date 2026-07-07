import React, { useState, useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
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
  const { t } = useTranslation();
  
  const [updateState, setUpdateState] = useState<GlobalUpdateState | null>(null);
  const [showUpToDate, setShowUpToDate] = useState(false);
  const [showPortableUpdateDialog, setShowPortableUpdateDialog] = useState(false);
  
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
    if (!updateState) return t("footer.checkForUpdates");
    
    if (updateState.isChecking) {
      return t("footer.checkingUpdates");
    }
    
    if (updateState.isDownloading) {
      const progress = updateState.downloadProgress;
      if (progress > 0 && progress < 100) {
        return t("footer.downloading", {
          progress: progress.toString().padStart(3),
        });
      }
      return progress === 100 ? t("footer.installing") : t("footer.preparing");
    }

    if (showUpToDate) {
      return t("footer.upToDate");
    }

    if (updateState.updateReady) {
      return t("footer.updateAvailableShort");
    }

    if (updateState.updateAvailable) {
      return t("footer.updateAvailableShort");
    }

    return t("footer.checkForUpdates");
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
  const isUpdateClickable = !isUpdateDisabled && (updateReady || (!isChecking && !showUpToDate));

  return (
    <>
      {showPortableUpdateDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="bg-bg border border-border rounded-lg p-6 max-w-md w-full mx-4 space-y-4">
            <h2 className="text-base font-semibold">
              {t("footer.portableUpdateTitle")}
            </h2>
            <p className="text-sm text-text/70">
              {t("footer.portableUpdateMessage")}
            </p>
            <div className="flex gap-2 justify-end">
              <button
                className="px-3 py-1.5 text-sm rounded border border-border hover:bg-border/50 transition-colors cursor-pointer"
                onClick={() => setShowPortableUpdateDialog(false)}
              >
                {t("common.close")}
              </button>
              <button
                className="px-3 py-1.5 text-sm rounded bg-logo-primary text-white hover:bg-logo-primary/80 transition-colors cursor-pointer"
                onClick={() => {
                  openUrl("https://thegai.app");
                  setShowPortableUpdateDialog(false);
                }}
              >
                {t("footer.portableUpdateButton")}
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
