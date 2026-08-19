#!/usr/bin/env bun

import type { Subprocess } from "bun";
import { mkdtempSync, watch, writeFileSync, type FSWatcher } from "node:fs";
import { open as openFile, stat as statFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createElement } from "react";
import { render, type Instance } from "ink";
import { DevStore, type Tab } from "./dev/store";
import { DevUi, type DevController } from "./dev/ui";

const root = resolve(import.meta.dir, "..");
const isMacOS = process.platform === "darwin";
const appName = "WakuWaku Debug";
const targetDir = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const appPath = isMacOS
  ? join(targetDir, "debug/WakuWaku Debug.app")
  : join(targetDir, "debug/wakuwaku");
const daemonPath = join(
  targetDir,
  `debug/wakuwaku-debug-daemon${process.platform === "win32" ? ".exe" : ""}`,
);
const watchedDirectories = ["src", "crates", "assets", "resources", "locales"];
const watchedFiles = ["Cargo.toml", "Cargo.lock", "build.rs"];
const rebuildDebounceMs = 1_000;
type BuildTarget = "app" | "daemon";

if (!process.stdin.isTTY || !process.stdout.isTTY) {

  console.error("[wakuwaku-dev] dev.ts is a terminal UI; run it from an interactive terminal.");
  process.exit(1);
}

type Watcher = FSWatcher & {
  on(event: "error", listener: (error: Error) => void): void;
};

const store = new DevStore();
const appTab = store.openTab("app", "App");
const daemonTab = store.openTab("daemon", "Daemon");
store.activate(appTab);
let instance: Instance | undefined;

let app: Subprocess<"ignore", "pipe", "pipe"> | undefined;
let appTails: Array<() => Promise<void>> = [];
let stopping = false;
let quitStarted = false;
let building = false;
let queuedBuild: BuildTarget | undefined;
let debouncedBuild: BuildTarget | undefined;
let appChangeRevision = 0;
let daemonChangeRevision = 0;
let rebuildTimer: ReturnType<typeof setTimeout> | undefined;
let daemonEverBuilt = false;
let everLaunched = false;
let finalExitCode: number | null = null;
let exitMessage: string | undefined;
const watchers: FSWatcher[] = [];

async function pumpStream(
  stream: ReadableStream<Uint8Array> | number,
  tab: Tab,
): Promise<void> {
  if (typeof stream === "number") return;
  const decoder = new TextDecoder();
  const reader = stream.getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value) store.ingest(tab, decoder.decode(value, { stream: true }));
    }
    const tail = decoder.decode();
    if (tail !== "") store.ingest(tab, tail);
  } finally {
    reader.releaseLock();
  }
}

function runCommand(command: string[], tab: Tab): Promise<number> {
  const proc = Bun.spawn(command, { cwd: root, stdout: "pipe", stderr: "pipe" });
  void pumpStream(proc.stdout, tab);
  void pumpStream(proc.stderr, tab);
  return proc.exited;
}

/** Poll a log file that `open --stdout/--stderr` writes the app's output to. */
function tailFile(path: string, tab: Tab): () => Promise<void> {
  let offset = 0;
  const decoder = new TextDecoder();
  const poll = async (): Promise<void> => {
    try {
      const info = await statFile(path);
      if (!info.isFile()) return;
      if (info.size < offset) offset = 0;
      if (info.size === offset) return;
      const handle = await openFile(path, "r");
      try {
        const length = info.size - offset;
        const buffer = Buffer.alloc(length);
        const { bytesRead } = await handle.read(buffer, 0, length, offset);
        offset += bytesRead;
        if (bytesRead > 0) {
          store.ingest(tab, decoder.decode(buffer.subarray(0, bytesRead), { stream: true }));
        }
      } finally {
        await handle.close();
      }
    } catch {
      // The log file may not exist yet; the next tick retries.
    }
  };
  void poll();
  const timer = setInterval(() => void poll(), 250);
  return async () => {
    clearInterval(timer);
    await poll();
  };
}

async function buildDaemon(): Promise<boolean> {
  if (daemonTab.lines.length > 0 || daemonTab.partial !== "") {
    store.pushLine(daemonTab, "── daemon rebuild ──");
  }
  store.setStatus(daemonTab, "building");
  store.setStatusLine("Building daemon…");
  const code = await runCommand(
    [
      "cargo",
      "build",
      "--package",
      "wakuwaku-daemon",
      "--features",
      "dev-binary",
      "--bin",
      "wakuwaku-debug-daemon",
    ],
    daemonTab,
  );
  if (stopping) return false;
  if (code !== 0) {
    store.setStatus(daemonTab, daemonEverBuilt ? "warn" : "error");
    store.setStatusLine("Daemon build failed; keeping the current daemon running.", "warn");
    store.activate(daemonTab);
    return false;
  }
  daemonEverBuilt = true;
  store.setStatus(daemonTab, "running");
  return true;
}

