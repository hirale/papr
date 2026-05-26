import { WebviewWindow } from "@tauri-apps/api/webviewWindow";

const BROWSER_LABEL = "papr-browser";
const BROWSER_PROFILE = "browser-profile";

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

export async function openInternalBrowser(url: string, title?: string): Promise<void> {
  const existing = await WebviewWindow.getByLabel(BROWSER_LABEL);
  if (existing) {
    await existing.close().catch(() => {});
    await sleep(120);
  }

  const win = new WebviewWindow(BROWSER_LABEL, {
    url,
    title: title || "Papr Browser",
    width: 1280,
    height: 820,
    minWidth: 760,
    minHeight: 480,
    center: true,
    focus: true,
    dataDirectory: BROWSER_PROFILE,
  });

  await new Promise<void>((resolve, reject) => {
    let settled = false;
    const finish = (fn: () => void) => {
      if (settled) return;
      settled = true;
      fn();
    };
    win.once("tauri://created", () => finish(resolve)).catch(reject);
    win.once("tauri://error", (event) =>
      finish(() => reject(new Error(String(event.payload)))),
    ).catch(reject);
  });
}
