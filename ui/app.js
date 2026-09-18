const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const statusElement = document.querySelector("#status");
const systemsElement = document.querySelector("#systems");
const directoryElement = document.querySelector("#vars-dir");
const messageElement = document.querySelector("#message");

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

(async () => {
  await listen("status-changed", (event) => render(event.payload));
  render(await invoke("current_status"));
  await invoke("refresh");
})();
