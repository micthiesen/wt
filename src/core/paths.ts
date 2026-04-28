/**
 * Compatibility surface for the old hard-coded constants. Values now
 * come from `config.ts` — this file only exists so existing call sites
 * don't have to change. New code should prefer `import { config } from
 * "./config.ts"` and read fields directly.
 */
import { config } from "./config.ts";

export const MAIN_CLONE = config.paths.mainClone;
export const WT_ROOT = config.paths.worktreeRoot;
export const LOG_DIR = config.paths.logDir;
export const LOCK_DIR = config.paths.lockDir;

export const BRANCH_PREFIX = config.branch.prefix;
export const BASE_BRANCH = config.branch.base;
export const SLUG_MAX_LEN = config.branch.slugMaxLen;

export const STAGE_PREFIX = config.stage.prefix;
export const DEFAULT_PERSONAL_STAGE = config.stage.defaultPersonal;
/** May be null when no public stage domain is configured. */
export const STAGE_DOMAIN: string | null = config.stage.domain;

export const ENV_FILES_TO_COPY = config.lifecycle.envFilesToCopy;
export const PARALLEL_WORKERS = config.lifecycle.parallelWorkers;

// SST-specific. These will throw at the call site if the user has
// disabled SST in config (sst = null) — wrap with `requireSst()` at the
// boundary if you need a clearer error.
export const SST_STATE_BUCKET = config.sst?.stateBucket ?? "";
export const SST_STATE_PREFIX = config.sst?.statePrefix ?? "";
export const AWS_PROFILE = config.sst?.awsProfile ?? "";
export const AUTO_REGEN_PATHS = config.sst?.autoRegenPaths ?? [];

export const LINEAR_WORKSPACE = config.linear?.workspace ?? "";