/** A failed app build keeps the previous app running when there is one. */
function finishFailedAppBuild(): void {
  const alive = app !== undefined;
  store.setStatus(appTab, alive ? "warn" : "error");
  store.setStatusLine(
    alive ? "Build failed; keeping the current app open." : "Build failed.",
    alive ? "warn" : "error",
  );
  store.activate(appTab);
}

async function runAppBuild(): Promise<boolean> {
  if (appTab.lines.length > 0 || appTab.partial !== "") {
    store.pushLine(appTab, "── rebuilding ──");
  }
  store.setStatus(appTab, "building");
  if (!(await buildDaemon())) {
    finishFailedAppBuild();
    return false;
  }
  store.setStatusLine(isMacOS ? "Building app bundle…" : "Building app…");
  const command = isMacOS
    ? [join(root, "scripts/bundle.sh"), "debug"]
    : [
        "cargo",
        "build",
        "--package",
        "wakuwaku",
        "--bin",
        "wakuwaku",
        "--bin",
        "wakuwaku_js_repl",
      ];
  const code = await runCommand(command, appTab);
  if (stopping) return false;
  if (code !== 0) {
    finishFailedAppBuild();
    return false;
  }
  return true;
}

function launchApp(): void {
  store.pushLine(appTab, `── launching ${appPath} ──`);
  let command: string[];
  if (isMacOS) {
    const logDir = mkdtempSync(join(tmpdir(), "wakuwaku-dev-"));
    const outPath = join(logDir, "out.log");
    const errPath = join(logDir, "err.log");
    writeFileSync(outPath, "");
    writeFileSync(errPath, "");
    command = ["open", "-n", "-W", "--stdout", outPath, "--stderr", errPath, appPath];
    appTails = [tailFile(outPath, appTab), tailFile(errPath, appTab)];
  } else {
    command = [appPath];
    appTails = [];
  }
  const launchedApp = Bun.spawn(command, {
    cwd: root,
    env: { ...process.env, WAKUWAKU_DAEMON_PATH: daemonPath },
    stdout: "pipe",
    stderr: "pipe",
  });
  void pumpStream(launchedApp.stdout, appTab);
  void pumpStream(launchedApp.stderr, appTab);
  app = launchedApp;
  everLaunched = true;
  store.markRunning(appTab);
  void launchedApp.exited.then((exitCode) => void handleAppExit(launchedApp, exitCode));
}

async function handleAppExit(
  launchedApp: Subprocess<"ignore", "pipe", "pipe">,
  exitCode: number,
): Promise<void> {
  for (const stop of appTails.splice(0)) await stop();
  store.flushPartial(appTab);
  if (stopping || app !== launchedApp) {
    // Replaced by a newer instance or the watcher is shutting down.
    if (appTab.status !== "stopped") store.markStopped(appTab, null);
    return;
  }
  app = undefined;
  stopping = true;
  closeWatchers();
  clearRebuildTimer();
  store.pushLine(appTab, `── app exited (code ${exitCode}) ──`);
  store.markStopped(appTab, exitCode);
  finalExitCode = exitCode;
  store.setStatusLine(
    `WakuWaku exited (code ${exitCode}); press q to quit.`,
    exitCode === 0 ? "info" : "error",
  );
}

async function stopApp(): Promise<void> {
  const waiter = app;
  app = undefined;
  if (isMacOS) {
    const killer = Bun.spawn(["pkill", "-TERM", "-x", appName], {
      stdout: "ignore",
      stderr: "ignore",
    });
    await killer.exited;
  } else if (waiter?.exitCode === null) {
    waiter.kill("SIGTERM");
  }
  if (waiter?.exitCode === null) {
    await waiter.exited;
  }
}

function clearRebuildTimer(): void {
  if (rebuildTimer === undefined) return;
  clearTimeout(rebuildTimer);
  rebuildTimer = undefined;
}

function closeWatchers(): void {
  for (const watcher of watchers.splice(0)) watcher.close();
}

function reportWatcherError(error: Error): void {
  if (stopping) return;
  exitMessage = `[wakuwaku-dev] File watcher failed: ${error.message}`;
  void requestQuit(1);
}

