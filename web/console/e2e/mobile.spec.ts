import { test, expect, switchLanguage } from "./fixtures";

test("mobile navigation and language selector remain usable at 390px", async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await switchLanguage(page, "简体中文");
  await page.getByRole("button", { name: "打开导航菜单" }).click();
  await page.getByRole("menuitem", { name: "项目", exact: true }).last().click();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("项目");
  await expect(page.locator(".mobile-navigation")).not.toBeVisible();
  await switchLanguage(page, "English");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Projects");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.screenshot({ path: testInfo.outputPath("projects-mobile-en.png"), fullPage: true });
});
