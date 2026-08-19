import { useEffect, useMemo, useRef, useState } from "react";
import { Box, Text, useInput, useWindowSize } from "ink";
import { wrapLine, wrapLines } from "./text";
import type { DevStore, StatusTone, Tab, TabStatus } from "./store";

export interface DevController {
  store: DevStore;
  requestQuit(): void;
  rebuildNow(): void;
}

interface ScrollState {
  follow: boolean;
  fromBottom: number;
}

const STATUS_TONE_COLOR: Record<StatusTone, string | undefined> = {
  info: "cyan",
  success: "green",
  warn: "yellow",
  error: "red",
};

// Fixed truecolor so the active tab stays readable regardless of how the
// terminal theme remaps the named ANSI palette.
const ACTIVE_TAB_BACKGROUND = "#2f6fed";

function tabIconColor(status: TabStatus, exitCode: number | null): string | undefined {
  switch (status) {
    case "building":
      return "yellow";
    case "running":
      return "green";
    case "warn":
      return "yellow";
    case "error":
      return "red";
    case "stopped":
      return exitCode ? "red" : undefined;
  }
}

function tabIconText(status: TabStatus, exitCode: number | null): string {
  switch (status) {
    case "building":
      return "⟳";
    case "running":
      return "✓";
    case "warn":
      return "⚠";
    case "error":
    case "stopped":
      return exitCode ? "✗" : "✓";
  }
}

function StatusIcon({ tab, dim }: { tab: Tab; dim: boolean }) {
  return (
    <Text color={tabIconColor(tab.status, tab.exitCode)} dimColor={dim && !tab.exitCode}>
      {tabIconText(tab.status, tab.exitCode)}
    </Text>
  );
}

function cellWidth(tab: Tab, hotkeyDigits: number): number {
  return hotkeyDigits + tab.title.length + 5;
}

function TabBar({ store, width }: { store: DevStore; width: number }) {
  const cells = store.tabs.map((tab, index) => ({
    tab,
    hotkey: index + 1,
    width: cellWidth(tab, String(index + 1).length),
  }));
  const fitting: typeof cells = [];
  let used = 0;
  for (let index = cells.length - 1; index >= 0; index -= 1) {
    const cell = cells[index]!;
    if (used + cell.width > width) break;
    used += cell.width;
    fitting.unshift(cell);
  }
  const dropped = cells.length - fitting.length;

  return (
    <Box width={width}>
      {dropped > 0 && <Text dimColor>… </Text>}
      {fitting.map(({ tab, hotkey }) => {
        const active = tab.id === store.activeTabId;
        return (
          <Text key={tab.id} backgroundColor={active ? ACTIVE_TAB_BACKGROUND : undefined}>
            <Text color={active ? "whiteBright" : "gray"} bold={active} dimColor={!active}>
              {" "}
              {hotkey}{" "}
            </Text>
            <StatusIcon tab={tab} dim={!active} />
            <Text bold={active} color={active ? "whiteBright" : "gray"}>
              {" "}
              {tab.title}{" "}
            </Text>
          </Text>
        );
      })}
    </Box>
  );
}

function LogPane({
  tab,
  scroll,
  height,
  width,
}: {
  tab: Tab | undefined;
  scroll: ScrollState;
  height: number;
  width: number;
}) {
  const contentRevision = tab?.contentRevision ?? -1;
  const partial = tab?.partial ?? "";
  const body = useMemo(() => {
    if (!tab) return [];
    const wrapped = wrapLines(tab.lines, width);
    if (partial !== "") wrapped.push(...wrapLine(partial, width));
    return wrapped;
  }, [tab, contentRevision, partial, width]);

  if (!tab) {
    return (
      <Box flexGrow={1} paddingX={1}>
        <Text dimColor>No output yet.</Text>
      </Box>
    );
  }

  const total = body.length;
  const maxFromBottom = Math.max(0, total - height);
  const fromBottom = scroll.follow ? 0 : Math.min(scroll.fromBottom, maxFromBottom);
  const end = total - fromBottom;
  const start = Math.max(0, end - height);
  const visible = body.slice(start, end);
  const more = fromBottom > 0 ? `${fromBottom} more ↓` : undefined;

  return (
    <Box flexGrow={1} flexDirection="column" paddingX={1}>
      {total === 0 ? (
        <Text dimColor>Waiting for output…</Text>
      ) : (
        <Text wrap="hard">{visible.join("\n")}</Text>
      )}
      <Text dimColor>{more ?? ""}</Text>
    </Box>
  );
}

