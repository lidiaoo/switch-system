const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const statusElement = document.querySelector("#status");
const systemsElement = document.querySelector("#systems");
const directoryElement = document.querySelector("#vars-dir");
const messageElement = document.querySelector("#message");
const autostartElement = document.querySelector("#autostart");
const closeActionElement = document.querySelector("#close-action");
const settingsMessageElement = document.querySelector("#settings-message");
const autostartStateElement = document.querySelector("#autostart-state");
const loginItemsElement = document.querySelector("#login-items");

const autostartStateLabels = new Map([
  ["enabled", "已加入系统登录项"],
  ["requires-approval", "已注册，待系统在登录项中批准"],
  ["disabled", "未加入系统登录项"],
  ["unavailable", "登录项状态不可用"],
]);

const closeActionLabels = new Map([
  ["tray", "最小化到托盘"],
  ["quit", "退出程序"],
]);

const switchButtonLabel = new Map([
  ["linux", "下次启动 Linux"],
  ["windows", "下次启动 Windows"],
  ["macos", "下次启动 macOS"],
]);

function render(status) {
  statusElement.textContent = status.message;
  messageElement.textContent = status.error || "";
  directoryElement.value = status.varsDir || "";
  systemsElement.replaceChildren();

  for (const item of status.variables) {
    const card = document.createElement("article");
    card.className = [
      "system-card",
      item.active ? "active" : "",
      item.exists ? "" : "unavailable",
    ].filter(Boolean).join(" ");

    const name = document.createElement("div");
    name.className = "system-name";
    const title = document.createElement("strong");
    title.textContent = item.active ? `● ${item.displayName}` : item.displayName;
    const source = document.createElement("small");
    source.textContent = item.exists ? item.sourceName : `${item.sourceName}（不存在）`;
    name.append(title, source);

    const button = document.createElement("button");
    button.type = "button";
    button.textContent = item.active ? "已选择" : switchButtonLabel.get(item.system);
    button.disabled = !item.exists || Boolean(status.error);
    button.addEventListener("click", async () => {
      button.disabled = true;
      statusElement.textContent = `正在设置 ${item.displayName}…`;
      try {
        await invoke("switch_system", { system: item.system });
      } catch (error) {
        messageElement.textContent = String(error);
      }
    });

    card.append(name, button);
    systemsElement.append(card);
  }
}

document.querySelector("#refresh").addEventListener("click", () => refreshStatus());

async function refreshStatus() {
  messageElement.textContent = "";
  statusElement.textContent = "正在扫描…";
  try {
    render(await invoke("refresh"));
  } catch (error) {
    messageElement.textContent = String(error);
  }
}

document.querySelector("#use-directory").addEventListener("click", async () => {
  messageElement.textContent = "";
  statusElement.textContent = "正在检查目录…";
  try {
    const path = directoryElement.value.trim();
    render(await invoke("set_vars_dir", { path: path || null }));
  } catch (error) {
    messageElement.textContent = String(error);
  }
});

function renderSettings(settings, fallbackMessage = "") {
  autostartElement.checked = Boolean(settings.autostart);
  closeActionElement.value = settings.closeAction === "quit" ? "quit" : "tray";
  autostartStateElement.textContent =
    autostartStateLabels.get(settings.autostartState) || "";
  const hint = settings.autostartHint || "";
  settingsMessageElement.textContent = hint || fallbackMessage;
  loginItemsElement.classList.toggle(
    "hidden",
    settings.autostartState !== "requires-approval",
  );
}

function setSettingsBusy(busy) {
  autostartElement.disabled = busy;
  closeActionElement.disabled = busy;
}

async function loadSettings() {
  try {
    renderSettings(await invoke("get_settings"));
  } catch (error) {
    settingsMessageElement.textContent = String(error);
  }
}

async function saveSettings(command, args, successText) {
  setSettingsBusy(true);
  settingsMessageElement.textContent = "";
  try {
    renderSettings(await invoke(command, args), successText);
  } catch (error) {
    settingsMessageElement.textContent = String(error);
    await loadSettings();
  } finally {
    setSettingsBusy(false);
  }
}

autostartElement.addEventListener("change", () =>
  saveSettings(
    "set_autostart",
    { enabled: autostartElement.checked },
    autostartElement.checked ? "已开启开机启动" : "已关闭开机启动",
  ));

closeActionElement.addEventListener("change", () =>
  saveSettings(
    "set_close_action",
    { action: closeActionElement.value },
    `已设置：点击 ❌ ${closeActionLabels.get(closeActionElement.value)}`,
  ));

loginItemsElement.addEventListener("click", async () => {
  settingsMessageElement.textContent = "";
  try {
    await invoke("open_login_items_settings");
    settingsMessageElement.textContent = "已打开系统设置的登录项面板";
  } catch (error) {
    settingsMessageElement.textContent = String(error);
  }
});

(async () => {
  await listen("status-changed", (event) => render(event.payload));
  render(await invoke("current_status"));
  await loadSettings();
  await invoke("refresh");
})();
