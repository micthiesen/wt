//! Native, recoverable execution for configured wt actions.

pub use config::{
    ActionContext, ActionDestination, ActionHarness, ActionPlan, ActionPlanError, ActionPr,
    ActionRow, ActionVars, PreparedAction, apply_vars, evaluate_requirements, prepare_action,
    prepare_action_with_vars,
};
pub use service::{
    ActionDone, ActionRequest, ActionService, ActionServiceConfig, ActionServiceError, ActionStart,
};
pub use types::{
    ActionArgHistory, ActionMeta, ActionOutputLine, ActionRun, ActionRunKind, ActionRunStatus,
    ActionRunView, ActionStream, IssueStatusExpectation,
};

mod config;
mod history;
mod service;
mod types;

pub use history::{
    ActionHistoryEntry, recent_values, record_value, record_value_for_run, refine_value,
};
