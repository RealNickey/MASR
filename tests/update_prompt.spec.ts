import { test, expect } from "@playwright/test";
import { setupMocks, getMockState, setMockState } from "./helpers";

test.describe("Aggressive Auto-Updates", () => {
  test.beforeEach(async ({ page }) => {
    await setupMocks(page);
  });

  test("does not show update modal if no update is available", async ({ page }) => {
    // Set updateAvailable to false
    await page.addInitScript(() => {
      const state = (window as any).__MOCK_STATE__;
      state.updateAvailable = false;
    });

    await page.goto("/");

    // Modal should not be visible
    const modalHeader = page.locator("text=Update Ready");
    await expect(modalHeader).not.toBeVisible();
  });

  test("automatically checks, downloads, and shows update modal when update is available", async ({ page }) => {
    // Set updateAvailable to true
    await page.addInitScript(() => {
      const state = (window as any).__MOCK_STATE__;
      state.updateAvailable = true;
      state.updateVersion = "0.9.0";
    });

    await page.goto("/");

    // Modal should appear
    const modalHeader = page.locator("text=Update Ready");
    await expect(modalHeader).toBeVisible();

    // Check version description
    const modalDesc = page.locator("text=ThegAi v0.9.0 is ready to install");
    await expect(modalDesc).toBeVisible();

    // Verify buttons exist
    const updateNowBtn = page.locator("button:has-text('Update Now')");
    const updateLaterBtn = page.locator("button:has-text('Update on Next Launch')");
    await expect(updateNowBtn).toBeVisible();
    await expect(updateLaterBtn).toBeVisible();
  });

  test("clicking Update Now installs the update and restarts the app", async ({ page }) => {
    await page.addInitScript(() => {
      const state = (window as any).__MOCK_STATE__;
      state.updateAvailable = true;
      state.updateVersion = "0.9.5";
    });

    await page.goto("/");

    const updateNowBtn = page.locator("button:has-text('Update Now')");
    await expect(updateNowBtn).toBeVisible();

    // Click Update Now
    await updateNowBtn.click();

    // Verify relaunch called by checking mock state
    const mockState = await getMockState(page);
    expect(mockState.relaunchCalled).toBe(true);
  });

  test("clicking Update on Next Launch sets flag in localStorage and dismisses modal", async ({ page }) => {
    await page.addInitScript(() => {
      const state = (window as any).__MOCK_STATE__;
      state.updateAvailable = true;
      state.updateVersion = "0.9.0";
    });

    await page.goto("/");

    const updateLaterBtn = page.locator("button:has-text('Update on Next Launch')");
    await expect(updateLaterBtn).toBeVisible();

    // Click Update on Next Launch
    await updateLaterBtn.click();

    // Modal should disappear
    const modalHeader = page.locator("text=Update Ready");
    await expect(modalHeader).not.toBeVisible();

    // Verify localStorage has key set
    const flag = await page.evaluate(() => localStorage.getItem("thegai_update_on_next_launch"));
    expect(flag).toBe("true");
  });

  test("startup installation overlay shows when flag is set in localStorage", async ({ page }) => {
    // Enable update and set flag in localStorage before loading
    await page.addInitScript(() => {
      const state = (window as any).__MOCK_STATE__;
      state.updateAvailable = true;
      state.updateVersion = "1.0.0";
      localStorage.setItem("thegai_update_on_next_launch", "true");
    });

    await page.goto("/");

    // Splash screen should show
    const splashText = page.locator("text=Installing update...");
    await expect(splashText).toBeVisible();

    // Verify relaunch called automatically on startup
    const mockState = await getMockState(page);
    expect(mockState.relaunchCalled).toBe(true);

    // Flag should be removed on successful relaunch
    const flag = await page.evaluate(() => localStorage.getItem("thegai_update_on_next_launch"));
    expect(flag).toBeNull();
  });

  test("startup installation gracefully handles failures and clears flag so user is not locked out", async ({ page }) => {
    // Set flag and trigger download failure during startup install
    await page.addInitScript(() => {
      const state = (window as any).__MOCK_STATE__;
      state.updateAvailable = true;
      state.updateVersion = "1.0.0";
      state.downloadShouldFail = true;
      localStorage.setItem("thegai_update_on_next_launch", "true");
    });

    await page.goto("/");

    // Splash screen should show
    const splashText = page.locator("text=Installing update...");
    await expect(splashText).toBeVisible();

    // Splash should display error description
    const errorText = page.locator("text=Install failed. Please restart the app manually.");
    await expect(errorText).toBeVisible();

    // Wait for fallback timeout to clear splash and boot app
    await page.waitForTimeout(4500);

    // Splash should be gone
    await expect(splashText).not.toBeVisible();

    // Flag should be cleared
    const flag = await page.evaluate(() => localStorage.getItem("thegai_update_on_next_launch"));
    expect(flag).toBeNull();
  });
});
