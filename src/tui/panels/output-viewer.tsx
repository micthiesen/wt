/**
 * The bottom pane. Renders one Output at a time — events, an action
 * run, or a live claude tmux session — picked by id. Owns the
 * surrounding box / border / title; defers to the per-kind content
 * components for the body.
 */
import { actionRegistry } from "../../core/actions.ts";
import { type Output, outputStatusLabel } from "../../core/outputs.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";

import {
  ActionContent,
  SessionContent,
  ShellContent,
} from "./action-viewer.tsx";
import { ActivityContent, DestroyContent } from "./activity.tsx";

type Props = {
  output: Output;
  height: number;
  pinned: boolean;
};

function borderColor(o: Output): string {
  if (o.kind === "events") return theme.border;
  if (o.kind === "session") return theme.info;
  // Destroy borrows the warn palette — it's a destructive op in
  // flight, distinct from a benign action's running cyan.
  if (o.kind === "destroy") return theme.warn;
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

function titleFor(o: Output): string {
  if (o.kind === "events") return "events";
  if (o.kind === "session") return o.title;
  if (o.kind === "destroy") {
    return `destroy · ${o.slug ?? "?"} · running`;
  }
  const killHint = o.status === "running" ? " · ! kill" : "";
  return `action · ${o.title} · ${outputStatusLabel(o.status)}${killHint}`;
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
  if (output.kind === "destroy" && output.slug) {
    return <DestroyContent slug={output.slug} height={height} />;
  }
  if (output.kind === "session" && output.slug) {
    // Both claude and shell tail their tmux pane: claude reads
    // stream-json from the wt-managed jsonl, shell reads the
    // pipe-pane log with ANSI stripped. Different parsers, same
    // shape downstream.
    if (output.sessionKind === "claude") {
      return <SessionContent slug={output.slug} height={height} />;
    }
    if (output.sessionKind === "shell") {
      return <ShellContent slug={output.slug} height={height} />;
    }
  }
  if (output.kind === "action" && output.slug) {
    // Both `outputs` (the picker source) and this lookup read from
    // the same `actionRegistry` map. The id is keyed on
    // `${slug}:${startedAt}`, so an entry that's in the picker is
    // always findable here. The `null` return is a defensive fallback
    // for the intra-render-mutation race window only.
    const run = actionRegistry.get(output.slug);
    if (run && run.startedAt === output.startedAt) {
      return <ActionContent run={run} height={height} />;
    }
  }
  return null;
}

