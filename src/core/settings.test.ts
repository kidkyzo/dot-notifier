import { describe, expect, it } from "vitest";
import { defaultSettings, loadSettings, parseMonitoredApps } from "./settings";

describe("user settings", () => {
  it("normalizes monitored apps and removes duplicates", () => {
    expect(parseMonitoredApps(" WhatsApp, Telegram, WhatsApp ,, ")).toEqual(["WhatsApp", "Telegram"]);
  });

  it("uses safe defaults for malformed stored settings", () => {
    expect(loadSettings({ getItem: () => "{" })).toEqual(defaultSettings);
    expect(loadSettings({ getItem: () => JSON.stringify({ peekDurationSeconds: 30 }) })).toEqual(defaultSettings);
  });

  it("defaults empty notification filters to WhatsApp only", () => {
    expect(loadSettings({
      getItem: () => JSON.stringify({ monitoredApps: [] }),
    }).monitoredApps).toEqual(["WhatsApp"]);
  });

  it("keeps the optional extended modules off by default", () => {
    expect(defaultSettings.nowPlayingEnabled).toBe(false);
    expect(defaultSettings.whatsappCardEnabled).toBe(false);
    expect(loadSettings({ getItem: () => "{}" })).toEqual(defaultSettings);
  });

  it("keeps Codex enabled by default and persists an explicit disabled choice", () => {
    expect(defaultSettings.codexEnabled).toBe(true);
    expect(loadSettings({ getItem: () => JSON.stringify({ codexEnabled: false }) }).codexEnabled).toBe(false);
  });
});
