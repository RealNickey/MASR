import { expect, test } from "@playwright/test";
import { getMockState, setMockState, setupMocks } from "./helpers";

test.describe("Meeting summary renderer", () => {
  test("renders flexible Markdown with company-meeting affordances and evidence pills", async ({
    page,
  }) => {
    await setupMocks(page);
    await page.goto("/");
    await page.getByText("Meetings").click();

    const state = await getMockState(page);
    await setMockState(page, {
      historyEntries: [
        {
          ...state.historyEntries[0],
          post_processed_text: `# Product review

## Decisions
- Approved the staged release [[cite:SEG-000]]

## Action Items
- [ ] Publish the release notes

> [!WARNING] The rollback plan is still unresolved.

| Owner | Due |
| --- | --- |
| Anu | Unknown |`,
          transcript_segments: [
            {
              start_ms: 12_000,
              end_ms: 15_000,
              source: "microphone",
              text: "നാളെ റിലീസ് ചെയ്യാം",
              confidence: 0.98,
            },
          ],
        },
      ],
    });

    await page.reload();
    await page.getByText("Meetings").click();

    await expect(
      page.getByRole("heading", { name: "Product review" }),
    ).toBeVisible();
    await expect(page.getByText("Approved the staged release")).toBeVisible();
    await expect(page.getByText("Warning")).toBeVisible();
    await expect(page.getByRole("cell", { name: "Unknown" })).toBeVisible();

    const actionToggle = page.getByRole("button", {
      name: "Mark action item complete",
    });
    await expect(actionToggle).toBeVisible();
    await actionToggle.click();
    const completedActionToggle = page.getByRole("button", {
      name: "Mark action item incomplete",
    });
    await expect(completedActionToggle).toHaveAttribute("aria-pressed", "true");

    const citation = page.getByLabel("Transcript evidence SEG-000");
    await expect(citation).toBeVisible();
    await citation.hover();
    await expect(page.getByText("നാളെ റിലീസ് ചെയ്യാം")).toBeVisible();
  });
});
