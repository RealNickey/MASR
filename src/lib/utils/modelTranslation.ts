import type { ModelInfo } from "@/bindings";

/**
 * Get the English name for a model
 * @param model - The model info object
 * @returns The model name
 */
export function getTranslatedModelName(model: ModelInfo): string {
  return model.name;
}

/**
 * Get the English description for a model
 * @param model - The model info object
 * @returns The model description
 */
export function getTranslatedModelDescription(model: ModelInfo): string {
  return model.is_custom ? "Custom transcription model" : model.description;
}
