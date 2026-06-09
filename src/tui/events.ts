import { useSyncExternalStore } from "react";

import type { EventKind } from "../core/logger.ts";

export type WtEvent = {
  id: number;
  ts: number;
  level: EventKind;
  source: string; // "app" | slug | arbitrary
  text: string;
};

type Listener = () => void;

const MAX_EVENTS = 500;

class EventLog {
  private events: readonly WtEvent[] = [];
  private listeners = new Set<Listener>();
  private nextId = 1;

  append(partial: Omit<WtEvent, "id" | "ts">): WtEvent {
    const full: WtEvent = { id: this.nextId++, ts: Date.now(), ...partial };
    const next = [...this.events, full];
    this.events = next.length > MAX_EVENTS ? next.slice(-MAX_EVENTS) : next;
    this.notify();
    return full;
  }

  // Arrow-bound so React's useSyncExternalStore gets stable refs.
  getSnapshot = (): readonly WtEvent[] => this.events;

  subscribe = (fn: Listener): (() => void) => {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  };

  private notify(): void {
    for (const l of this.listeners) l();
  }
}

export const events = new EventLog();

export function useEvents(): readonly WtEvent[] {
  return useSyncExternalStore(events.subscribe, events.getSnapshot, events.getSnapshot);
}
