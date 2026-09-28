import { test as base, expect } from "@playwright/test";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import fs from "node:fs/promises";
import path from "node:path";
import { once } from "node:events";

const execute = promisify(execFile);
const clientExe = path.resolve("target\\x86_64-pc-windows-msvc\\release\\zipped-file-client.exe");
const serverExe = path.resolve("target\\x86_64-pc-windows-msvc\\release\\zipped-file-serving.exe");
const headers = { "X-Requested-With": "zipped-file-serving" };
const directory = "O'Brien & $photos";

const test = base.extend({
  server: async ({}, use) => {
    await fs.mkdir("local-test", { recursive: true });
    const root = await fs.mkdtemp(path.resolve("local-test\\profiles-"));
    const served = path.join(root, "served");
    const folder = path.join(root, "Client's downloads");
    const profilesFile = path.join(root, "state", "profiles.json");
    await fs.mkdir(path.join(served, directory), { recursive: true });
    await fs.mkdir(folder);
    await fs.writeFile(path.join(served, directory, "sample.txt"), "downloaded from a generated command");
    let child;
    const server = {
      root, folder, profilesFile,
      async stop() {
        if (child && child.exitCode === null) {
          const stopped = once(child, "exit");
          child.kill();
          await stopped;
        }
        child = undefined;
      },
      async start() {
        child = spawn(serverExe, [served, "--bind", "127.0.0.1", "--port", "0", "--profiles-file", profilesFile], { windowsHide: true });
        let log = "";
        child.stdout.on("data", (data) => { log += data; });
        child.stderr.on("data", (data) => { log += data; });
        for (let i = 0; i < 200; i++) {
          const match = log.match(/127\.0\.0\.1:(\d+)/);
          if (match) { this.url = `http://127.0.0.1:${match[1]}`; return; }
          if (child.exitCode !== null) throw new Error(`Server exited: ${log}`);
          await new Promise((resolve) => setTimeout(resolve, 50));
        }
        throw new Error(`Server did not start: ${log}`);
      }
    };
    try { await server.start(); await use(server); }
    finally { await server.stop(); await fs.rm(root, { recursive: true, force: true }); }
  }
});

async function open(page, server) {
  await page.goto(server.url);
  await page.locator("#directory-command").click();
  await expect(page.locator("#profile-reload")).toBeEnabled();
  await expect(page.locator("#profile-status")).toContainText("Profiles loaded");
}

async function save(page, name, client, folder) {
  await page.locator("#profile-name").fill(name);
  await page.locator("#profile-client").fill(client);
  await page.locator("#profile-folder").fill(folder);
  await page.locator("#profile-save").click();
  await expect(page.locator("#profile-status")).toHaveText("Profile saved on the server.");
  await expect(page.locator("#command-copy")).toBeEnabled();
}

test("generate, copy, execute, choose profiles, and persist across browser/server restart", async ({ page, context, browser, server }) => {
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await open(page, server);
  await expect(page.locator("#command-copy")).toBeDisabled();
  await save(page, "Desktop", clientExe, server.folder);
  const firstId = await page.locator("#profile-select").inputValue();
  await page.getByRole("button", { name: `Client command for ${directory}`, exact: true }).click();
  await expect(page.locator("#command-directory")).toHaveText(directory);
  const command = await page.locator("#command-text").inputValue();
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: server.url });
  await page.locator("#command-copy").click();
  await expect(page.locator("#command-status")).toHaveText("Command copied.");
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(command);
  await execute("pwsh", ["-NoProfile", "-NonInteractive", "-Command", command]);
  expect(await fs.readFile(path.join(server.folder, `${directory}-download`, directory, "sample.txt"), "utf8"))
    .toBe("downloaded from a generated command");

  await page.locator("#profile-new").click();
  await save(page, "Laptop", "C:\\Other tools\\zipped-file-client.exe", "E:\\Downloads");
  await page.locator("#profile-select").selectOption(firstId);
  await expect(page.locator("#profile-folder")).toHaveValue(server.folder);
  await expect(page.locator("#command-text")).toHaveValue(command);
  await page.locator("#command-output").fill("another-copy");
  await expect(page.locator("#command-text")).toHaveValue(/another-copy/);
  const fresh = await browser.newContext();
  try {
    const other = await fresh.newPage();
    await open(other, server);
    await expect(other.locator("#profile-count")).toHaveText("2 / 99 profiles");
    await expect(other.locator("#profile-client")).toHaveValue(clientExe);
  } finally { await fresh.close(); }
  await server.stop();
  await server.start();
  await open(page, server);
  await expect(page.locator("#profile-count")).toHaveText("2 / 99 profiles");
  await expect(page.locator("#profile-folder")).toHaveValue(server.folder);
  await save(page, "Desktop renamed", clientExe, server.folder);
  expect(JSON.parse(await fs.readFile(server.profilesFile, "utf8")).profiles[0].name).toBe("Desktop renamed");
  expect(errors).toEqual([]);
});

