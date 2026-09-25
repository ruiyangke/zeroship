//! App-scoped workflow operations shared by native callers and transport adapters.

pub use zeroship_core::workflow_coordination::{
    ConflictPolicy, DeliveredSignal, RestartDeploy, RestartOptions, RestartTarget, RestartedRun,
    RunOperation, RunState, RunStatus, SignalOptions, StartOptions, StartedRun, TransitionedRun,
};
