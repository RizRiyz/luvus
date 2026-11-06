import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import path from "node:path";

/** Exercise the actual embedded client, not a synthetic DOM or CSS fixture. */
export async function checkBrowserLayout({ cdp, sessionId, evaluate, until, screenshots, checkViewport = false }) {
  const run = (expression) => evaluate(sessionId, expression);
  const streamErrors = [];
  const viewportRequests = new Map();
  const inputActions = [];
  let captureInput = false;
  let acceptedViewport;
  await cdp.call("Network.enable", {}, sessionId);
  cdp.events.on("Network.webSocketFrameSent", (event) => {
    if (event.sessionId !== sessionId) return;
    try {
      const frame = JSON.parse(event.params.response.payloadData);
      if (captureInput && frame.type === "stream.action" && ["type_literal", "paste_text", "send_key"].includes(frame.action) && inputActions.length < 64) {
        inputActions.push({ action: frame.action, params: frame.params });
      }
      if (checkViewport && frame.type === "stream.action" && frame.action === "set_viewport" && viewportRequests.size < 64) {
        viewportRequests.set(frame.id, frame.params);
      }
    } catch { /* Non-JSON network messages are not viewport requests. */ }
  });
  cdp.events.on("Network.webSocketFrameReceived", (event) => {
    if (event.sessionId !== sessionId) return;
    try {
      const frame = JSON.parse(event.params.response.payloadData);
      if (frame.type === "response" && frame.error && streamErrors.length < 8) streamErrors.push(frame.error.code);
      const viewport = viewportRequests.get(frame.id);
      if (viewport && frame.type === "response") {
        viewportRequests.delete(frame.id);
        if (frame.result?.dispatch === "executed") acceptedViewport = viewport;
      }
    } catch { /* Non-JSON network messages are not stream diagnostics. */ }
  });
  const terminalViewport = async () => {
    if (!checkViewport) return;
    const wanted = await run(`(() => {
      const output = document.querySelector('.terminal-output');
      const probe = document.querySelector('.terminal-cell-probe');
      if (!output || !probe) return null;
      const style = getComputedStyle(output), cell = probe.getBoundingClientRect();
      return {
        cols: Math.min(500, Math.floor((output.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)) / (cell.width / 10) + .01)),
        rows: Math.min(300, Math.floor((output.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom)) / cell.height + .01)),
      };
    })()`);
    assert.ok(wanted?.cols >= 20 && wanted?.rows >= 4, "terminal has a measurable viewport");
    try {
      await until(() => acceptedViewport?.cols === wanted.cols && acceptedViewport?.rows === wanted.rows);
    } catch (cause) {
      throw new Error(`Terminal viewport was not applied: ${JSON.stringify({ wanted, acceptedViewport, streamErrors })}`, { cause });
    }
  };
  const viewport = async (width, height, mobile = false) => {
    await cdp.call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile }, sessionId);
    await cdp.call("Emulation.setTouchEmulationEnabled", { enabled: mobile }, sessionId);
    await until(() => run(`innerWidth === ${width} && matchMedia('(min-width: 1024px)').matches === ${!mobile}`));
    await run("new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))");
    if (await run("!!document.querySelector('.terminal-output')")) await terminalViewport();
  };
  const targetPoint = async (selector) => {
    const point = await run(`(() => {
      const target = document.querySelector(${JSON.stringify(selector)});
      if (!target || target.disabled) return null;
      const rect = target.getBoundingClientRect();
      const x = rect.left + rect.width / 2, y = rect.top + rect.height / 2;
      return { x, y, visible: rect.width > 0 && rect.height > 0 && target.contains(document.elementFromPoint(x, y)) };
    })()`);
    assert.ok(point?.visible, `Clickable ${selector}`);
    return point;
  };
  const click = async (selector) => {
    const point = await targetPoint(selector);
    for (const type of ["mousePressed", "mouseReleased"]) {
      await cdp.call("Input.dispatchMouseEvent", { type, x: point.x, y: point.y, button: "left", clickCount: 1 }, sessionId);
    }
  };
  const tap = async (selector) => {
    const point = await targetPoint(selector);
    await cdp.call("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x: point.x, y: point.y }] }, sessionId);
    await cdp.call("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] }, sessionId);
  };
  const key = async (key, code, windowsVirtualKeyCode) => {
    for (const type of ["keyDown", "keyUp"]) await cdp.call("Input.dispatchKeyEvent", { type, key, code, windowsVirtualKeyCode }, sessionId);
  };
  const screenshot = async (name) => {
    if (!screenshots) return;
    const result = await cdp.call("Page.captureScreenshot", { format: "png" }, sessionId);
    await writeFile(path.join(screenshots, `${name}.png`), Buffer.from(result.data, "base64"));
  };
  const sessionDropdown = async () => {
    const open = async () => {
      await click('.hero-session');
      await until(() => run("!!document.querySelector('.session-menu-list') && document.querySelector('.hero-session')?.getAttribute('aria-expanded') === 'true'"));
    };
    const closed = () => until(() => run("!document.querySelector('.session-menu') && document.querySelector('.hero-session')?.getAttribute('aria-expanded') === 'false'"));
    await open();
    await click('.session-menu-head strong');
    assert.equal(await run("!!document.querySelector('.session-menu')"), true, "clicks inside the session dropdown keep it open");
    await click('.hero-center > h1');
    await closed();
    if (await run("matchMedia('(min-width: 1024px)').matches")) {
      await open();
      await click('[data-view-key="pane-filter:true"]');
      await closed();
      assert.equal(await run("document.querySelector('[data-view-key=\"pane-filter:true\"]')?.getAttribute('aria-pressed')"), "true", "outside clicks still activate the clicked control");
      await click('[data-view-key="pane-filter:false"]');
    }
    await open();
    await click('.hero-session');
    await closed();
    await open();
    await key("Escape", "Escape", 27);
    await closed();
    assert.equal(await run("document.activeElement === document.querySelector('.hero-session')"), true, "Escape returns focus to the session trigger");
  };
  const terminalReady = async () => {
    try {
      await until(() => run("!!document.querySelector('.terminal-content')?.textContent"));
      await terminalViewport();
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
    assert.deepEqual(controls.actions, ["Attach files", "Show or hide keyboard", "Control modifier", "Shift modifier", "escape", "tab", "enter", "up", "down", "left", "right", "Control C"], "action bar retains terminal keys and modifiers without a wrap/layout toggle");
    assert.equal(controls.wrapping, "pre-wrap", "default wrapping stays enabled");
    assert.equal(controls.overflowWrap, "anywhere", "long output fits the browser width");
    assert.equal(controls.fits, true, "action bar fits the viewport");
    await screenshot(screenshotName);
    await click('[aria-label="Hide terminal controls"]');
    assert.equal(await run("document.querySelector('.terminal-tools').inert"), true, "collapsed keys are not invisible tab targets");
  };
  const mobileInput = async () => {
    const field = await run(`(() => {
      const input = document.querySelector('.terminal-input-proxy');
      const box = input.getBoundingClientRect(), style = getComputedStyle(input);
      const x = box.left + box.width / 2, y = box.top + box.height / 2;
      return {x, y, height: box.height, width: box.width, touchable: document.elementFromPoint(x, y) === input,
        opacity: style.opacity, pointerEvents: style.pointerEvents, fontSize: style.fontSize,
        editable: !input.readOnly && !input.disabled, coarse: matchMedia('(pointer: coarse)').matches};
    })()`);
    assert.ok(field.height >= 44 && field.width >= 100, "mobile input has a real long-press target");
    assert.equal(field.touchable && field.editable && field.coarse, true);
    assert.equal(field.pointerEvents, "auto");
    assert.equal(field.opacity, "1");
    assert.equal(field.fontSize, "16px", "focusing input does not request iOS small-text zoom");
    await cdp.call("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x: field.x, y: field.y }] }, sessionId);
    await cdp.call("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] }, sessionId);
    await until(() => run("document.activeElement === document.querySelector('.terminal-input-proxy')"));
    inputActions.length = 0;
    captureInput = true;
    const typed = "printf 'WEB_MOBILE_%s\\n' repeatZZZZZ";
    await cdp.call("Input.insertText", { text: typed }, sessionId);
    for (let repeat = 0; repeat < 5; repeat += 1) {
      await cdp.call("Input.dispatchKeyEvent", { type: "keyDown", key: "Backspace", code: "Backspace", windowsVirtualKeyCode: 8, autoRepeat: repeat > 0 }, sessionId);
    }
    await cdp.call("Input.dispatchKeyEvent", { type: "keyUp", key: "Backspace", code: "Backspace", windowsVirtualKeyCode: 8 }, sessionId);
    await key("Enter", "Enter", 13);
    await until(() => run("document.querySelector('.terminal-content')?.textContent?.split(/\\r?\\n/).includes('WEB_MOBILE_repeat')"));
    captureInput = false;
    assert.deepEqual(inputActions.filter(({ action }) => action === "type_literal"), [{ action: "type_literal", params: { text: typed } }], "typing does not leak browser-only guards");
    assert.equal(inputActions.filter(({ action, params }) => action === "send_key" && params.key === "backspace").length, 5, "held Backspace forwards exactly one key per repeat");
    inputActions.length = 0;
    captureInput = true;
    // Exercise the native paste handler without reading or overwriting the
    // operator's system clipboard. The command must reach the actual PTY.
    const pasted = "printf 'WEB_PASTE_%s\\n' pasted";
    assert.equal(await run(`(() => {
      const clipboardData = new DataTransfer();
      clipboardData.setData('text/plain', ${JSON.stringify(pasted)});
      return document.querySelector('.terminal-input-proxy').dispatchEvent(new ClipboardEvent('paste', {clipboardData, bubbles: true, cancelable: true}));
    })()`), false, "native paste is handled by terminal input");
    await key("Enter", "Enter", 13);
    await until(() => run("document.querySelector('.terminal-content')?.textContent?.split(/\\r?\\n/).includes('WEB_PASTE_pasted')"));
    captureInput = false;
    assert.deepEqual(inputActions.filter(({ action }) => action === "paste_text"), [{ action: "paste_text", params: { text: pasted } }], "paste reaches the PTY once with paste semantics");
    inputActions.length = 0;
    captureInput = true;
    const prefix = "printf 'WEB_IME_%s\\n' ";
    await cdp.call("Input.insertText", { text: prefix }, sessionId);
    await cdp.call("Input.imeSetComposition", { text: "日本語", selectionStart: 3, selectionEnd: 3 }, sessionId);
    await cdp.call("Input.insertText", { text: "日本語" }, sessionId);
    await key("Enter", "Enter", 13);
    await until(() => run("document.querySelector('.terminal-content')?.textContent?.split(/\\r?\\n/).includes('WEB_IME_日本語')"));
    captureInput = false;
    assert.equal(inputActions.filter(({ action }) => action === "type_literal").map(({ params }) => params.text).join(""), prefix + "日本語", "native IME commits once without leaking the guards");
    console.log("mobile browser input passed: touchable paste target, native held-Backspace repeats, paste handler, and IME all reach the real debug PTY");
    await screenshot("terminal-mobile-input");
  };
  const mobileModifiers = async () => {
    await tap('[aria-label="Show terminal controls"]');
    await until(() => run("getComputedStyle(document.querySelector('.terminal-tools')).opacity === '1'"));
    await tap('[aria-label="Show or hide keyboard"]');
    assert.equal(await run("document.activeElement === document.querySelector('.terminal-input-proxy')"), false, "keyboard toggle blurs the input instead of re-focusing it on touch");
    await tap('[aria-label="Show or hide keyboard"]');
    assert.equal(await run("document.activeElement === document.querySelector('.terminal-input-proxy')"), true);
    const modifier = async (label) => {
      await tap(`[aria-label="${label} modifier"]`);
      assert.equal(await run(`document.querySelector('[aria-label="${label} modifier"]').getAttribute('aria-pressed')`), "true");
      assert.equal(await run("document.activeElement === document.querySelector('.terminal-input-proxy')"), true, "touching a modifier keeps the keyboard focused");
    };
    const clear = async () => {
      assert.equal(await run("Array.from(document.querySelectorAll('.terminal-modifier')).every(button => button.getAttribute('aria-pressed') === 'false')"), true, "modifiers clear after one terminal key");
    };
    const text = async (value) => cdp.call("Input.insertText", { text: value }, sessionId);
    captureInput = true;
    inputActions.length = 0;
    await text("printf 'WEB_SHIFT_%s\\n' ");
    await modifier("Shift");
    await text("a");
    await clear();
    await text("fter");
    await tap('[aria-label="enter"]');
    await until(() => run("document.querySelector('.terminal-content')?.textContent?.split(/\\r?\\n/).includes('WEB_SHIFT_After')"));
    await text("printf 'WEB_CTRL_%s\\n' wrong");
    await modifier("Control");
    await text("w");
    await clear();
    await text("correct");
    await tap('[aria-label="enter"]');
    await until(() => run("document.querySelector('.terminal-content')?.textContent?.split(/\\r?\\n/).includes('WEB_CTRL_correct')"));
    captureInput = false;
    assert.equal(inputActions.filter(({ params }) => params.key === "ctrl-w").length, 1, "Ctrl plus a phone letter sends one control key");
    assert.ok(inputActions.some(({ action, params }) => action === "type_literal" && params.text.includes("A")), "Shift types an uppercase character");

    // A real PTY consumer negotiates Kitty input, reads an exact bounded byte
    // count, and restores its tty/protocol on exit. No production pane is used.
    for (const flags of [1, 3]) {
      const expected = Buffer.from(flags === 1
        ? "\x1b[13;2u\x1b[13;5u\x1b[13;6u\x1b[Z\x1b[1;5D"
        : "\x1b[13;2u\x1b[13;2:3u\x1b[13;5u\x1b[13;5:3u\x1b[13;6u\x1b[13;6:3u\x1b[Z\x1b[1;5D\x1b[1;5:3D");
      const probe = `sh -c 'mode=$(stty -g); trap "stty $mode; printf \\\"\\\\033[<u\\\"" EXIT; stty -echo -icanon min 1 time 0; printf "\\033[>${flags}uWEB_KEYS_READY_${flags}\\n"; captured=$(od -An -v -t x1 -N ${expected.length} | tr -d " \\n"); printf "WEB_KEYS_HEX_${flags}:%s:END\\n" "$captured"'`;
      await text(probe);
      await tap('[aria-label="enter"]');
      await until(() => run(`document.querySelector('.terminal-content')?.textContent?.split(/\\r?\\n/).includes('WEB_KEYS_READY_${flags}')`));
      for (const modifiers of [["Shift"], ["Control"], ["Control", "Shift"]]) {
        for (const label of modifiers) await modifier(label);
        await tap('[aria-label="enter"]');
        await clear();
      }
      await modifier("Shift"); await tap('[aria-label="tab"]'); await clear();
      await modifier("Control"); await tap('[aria-label="left"]'); await clear();
      const received = () => run(`document.querySelector('.terminal-content')?.textContent?.match(/WEB_KEYS_HEX_${flags}:([0-9a-f\\s]+):END/)?.[1]`);
      await until(received);
      assert.equal((await received()).replace(/\s/g, ""), expected.toString("hex"), "modified keys preserve negotiated identities and release phases at the actual PTY");
    }
    await text("printf 'WEB_KEYS_%s\\n' restored");
    await tap('[aria-label="enter"]');
    await until(() => run("document.querySelector('.terminal-content')?.textContent?.split(/\\r?\\n/).includes('WEB_KEYS_restored')"));
    await modifier("Control");
    await tap('[aria-label="Hide terminal controls"]');
    await clear();
    for (const width of [390, 320]) {
      await viewport(width, 568, true);
      await tap('[aria-label="Show terminal controls"]');
      await until(() => run("getComputedStyle(document.querySelector('.terminal-tools')).opacity === '1'"));
      const targets = await run("Array.from(document.querySelectorAll('.terminal-tools button'), button => { const rect=button.getBoundingClientRect(); return {width:rect.width,height:rect.height,left:rect.left,right:rect.right}; })");
      assert.ok(targets.every(({ width: targetWidth, height, left, right }) => targetWidth >= 44 && height >= 44 && left >= 0 && right <= width), "every mobile key is touch-sized and visible without horizontal scrolling");
      await until(() => run("(() => { const caret=document.querySelector('.terminal-caret'), dock=document.querySelector('.terminal-controls-wrap'); if(!caret) return false; const box=caret.getBoundingClientRect(); return box.top >= 0 && box.bottom <= dock.getBoundingClientRect().top; })()"));
      await screenshot(`terminal-mobile-modifiers-${width}`);
      await tap('[aria-label="Hide terminal controls"]');
    }
    await viewport(390, 844, true);
    console.log("mobile modifiers passed: real touch Ctrl/Shift, one-shot state, shell editing, negotiated Kitty Enter/Tab/arrows, and all 44px keys visible at 320/390px");
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
    if (view === "terminal") await terminalViewport();
    return geometry;
  };

  for (const width of [1024, 1440, 1920]) {
    await viewport(width, 900);
    await desktop("dashboard");
  }
  await sessionDropdown();
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
  await viewport(1024, 900);
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
  await mobileInput();
  await mobileModifiers();
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
  await sessionDropdown();
  await viewport(1440, 900);
  await desktop("dashboard");
  await click('[data-view-key="pane-filter:false"]');
  console.log("browser layout passed: header-free desktop frame, aligned compact rails, both collapsed rails, compact action bar with default wrapping, live terminal input, mobile drawers, session dropdown inside/outside clicks and Escape on desktop/mobile, complementary breakpoint, and short viewport");
  if (checkViewport) console.log("browser viewport sizing passed: window, sidebar, and mobile viewport changes reach the real debug server");
}
