import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";

interface FallbackPayload {
  failed_model: string;
  failed_provider: string;
  error: string;
  next_model: string | null;
  next_provider: string | null;
}

export function useMeetingSummaryFallbackToast() {
  useEffect(() => {
    const unlisten = listen<FallbackPayload>(
      "meeting-summary-fallback",
      (event) => {
        if (import.meta.env.DEV) {
          const {
            failed_model,
            failed_provider,
            error,
            next_model,
            next_provider,
          } = event.payload;
          const description = next_model
            ? `Model ${failed_model} (${failed_provider}) failed: ${error}. Retrying with ${next_model} (${next_provider})...`
            : `Model ${failed_model} (${failed_provider}) failed: ${error}. No more models in fallback chain.`;
          toast.warning("Meeting Summary Fallback", {
            description,
          });
        }
      },
    );
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);
}
