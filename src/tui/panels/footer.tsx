import { theme } from "../theme.ts";

export type FooterMode =
  | { kind: "legend" }
  | { kind: "confirm"; message: string; pendingKey: string }
  | { kind: "toast"; message: string; color?: string }
  | {
      kind: "input";
      prompt: string;
      value: string;
      purpose: "new";
      /**
       * Optional default `--base` ref for the new-worktree input (set
       * by the `N` keybinding). Not rendered in the prompt; the event
       * log carries the notice. An explicit `--base` in the input
       * text overrides this.
       */
      base?: string;
    }
  | { kind: "filter"; value: string };

type Props = {
  mode: FooterMode;
  hint?: string;
  height?: number;
};

const LEGEND = [
  ["jk", "move"],
  ["o", "zed"],
  ["/", "filter"],
  ["n", "new"],
  ["d", "rm"],
  ["r", "refresh"],
  ["?", "help"],
  ["q", "quit"],
];

export function Footer({ mode, hint }: Props) {
  return (
    <box
      flexShrink={0}
      backgroundColor={theme.bgAlt}
      paddingLeft={1}
      paddingRight={1}
      height={1}
      flexDirection="row"
    >
      <box flexDirection="row" flexGrow={1} flexShrink={1} overflow="hidden">
        {mode.kind === "legend" ? <Legend /> : null}
        {mode.kind === "toast" ? (
          <text fg={mode.color ?? theme.ok}>{mode.message}</text>
        ) : null}
        {mode.kind === "confirm" ? (
          <text fg={theme.warn}>{mode.message}</text>
        ) : null}
        {mode.kind === "input" ? (
          <>
            <text>
              <span fg={theme.accent} attributes={1}>
                {mode.prompt}
              </span>
              <span> </span>
              <span fg={theme.fgBright}>{mode.value}</span>
              <span fg={theme.accent}>█</span>
            </text>
            <text fg={theme.fgDim}> (⏎ submit, esc cancel)</text>
          </>
        ) : null}
        {mode.kind === "filter" ? (
          <>
            <text>
              <span fg={theme.accent} attributes={1}>
                /
              </span>
              <span fg={theme.fgBright}>{mode.value}</span>
              <span fg={theme.accent}>█</span>
            </text>
            <text fg={theme.fgDim}> (⏎ apply, esc clear)</text>
          </>
        ) : null}
      </box>
      {hint ? (
        <box flexShrink={0} flexDirection="row">
          <text fg={theme.fgDim}>{hint}</text>
        </box>
      ) : null}
    </box>
  );
}

function Legend() {
  const parts: React.ReactNode[] = [];
  LEGEND.forEach(([key, label], i) => {
    if (i > 0) {
      parts.push(
        <text key={`sep-${i}`} fg={theme.fgDim}>
          {"  ·  "}
        </text>,
      );
    }
    parts.push(
      <text key={`k-${i}`} fg={theme.accent} attributes={1}>
        {key}
      </text>,
    );
    parts.push(
      <text key={`l-${i}`} fg={theme.fgDim}>
        {" "}
        {label}
      </text>,
    );
  });
  return <>{parts}</>;
}
