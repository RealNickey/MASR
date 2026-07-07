import React, { useState, useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { check, Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { getVersion } from "@tauri-apps/api/app";
import { commands } from "../../bindings";

// Global update state that can be read by other components like UpdateChecker
export interface GlobalUpdateState {
  isChecking: boolean;
  updateAvailable: boolean;
  isDownloading: boolean;
  downloadProgress: number;
  updateReady: boolean;
  updateVersion: string;
  currentVersion: string;
  error: string | null;
}

let globalUpdateState: GlobalUpdateState = {
  isChecking: false,
  updateAvailable: false,
  isDownloading: false,
  downloadProgress: 0,
  updateReady: false,
  updateVersion: "",
  currentVersion: "",
  error: null,
};

const listeners = new Set<(state: GlobalUpdateState) => void>();

export const getGlobalUpdateState = () => globalUpdateState;

export const subscribeToUpdateState = (listener: (state: GlobalUpdateState) => void) => {
  listeners.add(listener);
  listener(globalUpdateState);
  return () => {
    listeners.delete(listener);
  };
};

const updateGlobalState = (updates: Partial<GlobalUpdateState>) => {
  globalUpdateState = { ...globalUpdateState, ...updates };
  listeners.forEach((listener) => listener(globalUpdateState));
  // Dispatch a window event so non-React code or other frames can react if needed
  window.dispatchEvent(new CustomEvent("thegai-update-state-changed", { detail: globalUpdateState }));
};

// Expose a function to trigger a manual check
export const triggerManualUpdateCheck = async () => {
  window.dispatchEvent(new CustomEvent("thegai-trigger-update-check"));
};

export const GlobalUpdatePrompt: React.FC = () => {
  const { t } = useTranslation();
  const [showPrompt, setShowPrompt] = useState(false);
  const [isStartupInstalling, setIsStartupInstalling] = useState(false);
  const [startupError, setStartupError] = useState<string | null>(null);
  
  // Track active update instance
  const activeUpdateRef = useRef<Update | null>(null);
  const isCheckingRef = useRef(false);
  const retryCountRef = useRef(0);
  const checkIntervalRef = useRef<ReturnType<typeof setInterval>>();

  useEffect(() => {
    // 1. Check for deferred install from previous launch
    const checkDeferredInstall = async () => {
      const hasDeferred = localStorage.getItem("thegai_update_on_next_launch") === "true";
      if (!hasDeferred) {
        // Not in startup install mode, proceed with normal check
        startUpdateCheckingCycle();
        return;
      }

      // We have a deferred update to install!
      setIsStartupInstalling(true);
      updateGlobalState({ isChecking: true, isDownloading: true });

      try {
        console.log("[Updater] Performing deferred startup installation...");
        const update = await check();
        if (update) {
          activeUpdateRef.current = update;
          updateGlobalState({
            updateAvailable: true,
            updateVersion: update.version,
            currentVersion: update.currentVersion,
          });

          // Call download and install directly
          await update.downloadAndInstall((event) => {
            if (event.event === "Progress") {
              // We don't have total size here sometimes, but we can compute percentage
              // downloadAndInstall event data chunkLength is accumulative or chunk-wise.
              // Just show installing state
              updateGlobalState({ downloadProgress: 100 });
            }
          });

          console.log("[Updater] Update installed successfully, relaunching...");
          // Clear flag before relaunching so we don't get stuck in a loop
          localStorage.removeItem("thegai_update_on_next_launch");
          localStorage.removeItem("thegai_update_ready");
          
          await relaunch();
        } else {
          console.warn("[Updater] Startup check returned no update. Clearing flag.");
          localStorage.removeItem("thegai_update_on_next_launch");
          localStorage.removeItem("thegai_update_ready");
          setIsStartupInstalling(false);
          startUpdateCheckingCycle();
        }
      } catch (err: any) {
        console.error("[Updater] Startup install failed:", err);
        const errMsg = err?.message || String(err);
        setStartupError(errMsg);
        updateGlobalState({ error: errMsg });
        
        // Self-healing fallback: Clear flag so user is not permanently bricked/locked out
        localStorage.removeItem("thegai_update_on_next_launch");
        
        // Hide startup splash after 4 seconds to let user use the app
        setTimeout(() => {
          setIsStartupInstalling(false);
          startUpdateCheckingCycle();
        }, 4000);
      }
    };

    checkDeferredInstall();

    // Listen for manual update trigger
    const handleManualTrigger = () => {
      performUpdateCheck(true);
    };
    window.addEventListener("thegai-trigger-update-check", handleManualTrigger);

    return () => {
      window.removeEventListener("thegai-trigger-update-check", handleManualTrigger);
      if (checkIntervalRef.current) {
        clearInterval(checkIntervalRef.current);
      }
    };
  }, []);

  const startUpdateCheckingCycle = () => {
    // Perform initial check
    performUpdateCheck(false);

    // Schedule checking every 2 hours (aggressive enough for updates)
    checkIntervalRef.current = setInterval(() => {
      performUpdateCheck(false);
    }, 2 * 60 * 60 * 1000);
  };

  const performUpdateCheck = async (isManual = false) => {
    if (isCheckingRef.current) return;
    isCheckingRef.current = true;
    updateGlobalState({ isChecking: true, error: null });

    try {
      const portable = await commands.isPortable();
      if (portable) {
        // Skip auto-updating for portable installs
        updateGlobalState({ isChecking: false });
        isCheckingRef.current = false;
        return;
      }

      const update = await check();
      if (update) {
        activeUpdateRef.current = update;
        updateGlobalState({
          updateAvailable: true,
          updateVersion: update.version,
          currentVersion: update.currentVersion,
        });

        // Auto-download update in background
        await downloadUpdateBackground(update);
      } else {
        const appVersion = await getVersion().catch(() => "0.8.3");
        updateGlobalState({
          isChecking: false,
          updateAvailable: false,
          updateReady: false,
          currentVersion: appVersion,
          updateVersion: "",
        });
        isCheckingRef.current = false;
        retryCountRef.current = 0;
      }
    } catch (err: any) {
      console.error("[Updater] Update check failed:", err);
      const errMsg = err?.message || String(err);
      updateGlobalState({ isChecking: false, error: errMsg });
      isCheckingRef.current = false;

      // Exponential backoff retry if it was automatic (up to 3 times)
      if (!isManual && retryCountRef.current < 3) {
        retryCountRef.current += 1;
        const delay = Math.pow(2, retryCountRef.current) * 5000; // 10s, 20s, 40s
        console.log(`[Updater] Retrying check in ${delay / 1000}s... (Attempt ${retryCountRef.current}/3)`);
        setTimeout(() => performUpdateCheck(false), delay);
      }
    }
  };

  const downloadUpdateBackground = async (update: Update) => {
    // If update is already marked as ready in localStorage, skip download and show prompt
    const savedReadyVersion = localStorage.getItem("thegai_update_ready");
    if (savedReadyVersion === update.version) {
      updateGlobalState({
        isChecking: false,
        isDownloading: false,
        updateReady: true,
      });
      setShowPrompt(true);
      isCheckingRef.current = false;
      return;
    }

    // Set download mutex/flag in localStorage
    const now = Date.now();
    const activeDownloading = localStorage.getItem("thegai_update_downloading");
    const activeDownloadTime = parseInt(localStorage.getItem("thegai_update_download_time") || "0", 10);
    
    // If another window is downloading the same version and it started less than 5 mins ago, wait for it
    if (activeDownloading === update.version && now - activeDownloadTime < 5 * 60 * 1000) {
      console.log("[Updater] Another window is currently downloading the update. Waiting...");
      updateGlobalState({ isChecking: false, isDownloading: true, downloadProgress: 50 });
      
      // Poll storage for completion
      const pollInterval = setInterval(() => {
        if (localStorage.getItem("thegai_update_ready") === update.version) {
          clearInterval(pollInterval);
          updateGlobalState({ isDownloading: false, updateReady: true });
          setShowPrompt(true);
          isCheckingRef.current = false;
        }
      }, 5000);
      return;
    }

    // Acquire lock and start download
    localStorage.setItem("thegai_update_downloading", update.version);
    localStorage.setItem("thegai_update_download_time", now.toString());
    
    updateGlobalState({ isChecking: false, isDownloading: true, downloadProgress: 0 });

    try {
      let downloadedBytes = 0;
      let totalBytes = 0;

      await update.download((event) => {
        if (event.event === "Started") {
          totalBytes = event.data.contentLength ?? 0;
        } else if (event.event === "Progress") {
          downloadedBytes += event.data.chunkLength;
          const progress = totalBytes > 0 ? Math.round((downloadedBytes / totalBytes) * 100) : 50;
          updateGlobalState({ downloadProgress: Math.min(progress, 99) });
        }
      });

      // Download completed successfully
      localStorage.setItem("thegai_update_ready", update.version);
      localStorage.removeItem("thegai_update_downloading");
      localStorage.removeItem("thegai_update_download_time");

      updateGlobalState({
        isDownloading: false,
        downloadProgress: 100,
        updateReady: true,
      });

      setShowPrompt(true);
    } catch (err: any) {
      console.error("[Updater] Background download failed:", err);
      localStorage.removeItem("thegai_update_downloading");
      localStorage.removeItem("thegai_update_download_time");
      updateGlobalState({ isDownloading: false, error: err?.message || String(err) });
    } finally {
      isCheckingRef.current = false;
    }
  };

  const handleUpdateNow = async () => {
    if (!activeUpdateRef.current) return;
    updateGlobalState({ isChecking: true });

    try {
      console.log("[Updater] Installing update...");
      await activeUpdateRef.current.install();
      console.log("[Updater] Installation triggered, relaunching...");
      localStorage.removeItem("thegai_update_ready");
      await relaunch();
    } catch (err: any) {
      console.error("[Updater] Direct install failed, falling back to downloadAndInstall:", err);
      
      // Fallback: If direct install fails, try downloadAndInstall (in case files were cleared from temp)
      try {
        await activeUpdateRef.current.downloadAndInstall();
        localStorage.removeItem("thegai_update_ready");
        await relaunch();
      } catch (fallbackErr: any) {
        console.error("[Updater] Fallback installation also failed:", fallbackErr);
        updateGlobalState({ isChecking: false, error: fallbackErr?.message || String(fallbackErr) });
        alert(t("footer.updateNowError"));
      }
    }
  };

  const handleUpdateLater = () => {
    // Defer update to next launch
    localStorage.setItem("thegai_update_on_next_launch", "true");
    setShowPrompt(false);
  };

  // 1. Startup Installation Overlay
  if (isStartupInstalling) {
    return (
      <div className="fixed inset-0 z-[9999] flex flex-col items-center justify-center bg-warm-bone text-charcoal p-6 select-none">
        <div className="flex flex-col items-center max-w-sm w-full text-center space-y-6">
          <img
            src="/src/assets/logo.png"
            alt="Logo"
            className="h-16 w-16 object-contain animate-pulse"
          />
          <div className="space-y-2">
            <h2 className="text-xl font-bold font-cooper tracking-wide">
              {t("footer.installingUpdate")}
            </h2>
            <p className="text-sm text-text/60">
              {startupError 
                ? t("footer.updateNowError") 
                : t("common.loading")
              }
            </p>
          </div>
          {!startupError && (
            <div className="w-48 h-1.5 bg-mid-gray/20 rounded-full overflow-hidden">
              <div className="h-full bg-forest-green animate-infinite-scroll w-1/3 rounded-full animate-pulse" />
            </div>
          )}
        </div>
      </div>
    );
  }

  // 2. Ready to Install Choice Modal
  if (showPrompt && activeUpdateRef.current) {
    return (
      <div className="fixed inset-0 z-[999] flex items-center justify-center bg-black/50 backdrop-blur-xs p-4">
        <div className="bg-warm-bone border border-mid-gray/20 rounded-2xl p-6 max-w-md w-full shadow-2xl space-y-6 text-charcoal">
          <div className="flex items-start gap-4">
            <div className="p-3 bg-forest-green/10 text-forest-green rounded-xl">
              <img
                src="/src/assets/logo.png"
                alt="Logo"
                className="h-8 w-8 object-contain"
              />
            </div>
            <div className="space-y-1">
              <h2 className="text-lg font-bold font-cooper tracking-wide text-charcoal">
                {t("footer.updateReady")}
              </h2>
              <p className="text-sm text-text/70">
                {t("footer.updateReadyDescription", {
                  appName: "ThegAi",
                  version: activeUpdateRef.current.version,
                  current: activeUpdateRef.current.currentVersion,
                })}
              </p>
            </div>
          </div>

          <div className="flex gap-3 justify-end pt-2">
            <button
              onClick={handleUpdateLater}
              className="px-4 py-2 text-sm font-semibold rounded-xl border border-mid-gray/30 hover:bg-mid-gray/10 transition-colors cursor-pointer text-charcoal"
            >
              {t("footer.updateOnNextLaunch")}
            </button>
            <button
              onClick={handleUpdateNow}
              className="px-4 py-2 text-sm font-semibold rounded-xl bg-forest-green text-orange-off-white hover:bg-forest-green/90 transition-colors shadow-sm cursor-pointer"
            >
              {t("footer.updateNow")}
            </button>
          </div>
        </div>
      </div>
    );
  }

  return null;
};
