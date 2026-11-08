import { test, expect, type Page } from "@playwright/test";

async function openChat(page: Page) {
  const session = {
    id: "scroll-test", name: "scroll-test", role: "lead", harness: "codex",
    cwd: "/tmp/fixture", model: "fixture", effort: null, status: "idle",
    lifecycle: "active", description: "Scroll regression", note: "",
    created_by_session: null, created_by_name: null, children: [],
  };
  await page.addInitScript(() => localStorage.setItem("kurisu.token", "fixture"));
  // No live server or model calls. Reject any accidental mutation.
  await page.route("**/api/v1/**", async (route) => {
    expect(route.request().method()).toBe("GET");
    const path = new URL(route.request().url()).pathname;
    let ok: unknown = [];
    if (path.endsWith("/status")) ok = { api: "v1", server: null };
    else if (path.endsWith("/sessions")) ok = [session];
    else if (path.endsWith("/messages")) ok = Array.from({ length: 70 }, (_, i) => ({
      seq: i + 1, session: session.id, role: "agent", at: 1700000000 + i,
      text: `Selectable message ${i}. Drag over these words to copy them.\n\n\`\`\`\n${"long_code_".repeat(100)}\n\`\`\``,
    }));
    else if (path.endsWith("/events")) ok = [{
      seq: 1, actor: "user", actor_name: "you", target: session.id,
      field: "task", new: "unbroken_event_value_".repeat(50), at: 1700000000,
    }];
    else if (path.endsWith("/stream")) {
      await route.fulfill({ status: 401 });
      return;
    }
    await route.fulfill({ json: { ok } });
  });
  await page.goto("/#chat=scroll-test");
  await expect(page.locator(".msg")).toHaveCount(70);
}

async function stationaryPage(page: Page) {
  expect(await page.evaluate(() => [scrollX, scrollY])).toEqual([0, 0]);
}

