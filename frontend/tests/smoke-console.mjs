import { chromium } from "playwright";

const browser = await chromium.launch();
const context = await browser.newContext();
const page = await context.newPage();

const logs = [];
page.on("console", (msg) => {
  logs.push(`[${msg.type()}] ${msg.text()}`);
});
page.on("pageerror", (err) => {
  logs.push(`[pageerror] ${err.message}\n${err.stack ?? ""}`);
});

try {
  await page.goto("http://localhost:5180/", { waitUntil: "networkidle", timeout: 15000 });
} catch (err) {
  logs.push(`[nav-error] ${err.message}`);
}

await new Promise((r) => setTimeout(r, 800));

const info = await page.evaluate(() => {
  const root = document.getElementById("root");
  if (!root) return { error: "no root" };
  const rect = root.getBoundingClientRect();
  const children = Array.from(root.children).map((el) => ({
    tag: el.tagName,
    class: el.className,
    children: el.children.length,
    rect: el.getBoundingClientRect(),
  }));
  // Descend one level to find the main layout children
  const deepChildren = [];
  function walk(el, depth) {
    if (depth > 3) return;
    for (const child of el.children) {
      const r = child.getBoundingClientRect();
      deepChildren.push({
        tag: child.tagName,
        slot: child.getAttribute("data-slot") ?? "",
        class: (child.className || "").slice(0, 120),
        w: Math.round(r.width),
        h: Math.round(r.height),
      });
      walk(child, depth + 1);
    }
  }
  walk(root, 0);
  return { rootRect: rect, innerHTMLLen: root.innerHTML.length, children: children.length, deep: deepChildren.slice(0, 30) };
});

console.log("=== console logs ===");
for (const line of logs) console.log(line);
console.log("=== root info ===");
console.log(JSON.stringify(info, null, 2));

await browser.close();
