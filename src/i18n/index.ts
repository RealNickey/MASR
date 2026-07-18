export const SUPPORTED_LANGUAGES = [
  { code: "en", name: "English", nativeName: "English", priority: 1 },
];

export type SupportedLanguageCode = "en";

export const syncLanguageFromSettings = async () => {
  document.documentElement.lang = "en";
  document.documentElement.dir = "ltr";
};
