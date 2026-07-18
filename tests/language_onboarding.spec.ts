import { test, expect } from "@playwright/test";
import { setupMocks, getMockState } from "./helpers";

test.describe("Language Onboarding & Transcript Language Settings", () => {
  test("triggers onboarding and allows choosing Malayalam", async ({
    page,
  }) => {
    // Pass overrides to setupMocks to ensure it starts with correct state
    await setupMocks(page, false, {
      hasModelsAvailable: false,
      selectedModel: "",
    });

    // Go to homepage
    await page.goto("/");

    // We should be on the Language Onboarding screen
    const titleLocator = page.locator("text=Select Transcription Language");
    await expect(titleLocator).toBeVisible();

    // Verify both language options are visible
    const englishBtn = page.locator("button:has-text('English')");
    const malayalamBtn = page.locator("button:has-text('മലയാളം')");
    await expect(englishBtn).toBeVisible();
    await expect(malayalamBtn).toBeVisible();

    // Verify split screen contains shortcuts info
    await expect(page.locator("h3:has-text('Transcribe')")).toBeVisible();
    await expect(page.locator("h3:has-text('Meeting Mode')")).toBeVisible();

    // Select Malayalam
    await malayalamBtn.click();

    // Verify Malayalam button has the active styling
    await expect(malayalamBtn).toHaveClass(/border-forest-green/);

    // Click Continue
    const continueBtn = page.locator("button:has-text('Continue')");
    await expect(continueBtn).toBeVisible();
    await continueBtn.click();

    // Onboarding should finish and we should land on main app settings layout
    await expect(page.locator("text=Shortcuts & Language")).toBeVisible();

    // Verify mock state has updated the selected model to thegav1
    const state = await getMockState(page);
    expect(state.selectedModel).toBe("thegav1");
  });

  test("allows switching transcript language in General settings", async ({
    page,
  }) => {
    // Start with models available and selected model set to English
    await setupMocks(page, false, {
      hasModelsAvailable: true,
      selectedModel: "parakeet-tdt-0.6b-v3",
    });

    await page.goto("/");

    // Locate the Transcript Language setting row
    const rowTitle = page.locator("h3:has-text('Transcript Language')");
    await expect(rowTitle).toBeVisible();

    // Select Malayalam
    // Note: react-select is used for Select. Let's click the container first.
    const selectContainer = page.locator(".w-56");
    await expect(selectContainer).toBeVisible();

    // We can also click on react-select control
    await selectContainer.click();

    // Select Malayalam from dropdown option
    const option = page
      .locator("div[id*='-option-']")
      .locator("text=Malayalam");
    await expect(option).toBeVisible();
    await option.click();

    // Verify selected model has changed to thegav1
    const state = await getMockState(page);
    expect(state.selectedModel).toBe("thegav1");
  });

  test("triggers onboarding and allows choosing English", async ({ page }) => {
    await setupMocks(page, false, {
      hasModelsAvailable: false,
      selectedModel: "",
    });

    await page.goto("/");

    // We should be on the Language Onboarding screen
    const titleLocator = page.locator("text=Select Transcription Language");
    await expect(titleLocator).toBeVisible();

    const englishBtn = page.locator("button:has-text('English')");
    await expect(englishBtn).toBeVisible();

    // English should be selected by default (active styling)
    await expect(englishBtn).toHaveClass(/border-forest-green/);

    // Click Continue
    const continueBtn = page.locator("button:has-text('Continue')");
    await expect(continueBtn).toBeVisible();
    await continueBtn.click();

    // Onboarding should finish and we should land on main app settings layout
    await expect(page.locator("text=Shortcuts & Language")).toBeVisible();

    // Verify mock state has updated the selected model to parakeet-tdt-0.6b-v3
    const state = await getMockState(page);
    expect(state.selectedModel).toBe("parakeet-tdt-0.6b-v3");
  });

  test("allows switching transcript language to English in General settings", async ({
    page,
  }) => {
    // Start with Malayalam model
    await setupMocks(page, false, {
      hasModelsAvailable: true,
      selectedModel: "thegav1",
    });

    await page.goto("/");

    const rowTitle = page.locator("h3:has-text('Transcript Language')");
    await expect(rowTitle).toBeVisible();

    const selectContainer = page.locator(".w-56");
    await expect(selectContainer).toBeVisible();
    await selectContainer.click();

    // Select English from dropdown option
    const option = page.locator("div[id*='-option-']").locator("text=English");
    await expect(option).toBeVisible();
    await option.click();

    // Verify selected model has changed to parakeet-tdt-0.6b-v3
    const state = await getMockState(page);
    expect(state.selectedModel).toBe("parakeet-tdt-0.6b-v3");
  });
});
