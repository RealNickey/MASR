import React, { useState, useEffect } from "react";
import { createPortal } from "react-dom";
import { X, FileAudio, FilePlus2, Play, Trash2 } from "lucide-react";
import { toast } from "sonner";
import { open } from "@tauri-apps/plugin-dialog";
import { Button } from "./ui/Button";
import { commands } from "@/bindings";
import { motion, AnimatePresence } from "framer-motion";

interface LocalFileTranscriberProps {
  initialFiles: string[];
  onClose: () => void;
  onSuccess: (action: string) => void;
}

export const LocalFileTranscriber: React.FC<LocalFileTranscriberProps> = ({
  initialFiles,
  onClose,
  onSuccess,
}) => {
  const [files, setFiles] = useState<string[]>(initialFiles);
  const [action, setAction] = useState<"meeting" | "transcribe">("meeting");

  // Handle Escape key to close the modal
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, [onClose]);

  const handleAddMore = async () => {
    try {
      const selected = await open({
        multiple: true,
        filters: [
          {
            name: "Audio",
            extensions: ["wav", "mp3", "m4a", "flac", "ogg"],
          },
        ],
      });
      if (selected) {
        const newFiles = Array.isArray(selected) ? selected : [selected];
        setFiles((prev) => [
          ...prev,
          ...newFiles.filter((f) => !prev.includes(f)),
        ]);
      }
    } catch (error) {
      console.error("Failed to open file dialog:", error);
    }
  };

  const removeFile = (fileToRemove: string) => {
    setFiles((prev) => prev.filter((f) => f !== fileToRemove));
  };

  const handleTranscribe = () => {
    if (files.length === 0) return;

    const filesToProcess = [...files];
    const targetAction = action;

    // Close dialog immediately
    onClose();

    // Detach and process in background
    (async () => {
      toast.info(
        `Started transcription for ${filesToProcess.length} file(s) in background.`,
      );

      const results: { file: string; success: boolean }[] = [];

      for (const file of filesToProcess) {
        const fileName = file.split(/[/\\]/).pop() || file;
        try {
          const result = await commands.processLocalFile(file, targetAction);
          if (result.status === "ok") {
            results.push({ file, success: true });
          } else {
            toast.error(`Failed to process ${fileName}: ${result.error}`);
            results.push({ file, success: false });
          }
        } catch (error: any) {
          const errorMsg = error.message || error;
          toast.error(`Error processing ${fileName}: ${errorMsg}`);
          results.push({ file, success: false });
        }
      }

      const successCount = results.filter((r) => r.success).length;

      if (successCount > 0) {
        toast.success(`Successfully processed ${successCount} file(s)`);
        onSuccess(targetAction);
      }
    })();
  };

  const buttonText =
    files.length === 1
      ? "Start Transcription (1 file)"
      : `Start Transcription (${files.length} files)`;

  // Disable body scroll when modal is active
  useEffect(() => {
    const originalOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = originalOverflow;
    };
  }, []);

  return createPortal(
    <motion.div
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.2 }}
      onClick={onClose}
      className="fixed inset-0 z-[100] flex items-center justify-center bg-[#0a0908]/80 backdrop-blur-md p-4"
    >
      <motion.div
        initial={{ opacity: 0, scale: 0.95, y: 10 }}
        animate={{ opacity: 1, scale: 1, y: 0 }}
        exit={{ opacity: 0, scale: 0.95, y: 10 }}
        transition={{ type: "spring", duration: 0.3, bounce: 0.1 }}
        onClick={(e) => e.stopPropagation()}
        className="bg-[#141211] border border-stone-mist rounded-[20px] shadow-2xl w-full max-w-lg max-h-[85vh] md:max-h-[520px] flex flex-col overflow-hidden"
      >
        {/* Header */}
        <div className="flex items-start justify-between p-5 border-b border-stone-mist/50">
          <div className="flex gap-4">
            <div className="w-10 h-10 rounded-xl bg-forest-green/10 flex items-center justify-center text-forest-green shrink-0">
              <FileAudio className="w-5 h-5" />
            </div>
            <div>
              <h2 className="text-md font-bold font-cooper text-charcoal">
                {"Transcribe Local Files"}
              </h2>
              <p className="text-xs text-bark-grey mt-0.5">
                Convert local audio files into written text and summaries.
              </p>
            </div>
          </div>
          <button
            onClick={onClose}
            className="p-1.5 rounded-lg hover:bg-stone-mist/30 text-bark-grey transition-all hover:text-charcoal cursor-pointer active:scale-95 duration-100"
          >
            <X className="w-5 h-5" />
          </button>
        </div>

        {/* Scrollable Body */}
        <div className="p-5 flex-1 overflow-y-auto space-y-4">
          {/* File list section */}
          <div className="space-y-2">
            <div className="text-[10px] font-bold uppercase tracking-wider font-mono text-bark-grey/85 mb-2">
              {"Selected Files:"} ({files.length})
            </div>
            {files.length === 0 ? (
              <div className="text-center text-pebble py-8 border border-dashed border-stone-mist/60 rounded-xl bg-warm-bone/20 font-mono text-xs uppercase tracking-wider">
                {"No files selected. Add some audio files to transcribe."}
              </div>
            ) : (
              <div className="space-y-2 max-h-[120px] overflow-y-auto pr-1">
                <AnimatePresence initial={false}>
                  {files.map((file) => {
                    const fileName = file.split(/[/\\]/).pop() || file;
                    return (
                      <motion.div
                        layout
                        key={file}
                        initial={{ opacity: 0, y: 8 }}
                        animate={{ opacity: 1, y: 0 }}
                        exit={{ opacity: 0, scale: 0.95 }}
                        transition={{ duration: 0.15 }}
                        className="flex items-center justify-between bg-warm-bone/45 border border-stone-mist/40 rounded-xl p-3 hover:bg-warm-bone/80 transition-colors group"
                      >
                        <div className="flex items-center min-w-0 flex-1 mr-4">
                          <FileAudio className="w-4 h-4 text-bark-grey shrink-0 mr-2.5" />
                          <span
                            className="text-xs font-medium text-charcoal truncate"
                            title={file}
                          >
                            {fileName}
                          </span>
                        </div>
                        <button
                          onClick={() => removeFile(file)}
                          className="text-bark-grey hover:text-alarm-red transition-all p-1.5 hover:bg-alarm-red/10 rounded-lg cursor-pointer active:scale-90"
                        >
                          <Trash2 className="w-3.5 h-3.5" />
                        </button>
                      </motion.div>
                    );
                  })}
                </AnimatePresence>
              </div>
            )}

            {/* Add More Files Trigger */}
            <div className="pt-1">
              <button
                onClick={handleAddMore}
                className="flex items-center gap-1.5 text-xs font-semibold uppercase tracking-[0.04em] font-mono text-forest-green hover:text-deep-forest-green transition-all cursor-pointer active:scale-97"
              >
                <FilePlus2 className="w-4 h-4" />
                {"Add more files"}
              </button>
            </div>
          </div>

          {/* Action Choice cards */}
          <div className="space-y-2 pt-3 border-t border-stone-mist/50">
            <div className="text-[10px] font-bold uppercase tracking-wider font-mono text-bark-grey/85">
              {"Action:"}
            </div>
            <div className="grid grid-cols-1 gap-2.5">
              {/* Summarize card */}
              <label
                onClick={() => setAction("meeting")}
                className={`flex items-start gap-3 p-3 rounded-xl border cursor-pointer transition-all duration-150 active:scale-[0.99] select-none ${
                  action === "meeting"
                    ? "border-forest-green bg-forest-green/[0.04]"
                    : "border-stone-mist/60 bg-warm-bone/20 hover:border-stone-mist hover:bg-warm-bone/45"
                }`}
              >
                <input
                  type="radio"
                  name="action"
                  value="meeting"
                  checked={action === "meeting"}
                  onChange={() => setAction("meeting")}
                  className="w-4 h-4 mt-0.5 text-forest-green focus:ring-forest-green bg-[#141211] border-stone-mist"
                />
                <div className="flex flex-col">
                  <span className="text-xs font-semibold text-charcoal">
                    {"Summarize as Meeting"}
                  </span>
                  <span className="text-[11px] text-bark-grey mt-0.5 leading-relaxed">
                    {"Transcribes and summarizes to English (Meetings tab)"}
                  </span>
                </div>
              </label>

              {/* Transcribe card */}
              <label
                onClick={() => setAction("transcribe")}
                className={`flex items-start gap-3 p-3 rounded-xl border cursor-pointer transition-all duration-150 active:scale-[0.99] select-none ${
                  action === "transcribe"
                    ? "border-forest-green bg-forest-green/[0.04]"
                    : "border-stone-mist/60 bg-warm-bone/20 hover:border-stone-mist hover:bg-warm-bone/45"
                }`}
              >
                <input
                  type="radio"
                  name="action"
                  value="transcribe"
                  checked={action === "transcribe"}
                  onChange={() => setAction("transcribe")}
                  className="w-4 h-4 mt-0.5 text-forest-green focus:ring-forest-green bg-[#141211] border-stone-mist"
                />
                <div className="flex flex-col">
                  <span className="text-xs font-semibold text-charcoal">
                    {"Plain Transcribe"}
                  </span>
                  <span className="text-[11px] text-bark-grey mt-0.5 leading-relaxed">
                    {"Basic Malayalam transcription (History tab)"}
                  </span>
                </div>
              </label>
            </div>
          </div>
        </div>

        {/* Pinned Footer */}
        <div className="p-4 border-t border-stone-mist/50 bg-[#141211]/90 flex flex-col gap-3">
          <div className="text-xs text-pebble text-center leading-normal">
            All files will be processed sequentially in the background.
          </div>
          <div className="flex justify-end gap-3">
            <Button
              variant="ghost"
              onClick={onClose}
              className="active:scale-[0.97] transition-transform duration-150"
            >
              Cancel
            </Button>
            <Button
              variant="primary"
              onClick={handleTranscribe}
              disabled={files.length === 0}
              className="active:scale-[0.97] transition-transform duration-150 flex items-center gap-2"
            >
              <Play className="w-3 h-3 fill-current" />
              <span>{buttonText}</span>
            </Button>
          </div>
        </div>
      </motion.div>
    </motion.div>,
    document.body,
  );
};
