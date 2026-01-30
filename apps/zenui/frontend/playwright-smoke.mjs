import { chromium } from "playwright";

const mode = process.argv[2];
const baseUrl = "http://127.0.0.1:3780";

if (!mode || !["create", "persist"].includes(mode)) {
  console.error("usage: bun playwright-smoke.mjs <create|persist>");
  process.exit(1);
}

const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({ viewport: { width: 1440, height: 980 } });

try {
  await page.goto(baseUrl, { waitUntil: "networkidle" });

  if (mode === "create") {
    await page.getByRole("button", { name: "New Codex" }).click();
    await page.getByText("Session Ready").waitFor({ timeout: 15_000 });

    const composer = page.getByPlaceholder(/Ask anything, continue the current thread/i);
    await composer.fill("Reply with exactly: ZENUI_Codex_OK");
    await page.getByRole("button", { name: "Send" }).click();

    await page.locator(".message.assistant .bubble", { hasText: /^ZENUI_Codex_OK$/ }).waitFor({ timeout: 120_000 });
    const sessionTitle = await page.locator(".session-item strong").first().textContent();
    console.log(`created-session=${sessionTitle ?? "unknown"}`);
  }

  if (mode === "persist") {
    await page.locator(".session-item").first().waitFor({ timeout: 15_000 });
    await page.locator(".message.assistant .bubble", { hasText: /^ZENUI_Codex_OK$/ }).waitFor({ timeout: 15_000 });
    const sessionTitle = await page.locator(".session-item strong").first().textContent();
    console.log(`persisted-session=${sessionTitle ?? "unknown"}`);
  }
} finally {
  await browser.close();
}
