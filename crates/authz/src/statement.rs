use serde::{Deserialize, Serialize};

use crate::{Action, Condition, Effect, Resource};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Statement {
    pub effect: Effect,
    pub actions: Vec<Action>,
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}
