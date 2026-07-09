import { queryOptions } from "@tanstack/react-query";

import { readArchived } from "../../core/archive.ts";
import { readWtState, type WtState } from "../../core/wtstate.ts";

import { qk } from "../keys.ts";
import { STALE } from "./shared.ts";

export const archiveQuery = () =>
  queryOptions({
    queryKey: qk.archive(),
    queryFn: async (): Promise<string[]> => [...readArchived()],
    staleTime: STALE.fast,
  });

export const wtStateQuery = () =>
  queryOptions({
    queryKey: qk.wtState(),
    queryFn: async (): Promise<WtState> => readWtState(),
    staleTime: STALE.fast,
  });
