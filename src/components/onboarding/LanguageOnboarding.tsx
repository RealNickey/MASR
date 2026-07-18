import React, { useEffect, useState } from "react";
import { useSettings } from "@/hooks/useSettings";
import { commands } from "@/bindings";
import { Logo } from "../Logo";
import { Check, Keyboard, Users, Mic } from "lucide-react";

interface LanguageOnboardingProps {
  onComplete: () => void;
  isPreview?: boolean;
  onExitPreview?: () => void;
}

const LanguageOnboarding: React.FC<LanguageOnboardingProps> = ({
  onComplete,
  isPreview = false,
  onExitPreview,
}) => {
  const { settings, updateSetting } = useSettings();
  const [selectedLang, setSelectedLang] = useState<"en" | "ml">("en");
  const [englishModelId, setEnglishModelId] = useState("turbo");

  useEffect(() => {
    void commands.getInitialSetupStatus().then((result) => {
      if (result.status === "ok" && result.data.english_model_id) {
        setEnglishModelId(result.data.english_model_id);
      }
    });
  }, []);

  // Get current bindings or fallbacks
  const isMac = navigator.userAgent.indexOf("Mac") !== -1;
  const transcribeShortcut =
    settings?.bindings?.transcribe?.current_binding ||
    (isMac ? "option+space" : "ctrl+space");
  const meetingShortcut =
    settings?.bindings?.meeting?.current_binding ||
    (isMac ? "option+shift+m" : "ctrl+shift+m");

  const formatShortcut = (shortcut: string) => {
    return shortcut
      .split("+")
      .map((key) => key.charAt(0).toUpperCase() + key.slice(1))
      .join(" + ");
  };

  const handleContinue = async () => {
    const targetModel = selectedLang === "ml" ? "thegav1" : englishModelId;
    await updateSetting("selected_model", targetModel);

    onComplete();
  };

  return (
    <div className="h-screen w-screen flex flex-col p-6 gap-6 items-center justify-center relative overflow-y-auto bg-stone-mist/10 select-none">
      {isPreview && onExitPreview && (
        <button
          onClick={onExitPreview}
          className="absolute top-4 right-4 px-3 py-1.5 rounded-md border border-amber-500/20 bg-amber-500/10 hover:bg-amber-500/20 text-amber-600 text-[10px] font-mono font-bold tracking-wide uppercase transition-all duration-200"
        >
          {"Exit Preview"}
        </button>
      )}

      <div className="flex flex-col items-center gap-2 shrink-0">
        <Logo size="lg" className="mb-2" />
        <h2 className="text-2xl font-bold text-charcoal text-center">
          {"Select Transcription Language"}
        </h2>
        <p className="text-bark-grey text-sm text-center max-w-md">
          {
            "Choose your primary language for basic speech-to-text. Meetings will always use Malayalam."
          }
        </p>
      </div>

      <div className="max-w-2xl w-full flex flex-col gap-6">
        {/* Language Selection Cards */}
        <div className="grid grid-cols-2 gap-4">
          <button
            onClick={() => setSelectedLang("en")}
            className={`p-5 rounded-cards border text-left transition-all duration-200 relative ${
              selectedLang === "en"
                ? "border-forest-green bg-forest-green/5 ring-1 ring-forest-green"
                : "border-stone-mist bg-orange-off-white hover:border-bark-grey/50"
            }`}
          >
            <div className="flex justify-between items-start mb-2">
              <span className="text-lg font-bold text-charcoal">English</span>
              {selectedLang === "en" && (
                <span className="p-1 rounded-full bg-forest-green text-[#fffbf7]">
                  <Check className="w-3.5 h-3.5" />
                </span>
              )}
            </div>
            <p className="text-xs text-bark-grey leading-relaxed">
              Ideal for English speech-to-text. Uses a fast and accurate
              transcription model.
            </p>
          </button>

          <button
            onClick={() => setSelectedLang("ml")}
            className={`p-5 rounded-cards border text-left transition-all duration-200 relative ${
              selectedLang === "ml"
                ? "border-forest-green bg-forest-green/5 ring-1 ring-forest-green"
                : "border-stone-mist bg-orange-off-white hover:border-bark-grey/50"
            }`}
          >
            <div className="flex justify-between items-start mb-2">
              <span className="text-lg font-bold text-charcoal">
                മലയാളം (Malayalam)
              </span>
              {selectedLang === "ml" && (
                <span className="p-1 rounded-full bg-forest-green text-[#fffbf7]">
                  <Check className="w-3.5 h-3.5" />
                </span>
              )}
            </div>
            <p className="text-xs text-bark-grey leading-relaxed">
              മലയാളം സംഭാഷണം ടെക്സ്റ്റ് രൂപത്തിലേക്ക് മാറ്റാൻ. ഏറ്റവും
              കൃത്യതയുള്ള മോഡൽ ഉപയോഗിക്കുന്നു.
            </p>
          </button>
        </div>

        {/* Split Screen Shortcuts Info */}
        <div className="grid grid-cols-2 gap-4 border border-stone-mist/60 rounded-cards bg-orange-off-white/40 p-4">
          {/* Left Column: Transcribe */}
          <div className="flex flex-col gap-3 p-3 border-r border-stone-mist/40">
            <div className="flex items-center gap-2 text-forest-green">
              <Mic className="w-5 h-5" />
              <h3 className="font-bold text-sm text-charcoal">Transcribe</h3>
            </div>
            <p className="text-xs text-bark-grey leading-relaxed">
              Quickly dictate text into any application. Press the hotkey to
              start recording, speak, and press it again to paste the text
              instantly at your cursor.
            </p>
            <div className="mt-auto pt-4">
              <span className="text-[10px] uppercase font-mono font-bold tracking-wider text-bark-grey block mb-1">
                Shortcut
              </span>
              <kbd className="inline-flex items-center px-2.5 py-1 rounded bg-stone-mist/30 border border-stone-mist text-xs font-mono font-semibold text-charcoal">
                <Keyboard className="w-3.5 h-3.5 mr-1.5 opacity-70" />
                {formatShortcut(transcribeShortcut)}
              </kbd>
            </div>
          </div>

          {/* Right Column: Meeting */}
          <div className="flex flex-col gap-3 p-3 pl-5">
            <div className="flex items-center gap-2 text-tide-teal">
              <Users className="w-5 h-5" />
              <h3 className="font-bold text-sm text-charcoal">Meeting Mode</h3>
            </div>
            <p className="text-xs text-bark-grey leading-relaxed">
              Record meetings continuously in the background. Generates detailed
              transcripts, structured notes, and action items, and syncs
              summaries with your history.
            </p>
            <div className="mt-auto pt-4">
              <span className="text-[10px] uppercase font-mono font-bold tracking-wider text-bark-grey block mb-1">
                Shortcut
              </span>
              <kbd className="inline-flex items-center px-2.5 py-1 rounded bg-stone-mist/30 border border-stone-mist text-xs font-mono font-semibold text-charcoal">
                <Keyboard className="w-3.5 h-3.5 mr-1.5 opacity-70" />
                {formatShortcut(meetingShortcut)}
              </kbd>
            </div>
          </div>
        </div>

        {/* Continue Button */}
        <button
          onClick={handleContinue}
          className="w-full py-3 rounded-cards bg-[#1d7a46] hover:bg-[#1d7a46]/95 text-white font-bold text-sm transition-colors cursor-pointer shadow-md text-center"
        >
          {"Continue"}
        </button>
      </div>
    </div>
  );
};

export default LanguageOnboarding;
