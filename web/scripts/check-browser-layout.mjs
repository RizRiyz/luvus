import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import path from "node:path";

/** Exercise the actual embedded client, not a synthetic DOM or CSS fixture. */
export async function checkBrowserLayout({ cdp, sessionId, evaluate, until, screenshots }) {
  const run = (expression) => evaluate(sessionId, expression);
  const streamErrors = [];
  await cdp.call("Network.enable", {}, sessionId);
  cdp.events.on("Network.webSocketFrameReceived", (event) => {
    if (event.sessionId !== sessionId) return;
    try {
      const frame = JSON.parse(event.params.response.payloadData);
      if (frame.type === "response" && frame.error && streamErrors.length < 8) streamErrors.push(frame.error.code);
    } catch { /* Non-JSON network messages are not stream diagnostics. */ }
  });
  const viewport = async (width, height, mobile = false) => {
    await cdp.call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile }, sessionId);
    await cdp.call("Emulation.setTouchEmulationEnabled", { enabled: mobile }, sessionId);
    await until(() => run(`innerWidth === ${width} && matchMedia('(min-width: 1024px)').matches === ${!mobile}`));
    await run("new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))");
  };
  const click = async (selector) => {
    const point = await run(`(() => {
      const target = document.querySelector(${JSON.stringify(selector)});
      if (!target || target.disabled) return null;
      const rect = target.getBoundingClientRect();
      const x = rect.left + rect.width / 2, y = rect.top + rect.height / 2;
      return { x, y, visible: rect.width > 0 && rect.height > 0 && target.contains(document.elementFromPoint(x, y)) };
    })()`);
    assert.ok(point?.visible, `Clickable ${selector}`);
    for (const type of ["mousePressed", "mouseReleased"]) {
      await cdp.call("Input.dispatchMouseEvent", { type, x: point.x, y: point.y, button: "left", clickCount: 1 }, sessionId);
    }
  };
  const key = async (key, code, windowsVirtualKeyCode) => {
    for (const type of ["keyDown", "keyUp"]) await cdp.call("Input.dispatchKeyEvent", { type, key, code, windowsVirtualKeyCode }, sessionId);
  };
  const screenshot = async (name) => {
    if (!screenshots) return;
    const result = await cdp.call("Page.captureScreenshot", { format: "png" }, sessionId);
    await writeFile(path.join(screenshots, `${name}.png`), Buffer.from(result.data, "base64"));
  };
  const terminalReady = async () => {
    try {
      await until(() => run("!!document.querySelector('.terminal-content')?.textContent"));
    } catch (cause) {
      await screenshot("terminal-not-ready");
      const state = await run("({ open: !!document.querySelector('.terminal-screen'), status: document.querySelector('.terminal-status')?.textContent, error: document.querySelector('.toast')?.textContent })");
      throw new Error(`Isolated terminal not ready: ${JSON.stringify({ ...state, streamErrors })}`, { cause });
    }
  };
  const terminalControls = async (screenshotName) => {
    await click('[aria-label="Show terminal controls"]');
    await until(() => run("getComputedStyle(document.querySelector('.terminal-tools')).opacity === '1'"));
    const controls = await run(`(() => {
      const bar = document.querySelector('.terminal-controls').getBoundingClientRect();
      const output = getComputedStyle(document.querySelector('.terminal-output'));
      return {
        actions: Array.from(document.querySelectorAll('.terminal-tools button'), button => button.getAttribute('aria-label')),
        wrapping: output.whiteSpace, overflowWrap: output.overflowWrap,
        fits: bar.left >= 0 && bar.right <= innerWidth + 1,
      };
    })()`);
    assert.deepEqual(controls.actions, ["Attach files", "Show or hide keyboard", "escape", "tab", "up", "down", "left", "right", "Control C"], "action bar retains terminal controls without a wrap/layout toggle");
    assert.equal(controls.wrapping, "pre-wrap", "default wrapping stays enabled");
    assert.equal(controls.overflowWrap, "anywhere", "long output fits the browser width");
    assert.equal(controls.fits, true, "action bar fits the viewport");
    await screenshot(screenshotName);
    await click('[aria-label="Hide terminal controls"]');
  };
  const breakpoint = async (view) => {
    // CDP accepts integer viewport widths. Also inspect the browser-parsed
    // stylesheet to prove its mobile query covers fractional widths below
    // the exact desktop threshold used by the application's matchMedia.
    const mobileQuery = await run(`Array.from(document.styleSheets).flatMap(sheet => Array.from(sheet.cssRules))
      .find(rule => rule instanceof CSSMediaRule && Array.from(rule.cssRules)
        .some(style => style.selectorText === '.pane-navigation-open .terminal-navigation'))?.conditionText`);
    assert.equal(mobileQuery, "not all and (min-width: 1024px)", "mobile CSS is the exact complement of desktop routing");
    const previous = await run("({ width: innerWidth, height: innerHeight })");
    await viewport(1023, 768, true);
    await click(`.${view}-header [aria-label="Open navigation"]`);
    await until(() => run(`getComputedStyle(document.querySelector('#${view === "dashboard" ? "dashboard" : "terminal"}-navigation')).display === 'flex'`));
    await key("Escape", "Escape", 27);
    await viewport(1024, 768);
    await desktop(view);
    await viewport(previous.width, previous.height);
  };
  const desktop = async (view, workspacesCollapsed = false, panesCollapsed = false) => {
    const rootSelector = view === "dashboard" ? ".dashboard-layout" : ".terminal-screen";
    await until(() => run(`!!document.querySelector('${rootSelector} > .terminal-sidebar')`));
    const geometry = await run(`(() => {
      const root = document.querySelector('${rootSelector}');
      const rect = (selector) => {
        const node = root.querySelector(selector), box = node.getBoundingClientRect();
        return { left: box.left, right: box.right, top: box.top, bottom: box.bottom, width: box.width, height: box.height };
      };
      const frame = root.querySelector('.web-content-frame');
      return {
        frame: rect('.web-content-frame'), header: rect('.${view === "dashboard" ? "dashboard-header" : "terminal-header"}'),
        main: rect('.${view === "dashboard" ? "dashboard" : "terminal-main"}'),
        workspaces: rect('.terminal-workspace-sidebar'), panes: rect('.terminal-sidebar'),
        workspaceHeading: rect('.terminal-workspace-topbar'), paneHeading: rect('.terminal-sidebar > .terminal-sidebar-heading'),
        workspaceTitle: rect('.terminal-workspace-sidebar > .terminal-sidebar-heading'), paneToolbar: rect('.terminal-sidebar-toolbar'),
        radius: getComputedStyle(frame).borderTopLeftRadius,
        rightRadius: getComputedStyle(frame).borderTopRightRadius,
        headerHidden: getComputedStyle(root.querySelector('.${view === "dashboard" ? "dashboard-header" : "terminal-header"}')).display === 'none',
        hasContextLabel: !!root.querySelector('.app-header-context'),
        fits: document.documentElement.scrollWidth <= innerWidth,
      };
    })()`);
    const sameEdge = (a, b, message) => assert.ok(Math.abs(a - b) <= 1.1, `${message}: ${a} vs ${b}`);
    assert.equal(geometry.radius, "16px");
    assert.equal(geometry.rightRadius, geometry.radius, "matching rounded top corners beside both rails");
    assert.equal(geometry.fits, true, "no document horizontal overflow");
    assert.equal(geometry.headerHidden, true, "desktop has no header bar");
    assert.equal(geometry.hasContextLabel, false, "no redundant session/view label");
    assert.equal(geometry.header.height, 0, "hidden mobile navigation reserves no desktop space");
    sameEdge(geometry.frame.left, geometry.workspaces.right, "flat workspace rail meets the frame");
    sameEdge(geometry.main.top, geometry.frame.top + 1, "main content starts at the frame border without a header gap");
    sameEdge(geometry.main.left, geometry.frame.left + 1, "content fills the center frame horizontally");
    sameEdge(geometry.main.right, geometry.frame.right - 1, "content fits between the sidebars");
    sameEdge(geometry.main.bottom, geometry.frame.bottom - 1, "content fills the center frame vertically");
    sameEdge(geometry.frame.right, geometry.panes.left, "flat pane rail meets the frame");
    sameEdge(geometry.workspaces.top, geometry.panes.top, "both sidebars start at the same height");
    sameEdge(geometry.panes.top, geometry.frame.top, "pane rail is not lowered by the header");
    sameEdge(geometry.workspaceHeading.top, geometry.paneHeading.top, "sidebar headings align");
    sameEdge(geometry.paneHeading.height, 48, "compact pane heading row");
    if (!workspacesCollapsed) {
      sameEdge(geometry.workspaceHeading.bottom, geometry.paneHeading.bottom, "expanded sidebar heading rows have equal height");
      sameEdge(geometry.workspaceTitle.top - geometry.workspaceHeading.bottom, 8, "compact logo-to-workspaces gap");
    }
    if (!panesCollapsed) sameEdge(geometry.paneToolbar.top - geometry.paneHeading.bottom, 8, "compact agents-to-filters gap");
    sameEdge(geometry.panes.bottom, geometry.frame.bottom, "pane rail fits the frame height");
    assert.ok(geometry.workspaces.width <= 224, "workspace rail stays compact on wide screens");
    if (workspacesCollapsed) sameEdge(geometry.workspaces.width, 56, "collapsed workspace rail");
    if (panesCollapsed) sameEdge(geometry.panes.width, 56, "collapsed pane rail");
    return geometry;
  };

  for (const width of [1024, 1440, 1920]) {
    await viewport(width, 900);
    await desktop("dashboard");
  }
  await breakpoint("dashboard");
  await screenshot("dashboard-desktop");
  await click('[aria-label="Collapse workspaces sidebar"]');
  await desktop("dashboard", true, false);
  await click('[aria-label="Collapse panes sidebar"]');
  await desktop("dashboard", true, true);
  await click('[aria-label="Expand workspaces sidebar"]');
  await desktop("dashboard", false, true);
  await click('[aria-label="Expand panes sidebar"]');
  await click('.terminal-workspace-label:not(:disabled)');
  await terminalReady();
  await desktop("terminal");
  await breakpoint("terminal");
  await click('[data-view-key="pane-filter:true"]');
  await until(() => run("!!document.querySelector('.terminal-sidebar-pane.active')"));
  await screenshot("terminal-desktop");
  await terminalControls("terminal-desktop-controls");
  await click('[aria-label="Collapse workspaces sidebar"]');
  await desktop("terminal", true, false);
  await click('[aria-label="Collapse panes sidebar"]');
  await desktop("terminal", true, true);
  await screenshot("terminal-collapsed");
  await click('.terminal-workspace-topbar [aria-label="Luvus overview"]');
  await desktop("dashboard", true, true);
  await click('[aria-label="Expand workspaces sidebar"]');
  await click('[aria-label="Expand panes sidebar"]');
  await click('.terminal-workspace-label:not(:disabled)');
  await terminalReady();
  await viewport(1024, 768);
  await desktop("terminal");

  // A real browser keyboard action reaches the isolated debug server's shell.
  await run("document.querySelector('.terminal-input-proxy').focus()");
  await cdp.call("Input.insertText", { text: "printf 'WEB_LAYOUT_%s\\n' ok" }, sessionId);
  await key("Enter", "Enter", 13);
  await until(() => run("document.querySelector('.terminal-content')?.textContent?.includes('WEB_LAYOUT_ok')"));

  await viewport(390, 844, true);
  await until(() => run("!!document.querySelector('#terminal-navigation > .terminal-sidebar')"));
  assert.equal(await run("getComputedStyle(document.querySelector('.web-content-frame')).display"), "contents");
  await terminalControls("terminal-mobile-controls");
  await click('.terminal-header [aria-label="Open navigation"]');
  await until(() => run("document.querySelector('#terminal-navigation')?.contains(document.activeElement)"));
  assert.equal(await run("document.querySelector('.terminal-header').inert && document.querySelector('.terminal-main').inert"), true);
  await key("Tab", "Tab", 9);
  assert.equal(await run("document.querySelector('#terminal-navigation').contains(document.activeElement)"), true, "drawer traps keyboard focus");
  await screenshot("terminal-mobile-drawer");
  await key("Escape", "Escape", 27);
  assert.equal(await run("!document.querySelector('.terminal-header').inert && !document.querySelector('.terminal-main').inert"), true);
  await viewport(390, 480, true);
  assert.equal(await run("document.querySelector('.terminal-main').getBoundingClientRect().bottom <= innerHeight + 1"), true, "short mobile viewport keeps the terminal bounded");
  await viewport(390, 844, true);
  await screenshot("terminal-mobile");
  await click('.terminal-header [aria-label="Luvus overview"]');
  await until(() => run("!!document.querySelector('.dashboard-layout')"));
  await click('.dashboard-header [aria-label="Open navigation"]');
  await until(() => run("document.querySelector('#dashboard-navigation')?.contains(document.activeElement)"));
  assert.equal(await run("!!document.querySelector('#dashboard-navigation > .terminal-sidebar') && document.querySelector('.dashboard-header').inert"), true);
  await screenshot("dashboard-mobile-drawer");
  await key("Escape", "Escape", 27);
  await viewport(1440, 900);
  await desktop("dashboard");
  await click('[data-view-key="pane-filter:false"]');
  console.log("browser layout passed: header-free desktop frame, aligned compact rails, both collapsed rails, compact action bar with default wrapping, live terminal input, mobile drawers, complementary breakpoint, and short viewport");
}
