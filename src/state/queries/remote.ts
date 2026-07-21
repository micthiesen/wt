import { keepPreviousData, queryOptions } from "@tanstack/react-query";

import { config } from "../../core/config.ts";
import { fetchRemoteWorktrees } from "../../core/remote-worktrees.ts";
import { qk } from "../keys.ts";

export const remoteWorktreesQuery = () =>
  queryOptions({
    queryKey: qk.remoteWorktrees(),
    queryFn: ({ signal }) =>
      config.remote ? fetchRemoteWorktrees(config.remote, signal) : Promise.resolve([]),
    staleTime: 3_000,
    // Reachability is not membership. Keep the last successful (persisted)
    // inventory visible while a sleeping/offline host rejects refetches.
    placeholderData: keepPreviousData,
    refetchInterval: (query) =>
      query.state.data?.some((row) => row.status === "busy") ? 2_000 : 15_000,
    retry: 1,
  });
