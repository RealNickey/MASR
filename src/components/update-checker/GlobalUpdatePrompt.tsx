import React, { useState, useEffect, useRef } from "react";
import { check, Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { getVersion } from "@tauri-apps/api/app";
import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { ask, message } from "@tauri-apps/plugin-dialog";
import { commands } from "../../bindings";
import { useSettingsStore } from "../../stores/settingsStore";

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
  showPrompt: boolean;
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
  showPrompt: false,
};

const listeners = new Set<(state: GlobalUpdateState) => void>();

const PRIMARY_WINDOW_LABEL = "primary";
const WAIT_MUTEX_TIMEOUT_MS = 5 * 60 * 1000;

export const getGlobalUpdateState = () => globalUpdateState;

export const subscribeToUpdateState = (
  listener: (state: GlobalUpdateState) => void,
) => {
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
  window.dispatchEvent(
    new CustomEvent("thegai-update-state-changed", {
      detail: globalUpdateState,
    }),
  );
  // Emit Tauri event for cross-window sync
  emit("thegai-update-state-sync", globalUpdateState).catch(console.error);
};

// Expose a function to trigger a manual check
export const triggerManualUpdateCheck = async () => {
  await emit("thegai-trigger-update-check");
};

// Module-level flags to prevent StrictMode double-mounting race condition and duplicate cycles
let deferredInstallPromise: Promise<void> | null = null;
let updateCheckingCycleStarted = false;
let startupUpdateFlowPromise: Promise<void> | null = null;
let updateChoiceDialogPromise: Promise<void> | null = null;