function HelpPane() {
  const rows: [string, string][] = [
    ["←/→ h/l  tab/shift+tab", "switch tabs"],
    ["1-9", "jump to tab"],
    ["↑/↓ j/k  pgup/pgdn", "scroll"],
    ["g/home  G/end/enter", "jump to top / follow output"],
    ["c", "clear active tab"],
    ["r", "rebuild now"],
    ["?", "toggle this help"],
    ["q  ctrl+c", "quit (stops the app)"],
  ];
  return (
    <Box flexGrow={1} flexDirection="row" justifyContent="center">
      <Box flexDirection="column" borderStyle="round" paddingX={2}>
        <Text bold>Keys</Text>
        {rows.map(([keys, description]) => (
          <Text key={keys}>
            <Text dimColor> {keys.padEnd(28)}</Text>
            {description}
          </Text>
        ))}
        <Text dimColor> esc/? closes this pane</Text>
      </Box>
    </Box>
  );
}

function StatusBar({ store, width }: { store: DevStore; width: number }) {
  const hints = "? help · r rebuild · q quit";
  const budget = Math.max(0, width - hints.length - 3);
  const text = store.statusText.length > budget
    ? `${store.statusText.slice(0, Math.max(0, budget - 1))}…`
    : store.statusText;
  return (
    <Box width={width} justifyContent="space-between">
      <Text color={STATUS_TONE_COLOR[store.statusTone]}> {text}</Text>
      <Text dimColor>{hints}</Text>
    </Box>
  );
}

export function DevUi({ controller }: { controller: DevController }) {
  const store = controller.store;
  const [, setTick] = useState(0);
  useEffect(() => store.subscribe(() => setTick((tick) => tick + 1)), [store]);
  const { columns, rows } = useWindowSize();
  const [helpOpen, setHelpOpen] = useState(false);
  const scrollStates = useRef(new Map<number, ScrollState>());
  const width = Math.max(20, columns);
  const logHeight = Math.max(1, rows - 3);

  useInput((input, key) => {
    const active = store.activeTab;
    let scroll = scrollStates.current.get(active?.id ?? 0);
    if (active && scroll === undefined) {
      scroll = { follow: true, fromBottom: 0 };
      scrollStates.current.set(active.id, scroll);
    }

    if (input === "\x03") {
      controller.requestQuit();
      return;
    }
    if (helpOpen) {
      if (key.escape || input === "q" || input === "?") setHelpOpen(false);
      return;
    }
    if (input === "q" && !key.ctrl) {
      controller.requestQuit();
      return;
    }
    if (input === "?") {
      setHelpOpen(true);
      return;
    }

    const scrollBy = (amount: number) => {
      if (!scroll) return;
      const base = scroll.follow ? 0 : scroll.fromBottom;
      if (base + amount <= 0) {
        scroll.follow = true;
        scroll.fromBottom = 0;
      } else {
        scroll.follow = false;
        scroll.fromBottom = base + amount;
      }
    };

    if (key.leftArrow || input === "h") store.cycleTab(-1);
    else if (key.rightArrow || input === "l") store.cycleTab(1);
    else if (key.tab) store.cycleTab(key.shift ? -1 : 1);
    else if (input >= "1" && input <= "9") store.activateIndex(Number(input) - 1);
    else if (key.upArrow || input === "k") scrollBy(1);
    else if (key.pageUp) scrollBy(Math.floor(logHeight / 2));
    else if (key.home || input === "g") {
      if (scroll) {
        scroll.follow = false;
        scroll.fromBottom = Number.MAX_SAFE_INTEGER;
      }
    } else if (key.return || input === "G") {
      if (scroll) {
        scroll.follow = true;
        scroll.fromBottom = 0;
      }
    } else if (key.downArrow || input === "j") scrollBy(-1);
    else if (key.pageDown) scrollBy(-Math.floor(logHeight / 2));
    else if (input === "c") {
      if (active) store.clear(active);
    } else if (input === "r") {
      controller.rebuildNow();
    } else {
      return;
    }
    setTick((tick) => tick + 1);
  });

  const active = store.activeTab;
  const scroll = scrollStates.current.get(active?.id ?? 0) ?? { follow: true, fromBottom: 0 };

  return (
    <Box flexDirection="column" width={width} height={rows}>
      <TabBar store={store} width={width} />
      {helpOpen ? (
        <HelpPane />
      ) : (
        <LogPane tab={active} scroll={scroll} height={logHeight} width={width - 2} />
      )}
      <StatusBar store={store} width={width} />
    </Box>
  );
}
