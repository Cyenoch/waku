export type TabStatus = "building" | "running" | "warn" | "error" | "stopped";
export type TabKind = "app" | "daemon";
export type StatusTone = "info" | "success" | "warn" | "error";

export interface Tab {
  id: number;
  kind: TabKind;
  title: string;
  status: TabStatus;
  /** Exit code of the app process, once known. */
  exitCode: number | null;
  lines: string[];
  /** Trailing partial line from the active writers, rendered as the last line. */
  partial: string;
  /** Bumped on every content change so views can memoize wrapping. */
  contentRevision: number;
}

const MAX_LINES_PER_TAB = 5_000;
const CONTENT_FLUSH_MS = 33;

/**
 * Mutable state shared between the watcher controller and the Ink UI.
 * Mutations are synchronous; listeners re-render the whole tree.
 */
export class DevStore {
  private readonly listeners = new Set<() => void>();
  private nextTabId = 1;
  private flushTimer: ReturnType<typeof setTimeout> | undefined;
  version = 0;
  readonly tabs: Tab[] = [];
  activeTabId = 0;
  statusText = "Starting…";
  statusTone: StatusTone = "info";

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  private emit(): void {
    this.version += 1;
    for (const listener of [...this.listeners]) listener();
  }

  /** Coalesce content-only updates so output bursts render at ~30 fps. */
  private emitContent(): void {
    if (this.flushTimer !== undefined) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = undefined;
      this.emit();
    }, CONTENT_FLUSH_MS);
  }

  get activeTab(): Tab | undefined {
    return this.tabs.find((tab) => tab.id === this.activeTabId);
  }

  /** The watcher owns the lifecycle: exactly one App and one Daemon tab. */
  openTab(kind: TabKind, title: string): Tab {
    const tab: Tab = {
      id: this.nextTabId++,
      kind,
      title,
      status: "building",
      exitCode: null,
      lines: [],
      partial: "",
      contentRevision: 0,
    };
    this.tabs.push(tab);
    this.emit();
    return tab;
  }

  activate(tab: Tab): void {
    if (this.activeTabId === tab.id) return;
    this.activeTabId = tab.id;
    this.emit();
  }

  cycleTab(delta: number): void {
    if (this.tabs.length === 0) return;
    const current = this.tabs.findIndex((tab) => tab.id === this.activeTabId);
    const base = current === -1 ? 0 : current + delta;
    const next = ((base % this.tabs.length) + this.tabs.length) % this.tabs.length;
    this.activate(this.tabs[next]!);
  }

  setStatus(tab: Tab, status: TabStatus): void {
    if (tab.status === status) return;
    tab.status = status;
    this.emit();
  }

  activateIndex(index: number): void {
    const tab = this.tabs[index];
    if (tab) this.activate(tab);
  }

  markRunning(tab: Tab): void {
    tab.exitCode = null;
    tab.status = "running";
    this.emit();
  }

  markStopped(tab: Tab, exitCode: number | null): void {
    tab.status = "stopped";
    tab.exitCode = exitCode;
    this.emit();
  }

  pushLine(tab: Tab, line: string): void {
    tab.lines.push(line);
    if (tab.lines.length > MAX_LINES_PER_TAB) {
      tab.lines.splice(0, tab.lines.length - MAX_LINES_PER_TAB);
    }
    tab.contentRevision += 1;
    this.emitContent();
  }

  /** Split a chunk on line boundaries and merge it with the partial line. */
  ingest(tab: Tab, chunk: string): void {
    if (chunk === "") return;
    const pieces = chunk.split(/\r\n|\r|\n/);
    const first = pieces.shift() ?? "";
    if (tab.partial !== "") {
      tab.lines.push(tab.partial + first);
    } else if (first !== "") {
      tab.lines.push(first);
    }
    tab.partial = pieces.pop() ?? "";
    for (const piece of pieces) tab.lines.push(piece);
    if (tab.lines.length > MAX_LINES_PER_TAB) {
      tab.lines.splice(0, tab.lines.length - MAX_LINES_PER_TAB);
    }
    tab.contentRevision += 1;
    this.emitContent();
  }

  flushPartial(tab: Tab): void {
    if (tab.partial === "") return;
    const line = tab.partial;
    tab.partial = "";
    this.pushLine(tab, line);
  }

  clear(tab: Tab): void {
    tab.lines = [];
    tab.partial = "";
    tab.contentRevision += 1;
    this.emit();
  }

  setStatusLine(text: string, tone: StatusTone = "info"): void {
    this.statusText = text;
    this.statusTone = tone;
    this.emit();
  }
}