export const GlobalUpdatePrompt: React.FC = () => {
  const [updateState, setUpdateState] =
    useState<GlobalUpdateState>(globalUpdateState);
  const [isStartupInstalling, setIsStartupInstalling] = useState(false);
  const [startupError, setStartupError] = useState<string | null>(null);
  const currentWindowLabel = getCurrentWebviewWindow().label;
  const isPrimaryWindow = currentWindowLabel === PRIMARY_WINDOW_LABEL;

  // Track active update instance
  const activeUpdateRef = useRef<Update | null>(null);
  const isCheckingRef = useRef(false);
  const retryCountRef = useRef(0);
  const checkIntervalRef = useRef<ReturnType<typeof setInterval>>();
  const pollIntervalRef = useRef<ReturnType<typeof setInterval>>();

  const settings = useSettingsStore((state) => state.settings);
  const isLoading = useSettingsStore((state) => state.isLoading);

  useEffect(() => {
    // Subscribe to global update state
    const unsubscribe = subscribeToUpdateState((state) => {
      setUpdateState(state);
    });

    // Listen for manual update trigger from any window
    const unlistenManualTriggerPromise = listen(
      "thegai-trigger-update-check",
      () => {
        if (!isPrimaryWindow) return;
        performUpdateCheck(true);
      },
    );

    // Listen for update state sync from other windows
    const unlistenSyncPromise = listen<GlobalUpdateState>(
      "thegai-update-state-sync",
      (event) => {
        const newState = event.payload;
        globalUpdateState = { ...globalUpdateState, ...newState };
        listeners.forEach((listener) => listener(globalUpdateState));
        window.dispatchEvent(
          new CustomEvent("thegai-update-state-changed", {
            detail: globalUpdateState,
          }),
        );
      },
    );

    return () => {
      unsubscribe();
      unlistenManualTriggerPromise.then((unlisten) => unlisten());
      unlistenSyncPromise.then((unlisten) => unlisten());
      if (checkIntervalRef.current) {
        clearInterval(checkIntervalRef.current);
      }
      if (pollIntervalRef.current) {
        clearInterval(pollIntervalRef.current);
      }
    };
  }, [isPrimaryWindow]);

  // Trigger startup and deferred install check once settings are loaded
  useEffect(() => {
    if (isLoading || settings === null || !isPrimaryWindow) return;
    if (startupUpdateFlowPromise) return;

    startupUpdateFlowPromise = (async () => {
      const hasDeferred =
        localStorage.getItem("thegai_update_on_next_launch") === "true";
      if (!hasDeferred) {
        if (!updateCheckingCycleStarted) {
          updateCheckingCycleStarted = true;
          startUpdateCheckingCycle();
        }
        return;
      }

      if (deferredInstallPromise) return;

      deferredInstallPromise = (async () => {
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
                updateGlobalState({ downloadProgress: 100 });
              }
            });

            console.log(
              "[Updater] Update installed successfully, relaunching...",
            );
            // Clear flag before relaunching so we don't get stuck in a loop
            localStorage.removeItem("thegai_update_on_next_launch");
            localStorage.removeItem("thegai_update_ready");

            await relaunch();
          } else {
            console.warn(
              "[Updater] Startup check returned no update. Clearing flag.",
            );
            localStorage.removeItem("thegai_update_on_next_launch");
            localStorage.removeItem("thegai_update_ready");
            setIsStartupInstalling(false);
            if (!updateCheckingCycleStarted) {
              updateCheckingCycleStarted = true;
              startUpdateCheckingCycle();
            }
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
            if (!updateCheckingCycleStarted) {
              updateCheckingCycleStarted = true;
              startUpdateCheckingCycle();
            }
          }, 4000);
        }
      })();

      await deferredInstallPromise;
    })();

    void startupUpdateFlowPromise
      .catch((error) => {
        console.error("[Updater] Startup flow failed:", error);
      })
      .finally(() => {
        startupUpdateFlowPromise = null;
      });
  }, [isLoading, settings, isPrimaryWindow]);

  const startUpdateCheckingCycle = () => {
    if (!isPrimaryWindow) return;

    // Perform initial check
    performUpdateCheck(false);

    // Schedule checking every 2 hours (aggressive enough for updates)
    checkIntervalRef.current = setInterval(
      () => {
        performUpdateCheck(false);
      },
      2 * 60 * 60 * 1000,
    );
  };

  const performUpdateCheck = async (isManual = false) => {
    if (!isPrimaryWindow) return;

    if (isCheckingRef.current) return;
    isCheckingRef.current = true;
    updateGlobalState({ isChecking: true, error: null });

    try {
      // 1. Check if settings permit update checks
      const latestSettings = useSettingsStore.getState().settings;
      const updateChecksEnabled = latestSettings?.update_checks_enabled ?? true;
      if (!updateChecksEnabled && !isManual) {
        updateGlobalState({ isChecking: false });
        isCheckingRef.current = false;
        return;
      }

      // 2. Check if portable
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
          showPrompt: false,
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
        console.log(
          `[Updater] Retrying check in ${delay / 1000}s... (Attempt ${retryCountRef.current}/3)`,
        );
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
        showPrompt: true,
      });
      isCheckingRef.current = false;
      return;
    }

    // Set download mutex/flag in localStorage
    const now = Date.now();
    const activeDownloading = localStorage.getItem("thegai_update_downloading");
    const activeDownloadTime = parseInt(
      localStorage.getItem("thegai_update_download_time") || "0",
      10,
    );

    // If another window is downloading the same version and it started less than 5 mins ago, wait for it
    if (
      activeDownloading === update.version &&
      now - activeDownloadTime < 5 * 60 * 1000
    ) {
      console.log(
        "[Updater] Another window is currently downloading the update. Waiting...",
      );
      updateGlobalState({
        isChecking: false,
        isDownloading: true,
        downloadProgress: 50,
      });

      // Poll storage for completion or cancellation
      pollIntervalRef.current = setInterval(() => {
        const readyVersion = localStorage.getItem("thegai_update_ready");
        const currentDownloading = localStorage.getItem(
          "thegai_update_downloading",
        );
        const currentDownloadTime = parseInt(
          localStorage.getItem("thegai_update_download_time") || "0",
          10,
        );

        if (Date.now() - activeDownloadTime >= WAIT_MUTEX_TIMEOUT_MS) {
          if (pollIntervalRef.current) clearInterval(pollIntervalRef.current);
          console.warn(
            "[Updater] Waited too long for another downloader. Stopping wait.",
          );
          updateGlobalState({
            isDownloading: false,
            error: "Download timed out while waiting for another window.",
          });
          isCheckingRef.current = false;
          return;
        }

        if (readyVersion === update.version) {
          if (pollIntervalRef.current) clearInterval(pollIntervalRef.current);
          updateGlobalState({
            isDownloading: false,
            updateReady: true,
            showPrompt: true,
          });
          isCheckingRef.current = false;
        } else if (
          currentDownloading !== update.version ||
          currentDownloadTime !== activeDownloadTime
        ) {
          // The other window's download lock was released (download failed or window closed)
          if (pollIntervalRef.current) clearInterval(pollIntervalRef.current);
          console.warn(
            "[Updater] Active downloader lock was released or changed. Stopping wait.",
          );
          updateGlobalState({
            isDownloading: false,
            error: "Download failed or was cancelled by another window.",
          });
          isCheckingRef.current = false;
        }
      }, 5000);
      return;
    }

    // Acquire lock and start download
    localStorage.setItem("thegai_update_downloading", update.version);
    localStorage.setItem("thegai_update_download_time", now.toString());

    updateGlobalState({
      isChecking: false,
      isDownloading: true,
      downloadProgress: 0,
    });

    try {
      let downloadedBytes = 0;
      let totalBytes = 0;
      let downloadCompleted = false;

      const finalizeDownload = () => {
        if (downloadCompleted) return;
        downloadCompleted = true;

        localStorage.setItem("thegai_update_ready", update.version);
        localStorage.removeItem("thegai_update_downloading");
        localStorage.removeItem("thegai_update_download_time");

        updateGlobalState({
          isDownloading: false,
          downloadProgress: 100,
          updateReady: true,
          showPrompt: true,
        });
      };

      await update.download((event) => {
        if (event.event === "Started") {
          totalBytes = event.data.contentLength ?? 0;
        } else if (event.event === "Progress") {
          downloadedBytes += event.data.chunkLength;
          const progress =
            totalBytes > 0
              ? Math.round((downloadedBytes / totalBytes) * 100)
              : 50;
          updateGlobalState({ downloadProgress: Math.min(progress, 99) });
          if (totalBytes > 0 && downloadedBytes >= totalBytes) {
            finalizeDownload();
          }
        } else if (event.event === "Finished") {
          finalizeDownload();
        }
      });

      finalizeDownload();
    } catch (err: any) {
      console.error("[Updater] Background download failed:", err);
      localStorage.removeItem("thegai_update_downloading");
      localStorage.removeItem("thegai_update_download_time");
      updateGlobalState({
        isDownloading: false,
        error: err?.message || String(err),
      });
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
      updateGlobalState({ showPrompt: false });
      await relaunch();
    } catch (err: any) {
      console.error(
        "[Updater] Direct install failed, falling back to downloadAndInstall:",
        err,
      );

      // Fallback: If direct install fails, try downloadAndInstall (in case files were cleared from temp)
      try {
        await activeUpdateRef.current.downloadAndInstall();
        localStorage.removeItem("thegai_update_ready");
        updateGlobalState({ showPrompt: false });
        await relaunch();
      } catch (fallbackErr: any) {
        console.error(
          "[Updater] Fallback installation also failed:",
          fallbackErr,
        );
        updateGlobalState({
          isChecking: false,
          error: fallbackErr?.message || String(fallbackErr),
        });
        await message("Install failed. Please restart the app manually.", {
          title: "ThegAi update",
          kind: "error",
        });
      }
    }
  };

  const handleUpdateLater = () => {
    // Defer update to next launch
    localStorage.setItem("thegai_update_on_next_launch", "true");
    updateGlobalState({ showPrompt: false });
  };

  // A native system dialog is modal to the invoking webview, keeping the
  // update decision above ThegAi on Windows, macOS, and Linux. The module
  // promise prevents duplicate dialogs when update state is broadcast.
  useEffect(() => {
    if (
      !isPrimaryWindow ||
      !updateState.showPrompt ||
      !activeUpdateRef.current ||
      updateChoiceDialogPromise
    ) {
      return;
    }

    updateChoiceDialogPromise = (async () => {
      try {
        await getCurrentWebviewWindow().setFocus();
        const installNow = await ask(
          `ThegAi v${activeUpdateRef.current?.version} is ready to install. You're on v${activeUpdateRef.current?.currentVersion}.`,
          {
            title: "Update ready",
            kind: "info",
            okLabel: "Update now",
            cancelLabel: "Later",
          },
        );

        if (installNow) {
          await handleUpdateNow();
        } else {
          handleUpdateLater();
        }
      } catch (error) {
        console.error("[Updater] Failed to show update dialog:", error);
        updateGlobalState({ error: String(error) });
      } finally {
        updateChoiceDialogPromise = null;
      }
    })();
  }, [isPrimaryWindow, updateState.showPrompt, updateState.updateVersion]);

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
              {"Installing update..."}
            </h2>
            <p className="text-sm text-text/60">
              {startupError
                ? "Install failed. Please restart the app manually."
                : "Loading..."}
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

  return null;
};