for (const width of [1280, 390]) {
  test(`transcript stays within viewport and owns gestures at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 720 });
    await openChat(page);
    const feed = page.locator(".feed");
    const geometry = await feed.evaluate((el) => ({ width: el.clientWidth, content: el.scrollWidth }));
    expect(geometry.content).toBeLessThanOrEqual(geometry.width);
    await expect(feed).toHaveCSS("overscroll-behavior", "contain");
    await expect(page.locator("html")).toHaveCSS("overscroll-behavior", "none");
    await feed.evaluate((el) => { el.scrollTop = 600; });
    const bounds = (await feed.boundingBox())!;
    await page.mouse.move(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2);
    await page.mouse.wheel(0, 250);
    await expect.poll(() => feed.evaluate((el) => el.scrollTop)).toBeGreaterThan(600);
    await page.mouse.wheel(300, 0);
    await expect.poll(() => feed.evaluate((el) => el.scrollLeft)).toBe(0);
    await stationaryPage(page);

    // At both boundaries, wheel gestures leave the shell/header/composer still.
    const headerY = (await page.locator(".chead").boundingBox())!.y;
    const composerY = (await page.locator(".composer").boundingBox())!.y;
    for (const edge of ["top", "bottom"]) {
      await feed.evaluate((el, edge) => { el.scrollTop = edge === "top" ? 0 : el.scrollHeight; }, edge);
      await page.mouse.wheel(0, edge === "top" ? -900 : 900);
      await stationaryPage(page);
      expect((await page.locator(".chead").boundingBox())!.y).toBe(headerY);
      expect((await page.locator(".composer").boundingBox())!.y).toBe(composerY);
    }

    // Exercise the native scrollbar too. Make its gutter visible on overlay-scrollbar hosts.
    await page.addStyleTag({ content: ".feed::-webkit-scrollbar { width: 14px; } .feed::-webkit-scrollbar-thumb { background: #888; }" });
    const beforeDrag = await feed.evaluate((el) => {
      el.scrollTop = (el.scrollHeight - el.clientHeight) / 2;
      return el.scrollTop;
    });
    const bar = await feed.evaluate((el) => {
      const r = el.getBoundingClientRect();
      return { x: r.x + el.clientWidth + (el.offsetWidth - el.clientWidth) / 2,
        y: r.y, height: el.clientHeight, gutter: el.offsetWidth - el.clientWidth };
    });
    expect(bar.gutter).toBeGreaterThan(0);
    await page.mouse.move(bar.x, bar.y + bar.height / 2);
    await page.mouse.down();
    await page.mouse.move(bar.x, bar.y + bar.height * 0.75, { steps: 15 });
    await page.mouse.up();
    await expect.poll(() => feed.evaluate((el) => el.scrollTop)).toBeGreaterThan(beforeDrag);
    await stationaryPage(page);

    if (width < 760) {
      const cdp = await page.context().newCDPSession(page);
      await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true });
      await feed.evaluate((el) => { el.scrollTop = 300; });
      const x = Math.round(bounds.x + bounds.width / 2);
      const y = Math.round(bounds.y + bounds.height / 2);
      await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y }] });
      for (let distance = 20; distance <= 200; distance += 20) {
        await cdp.send("Input.dispatchTouchEvent", {
          type: "touchMove", touchPoints: [{ x, y: y - distance }],
        });
        await page.waitForTimeout(20);
      }
      await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
      await expect.poll(() => feed.evaluate((el) => el.scrollTop)).toBeGreaterThan(300);
      await stationaryPage(page);
      await cdp.detach();
    }

    // Code blocks scroll independently; normal message text remains selectable.
    const text = page.locator(".msg .text p").first();
    await text.scrollIntoViewIfNeeded();
    const r = (await text.boundingBox())!;
    await page.mouse.move(r.x + 2, r.y + 9);
    await page.mouse.down();
    await page.mouse.move(r.x + Math.min(160, r.width - 4), r.y + 9, { steps: 12 });
    await page.mouse.up();
    expect(await page.evaluate(() => getSelection()?.toString().length)).toBeGreaterThan(5);
    await stationaryPage(page);
    const pre = page.locator(".text pre").first();
    expect(await pre.evaluate((el) => el.scrollWidth > el.clientWidth)).toBe(true);

    // Dragging nonselectable header chrome cannot start a document selection.
    await page.evaluate(() => getSelection()?.removeAllRanges());
    const h = (await page.locator(".chead h1").boundingBox())!;
    await page.mouse.move(h.x + 2, h.y + 8);
    await page.mouse.down();
    await page.mouse.move(h.x + 100, h.y + 8, { steps: 10 });
    await page.mouse.up();
    expect(await page.evaluate(() => getSelection()?.toString())).toBe("");
    await stationaryPage(page);

    if (width < 760) {
      const before = await feed.evaluate((el) => el.scrollTop);
      await page.getByRole("button", { name: "Sessions", exact: true }).click();
      await expect(page.locator(".scrim")).toBeVisible();
      await expect(page.locator("nav")).toHaveCSS("transform", "none");
      await page.mouse.move(width - 10, 350);
      await page.mouse.wheel(0, 400);
      expect(await feed.evaluate((el) => el.scrollTop)).toBe(before);
      await stationaryPage(page);
    }
    const sidebar = (await page.locator(".ws b").boundingBox())!;
    await page.evaluate(() => getSelection()?.removeAllRanges());
    await page.mouse.move(sidebar.x + 2, sidebar.y + 8);
    await page.mouse.down();
    await page.mouse.move(sidebar.x + 70, sidebar.y + 8, { steps: 10 });
    await page.mouse.up();
    expect(await page.evaluate(() => getSelection()?.toString())).toBe("");
    await stationaryPage(page);
    await expect(page.locator(".channels")).toHaveCSS("overscroll-behavior", "contain");
  });
}

test("iPhone-sized touch gestures leave the root fixed and composer inside the safe area", async ({ page }) => {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 390, height: 844, deviceScaleFactor: 3, mobile: true,
  });
  await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 });
  await openChat(page);
  // Chrome has no iPhone notch/home indicator. Inject the values normally supplied
  // by env(safe-area-inset-*) to check layout consumption, not Safari's inset values.
  await page.addStyleTag({ content: ":root { --safe-top: 47px; --safe-bottom: 34px; }" });
  const feed = page.locator(".feed");
  await expect(page.locator("body")).toHaveCSS("position", "fixed");
  await expect(page.locator("main")).toHaveCSS("padding-bottom", "34px");

  async function swipe(selector: string, distance: number) {
    const rect = (await page.locator(selector).boundingBox())!;
    const x = Math.round(rect.x + rect.width / 2);
    const y = Math.round(rect.y + rect.height / 2);
    await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y }] });
    for (let step = 1; step <= 10; step++) {
      await cdp.send("Input.dispatchTouchEvent", {
        type: "touchMove", touchPoints: [{ x, y: y + distance * step / 10 }],
      });
      await page.waitForTimeout(20);
    }
    await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
    // Wait for momentum to settle before starting another gesture.
    await page.waitForTimeout(400);
  }

  async function fixedGeometry(height: number) {
    await stationaryPage(page);
    const g = await page.evaluate(() => {
      const rect = (selector: string) => {
        const r = document.querySelector(selector)!.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom };
      };
      return { rootHeight: document.documentElement.scrollHeight, height: innerHeight,
        scale: devicePixelRatio, header: rect(".chead"), composer: rect(".composer"),
        feed: rect(".feed"), viewportY: visualViewport!.offsetTop };
    });
    expect(g.scale).toBe(3);
    expect(g.height).toBe(height);
    expect(g.rootHeight).toBe(height);
    expect(g.viewportY).toBe(0);
    expect(g.header.top).toBeGreaterThanOrEqual(47);
    expect(g.composer.bottom).toBeLessThanOrEqual(height - 34);
    expect(g.feed.bottom).toBeLessThanOrEqual(g.composer.top);
  }

  for (const height of [844, 660]) {
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 390, height, deviceScaleFactor: 3, mobile: true,
    });
    await feed.evaluate((el) => { el.scrollTop = 400; });
    await swipe(".feed", -150);
    await expect.poll(() => feed.evaluate((el) => el.scrollTop)).toBeGreaterThan(400);
    await fixedGeometry(height);
    for (const selector of [".chead", ".ctools", ".composer textarea"]) {
      const before = await feed.evaluate((el) => el.scrollTop);
      await swipe(selector, -120);
      expect(await feed.evaluate((el) => el.scrollTop)).toBe(before);
      await fixedGeometry(height);
    }
    // Pull down at the transcript's top, and push up at its bottom.
    for (const edge of ["top", "bottom"]) {
      await feed.evaluate((el, edge) => { el.scrollTop = edge === "top" ? 0 : el.scrollHeight; }, edge);
      await swipe(".feed", edge === "top" ? 150 : -150);
      await fixedGeometry(height);
      expect(await feed.evaluate((el) => el.scrollTop >= 0 && el.scrollTop <= el.scrollHeight - el.clientHeight)).toBe(true);
    }
  }
  await cdp.detach();
});
