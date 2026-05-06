/**
 * The bottom pane. Renders one Output at a time — events, an action
 * run, or a live tmux session — picked by id. Owns the surrounding
 * box / border / title; defers to the per-kind content components for
 * the body.
 */
import { actionRegistry } from "../../core/actions.ts";
import { type Output } from "../../core/outputs.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";

import { ActionContent, SessionContent } from "./action-viewer.tsx";
import { ActivityContent } from "./activity.tsx";

type Props = {
  output: Output;
  height: number;
  pinned: boolean;
};

function borderColor(o: Output): string {
  if (o.kind === "events") return theme.border;
  if (o.kind === "session") return theme.info;
  switch (o.status) {
    case "running":
      return theme.accent;
    case "done":
      return theme.ok;
    case "killed":
      return theme.warn;
    case "failed":
      return theme.err;
    default:
      return theme.border;
  }
}

function statusLabel(o: Output): string {
  switch (o.status) {
    case "running":
      return "running";
    case "done":
      return "done";
    case "killed":
      return "killed";
    case "failed":
      return "failed";
    case "live":
      return "live";
  }
}

function titleFor(o: Output): string {
  if (o.kind === "events") return "events";
  if (o.kind === "session") return o.title;
  const killHint = o.status === "running" ? " · ! kill" : "";
  return `action · ${o.title} · ${statusLabel(o)}${killHint}`;
}

export function OutputViewer({ output, height, pinned }: Props) {
  const pinPrefix = pinned ? `${NF.pin} ` : "";
  const title = ` ${pinPrefix}${titleFor(output)} `;
  return (
    <box
      flexShrink={0}
      height={height}
      border
      borderStyle="single"
      borderColor={borderColor(output)}
      title={title}
      titleAlignment="left"
      flexDirection="column"
      paddingLeft={1}
      paddingRight={1}
    >
      <OutputContent output={output} height={height} />
    </box>
  );
}

function OutputContent({ output, height }: { output: Output; height: number }) {
  if (output.kind === "events") {
    return <ActivityContent height={height} />;
  }
  if (output.kind === "session" && output.slug) {
    return <SessionContent slug={output.slug} height={height} />;
  }
  if (output.kind === "action" && output.slug) {
    const run = actionRegistry.get(output.slug);
    if (run && run.startedAt === output.startedAt) {
      return <ActionContent run={run} height={height} />;
    }
    // The action's runtime entry was evicted (retention rotated it
    // out), but the picker still has the metadata. Show a placeholder
    // rather than a blank pane so the user knows why nothing's there.
    return (
      <text fg={theme.fgDim}>
        (run no longer in memory — check the log file)
      </text>
    );
  }
  return null;
}