test("99 profile cap blocks creation but allows edit and delete", async ({ page, request, server }) => {
  for (let revision = 0; revision < 99; revision++) {
    const response = await request.post(`${server.url}/api/profiles`, {
      headers,
      data: { revision, name: `Profile ${revision}`, client_path: clientExe, download_folder: server.folder }
    });
    expect(response.ok()).toBeTruthy();
  }
  await open(page, server);
  await expect(page.locator("#profile-count")).toHaveText("99 / 99 profiles");
  await expect(page.locator("#profile-new")).toBeDisabled();
  await save(page, "Renamed at 99", clientExe, server.folder);
  const response = await request.post(`${server.url}/api/profiles`, {
    headers, data: { revision: 100, name: "100th", client_path: clientExe, download_folder: server.folder }
  });
  expect(response.status()).toBe(409);
  page.once("dialog", (dialog) => dialog.accept());
  await page.locator("#profile-delete").click();
  await expect(page.locator("#profile-count")).toHaveText("98 / 99 profiles");
  await expect(page.locator("#profile-new")).toBeEnabled();
});

test("stale edits, invalid names, clipboard fallback, and profile-load errors are visible", async ({ page, browser, server }) => {
  await open(page, server);
  await save(page, "First", clientExe, server.folder);
  const otherContext = await browser.newContext();
  try {
    const other = await otherContext.newPage();
    await open(other, server);
    await save(page, "Changed", clientExe, server.folder);
    await other.locator("#profile-name").fill("Stale");
    await other.locator("#profile-save").click();
    await expect(other.locator("#profile-status")).toContainText("Profiles changed in another browser");
    await expect(other.locator("#command-copy")).toBeDisabled();
    other.once("dialog", (dialog) => dialog.accept());
    await other.locator("#profile-reload").click();
    await expect(other.locator("#profile-name")).toHaveValue("Changed");
  } finally { await otherContext.close(); }
  await page.locator("#command-output").fill("../escape");
  await expect(page.locator("#command-copy")).toBeDisabled();
  await expect(page.locator("#command-status")).toContainText("without path separators");
  await page.locator("#command-output").fill("valid-output");
  await page.evaluate(() => {
    Object.defineProperty(navigator, "clipboard", { value: undefined, configurable: true });
    document.execCommand = () => false;
  });
  await page.locator("#command-copy").click();
  await expect(page.locator("#command-status")).toContainText("press Ctrl+C");
  expect(await page.locator("#command-text").evaluate((element) => element.selectionEnd - element.selectionStart))
    .toBe((await page.locator("#command-text").inputValue()).length);
  await page.route("**/api/profiles", (route) => route.fulfill({ status: 500, json: { error: "Storage unavailable" } }));
  await page.locator("#profile-reload").click();
  await expect(page.locator("#profile-status")).toContainText("Storage unavailable");
  await expect(page.locator("#profile-save")).toBeDisabled();
  await expect(page.locator("#command-copy")).toBeDisabled();
  await expect(page.getByRole("link", { name: `${directory}/`, exact: true })).toBeVisible();
});
