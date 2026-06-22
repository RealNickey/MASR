import { expect, test } from "@playwright/test";
import { getMockState, setMockState, setupMocks } from "./helpers";

test.describe("Meeting Assistant", () => {
  test.beforeEach(async ({ page }) => {
    await setupMocks(page, false);
    await page.goto("/");
  });

  test("local detected meeting prompt starts recording", async ({ page }) => {
    await page.evaluate(async () => {
      const mod = await import("/src/bindings.ts");
      await mod.commands.startMeetingRecordingFromPrompt();
    });

    const state = await getMockState(page);
    expect(state.promptEvents).toContainEqual({ action: "start" });
  });

  test("meeting prompt renders compact UI, times out, and records from the prompt", async ({
    page,
  }) => {
    await page.goto("/src/meeting_prompt/index.html");

    await page.evaluate(() => {
      (window as any).__MEETING_PROMPT_TIMEOUT_MS__ = 800;
    });

    await page.evaluate(() => {
      (window as any).__EMIT_EVENT__("meeting-overlay-show", {
        state: "suggestion",
        prompt: {
          provider: "Google Meet",
          title: "Detected standup",
          source: "LocalDetection",
          start_time: new Date().toISOString(),
          join_url: "https://meet.google.com/abc-defg-hij",
        },
      });
    });

    await expect(page.getByText("Meeting Detected")).toBeVisible();
    await expect(page.getByText("Google Meet")).toBeVisible();
    await expect(page.getByText("Detected standup")).not.toBeVisible();
    await expect(
      page.locator(".meeting-prompt-provider-cell img[alt='Google Meet']"),
    ).toBeVisible();
    await expect(page.locator(".meeting-prompt-progress")).toBeVisible();
    await expect(page.getByRole("button", { name: "Dismiss" })).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Record" }),
    ).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Join" }),
    ).not.toBeVisible();

    await page.waitForTimeout(900);
    let state = await getMockState(page);
    expect(
      state.promptEvents.some((event) => event.action === "dismiss"),
    ).toBe(true);

    await page.evaluate(() => {
      (window as any).__EMIT_EVENT__("meeting-overlay-show", {
        state: "suggestion",
        prompt: {
          provider: "Microsoft Teams",
          title: "Upcoming design review",
          source: "GoogleCalendar",
          start_time: new Date().toISOString(),
          join_url:
            "https://teams.microsoft.com/l/meetup-join/19%3ameeting_id%40thread.v2/0",
        },
      });
    });

    await expect(page.getByText("Microsoft Teams")).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Join" }),
    ).toBeVisible();
    await expect(page.getByRole("button", { name: "Dismiss" })).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Record" }),
    ).toBeVisible();

    await page.getByRole("button", { name: "Record" }).click();
    state = await getMockState(page);
    expect(state.promptEvents).toContainEqual({ action: "start" });
    await expect(page.locator(".meeting-prompt-card")).not.toBeVisible();
  });

  test("meeting recording uses the bottom recording overlay", async ({
    page,
  }) => {
    await page.goto("/src/overlay/index.html");

    await page.evaluate(() => {
      (window as any).__EMIT_EVENT__("recording-state-changed", {
        mode: "meeting",
      });
      (window as any).__EMIT_EVENT__("show-overlay", "recording");
    });

    await expect(page.locator(".recording-overlay.meeting")).toBeVisible();
    await expect(page.locator(".overlay-meeting-icon")).toBeVisible();

    await page.evaluate(() => {
      (window as any).__EMIT_EVENT__("show-overlay", "transcribing");
    });

    await expect(page.getByText("Transcribing...")).toBeVisible();
  });

  test("calendar prompt can be dismissed and calendar auth stays independent", async ({
    page,
  }) => {
    await page.getByText("Meetings").click();

    await setMockState(page, { calendarConnected: true });
    await page.reload();
    await page.getByText("Meetings").click();

    await expect(page.locator("text=Google Calendar Prompts")).toBeVisible();
    await expect(
      page.locator("text=Connected for meeting reminders"),
    ).toBeVisible();
    await expect(page.locator(".send-via-google-btn")).not.toBeVisible();

    await page.evaluate(async () => {
      const mod = await import("/src/bindings.ts");
      await mod.commands.setMeetingCalendarPromptsEnabled(true);
    });

    const stateAfterToggle = await getMockState(page);
    expect(stateAfterToggle.calendarPromptsEnabled).toBe(true);
    expect(stateAfterToggle.gmailTasksConnected).toBe(false);

    await page.evaluate(async () => {
      const mod = await import("/src/bindings.ts");
      await mod.commands.dismissMeetingPrompt({
        provider: "Google Meet",
        title: "Design Review",
        source: "GoogleCalendar",
        start_time: new Date().toISOString(),
        join_url: "https://meet.google.com/abc-defg-hij",
      });
    });
    const finalState = await getMockState(page);
    expect(
      finalState.promptEvents.some((event) => event.action === "dismiss"),
    ).toBe(true);
  });

  test("meeting prompt settings persist and calendar prompts stay gated by calendar auth", async ({
    page,
  }) => {
    await page.getByText("Meetings").click();

    const calendarToggle = page.locator(
      "xpath=//h3[normalize-space()='Calendar Prompts']/ancestor::div[contains(@class,'justify-between')][1]//input[@type='checkbox']",
    );

    await expect(calendarToggle).toBeDisabled();

    await page.evaluate(async () => {
      const mod = await import("/src/bindings.ts");
      await mod.commands.changeMeetingDetectionEnabledSetting(true);
      await mod.commands.changeMeetingPromptLeadMinutesSetting(5);
    });

    await page.reload();
    await page.getByText("Meetings").click();

    const state = await getMockState(page);
    expect(state.meetingDetectionEnabled).toBe(true);
    expect(state.meetingPromptLeadMinutes).toBe(5);

    await expect(page.getByText("Prompt Lead Time")).toBeVisible();
  });

  test("oauth-unavailable state disables Google meeting integrations", async ({
    page,
  }) => {
    await setMockState(page, { oauthClientConfigured: false });
    await page.reload();
    await page.getByText("Meetings").click();

    await expect(
      page
        .getByText(
          "Google Calendar is unavailable until a desktop OAuth client ID is configured for this build.",
        )
        .first(),
    ).toBeVisible();

    await expect(page.locator(".google-connect-btn")).toBeDisabled();

    const calendarToggle = page.locator(
      "xpath=//h3[normalize-space()='Calendar Prompts']/ancestor::div[contains(@class,'justify-between')][1]//input[@type='checkbox']",
    );
    await expect(calendarToggle).toBeDisabled();
  });
});