function mergedTarget(
  current: BuildTarget | undefined,
  next: BuildTarget,
): BuildTarget {
  return current === "app" || next === "app" ? "app" : "daemon";
}

function targetForChange(directory: string, filename: string | Buffer | null): BuildTarget {
  if (directory !== "crates" || filename === null) return "app";
  const relativePath = filename.toString().replaceAll("\\", "/");
  if (
    relativePath.startsWith("wakuwaku-daemon/") ||
    relativePath.startsWith("wakuwaku-core/")
  ) {
    return "daemon";
  }
  return "app";
}

function scheduleBuild(target: BuildTarget): void {
  if (stopping) return;
  daemonChangeRevision += 1;
  if (target === "app") appChangeRevision += 1;
  debouncedBuild = mergedTarget(debouncedBuild, target);
  clearRebuildTimer();
  rebuildTimer = setTimeout(() => {
    rebuildTimer = undefined;
    if (debouncedBuild !== undefined) {
      queuedBuild = mergedTarget(queuedBuild, debouncedBuild);
      debouncedBuild = undefined;
    }
    void drainBuildQueue();
  }, rebuildDebounceMs);
}

function startWatchers(): void {
  for (const directory of watchedDirectories) {
    const watcher = watch(
      join(root, directory),
      { recursive: true },
      (_eventType, filename) => scheduleBuild(targetForChange(directory, filename)),
    ) as Watcher;
    watcher.on("error", reportWatcherError);
    watchers.push(watcher);
  }

  const rootWatcher = watch(root, (_eventType, filename) => {
    if (filename && watchedFiles.includes(filename.toString())) scheduleBuild("app");
  }) as Watcher;
  rootWatcher.on("error", reportWatcherError);
  watchers.push(rootWatcher);
}

async function drainBuildQueue(): Promise<void> {
  if (building || stopping) return;
  building = true;
  try {
    while (queuedBuild !== undefined && !stopping) {
      const target = queuedBuild;
      queuedBuild = undefined;
      const buildAppRevision = appChangeRevision;
      const buildDaemonRevision = daemonChangeRevision;
      if (target === "daemon") {
        if (!(await buildDaemon()) || stopping) continue;
        if (daemonChangeRevision === buildDaemonRevision) {
          store.setStatusLine(
            "Daemon rebuilt; WakuWaku swaps it without relaunching.",
            "success",
          );
        }
        continue;
      }

      // App changes make a bundle compiled from an older revision stale. A
      // daemon-only edit does not: launch the app, then let its supervisor pick
      // up the independently rebuilt daemon.
      if (!(await runAppBuild()) || stopping) continue;
      if (appChangeRevision !== buildAppRevision) {
        store.setStatusLine("More changes arrived during the build; rebuilding…");
        continue;
      }

      await stopApp();
      if (!stopping) {
        launchApp();
        store.setStatusLine(
          "Watching for changes; daemon-only edits hot-reload without relaunching.",
          "success",
        );
      }
    }
  } finally {
    building = false;
    if (queuedBuild !== undefined && !stopping) void drainBuildQueue();
  }
}

async function requestQuit(code?: number): Promise<void> {
  if (quitStarted) return;
  quitStarted = true;
  stopping = true;
  store.setStatusLine("Stopping…");
  closeWatchers();
  clearRebuildTimer();
  await stopApp();
  instance?.unmount();
  if (exitMessage !== undefined) process.stderr.write(`${exitMessage}\n`);
  process.exit(code ?? finalExitCode ?? (everLaunched ? 0 : 1));
}

const controller: DevController = {
  store,
  requestQuit: () => void requestQuit(),
  rebuildNow: () => {
    if (!stopping) scheduleBuild("app");
  },
};

process.on("SIGINT", () => void requestQuit());
process.on("SIGTERM", () => void requestQuit());

instance = render(createElement(DevUi, { controller }), {
  exitOnCtrlC: false,
  alternateScreen: true,
});

startWatchers();
building = true;
const initialAppRevision = appChangeRevision;
const initialBuildSucceeded = await runAppBuild();
building = false;

if (!initialBuildSucceeded) {
  store.setStatusLine("Initial build failed; the watcher retries after the next change.", "error");
} else if (appChangeRevision === initialAppRevision) {
  await stopApp();
  launchApp();
  store.setStatusLine(
    "Watching for changes; daemon-only edits hot-reload without relaunching.",
    "success",
  );
} else {
  store.setStatusLine("Changes arrived during the initial build; rebuilding…");
  if (queuedBuild !== undefined) void drainBuildQueue();
}
