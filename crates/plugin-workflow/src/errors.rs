use compio_postgres::error::SqlState;

#[derive(Debug)]
pub enum WorkflowError {
    Invalid(String),
    Deadlock(String),
    Db(String),
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "invalid workflow StepResult: {msg}"),
            Self::Deadlock(msg) => write!(f, "deadlock: {msg}"),
            Self::Db(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for WorkflowError {}

impl From<compio_postgres::Error> for WorkflowError {
    fn from(e: compio_postgres::Error) -> Self {
        if e.code() == Some(&SqlState::T_R_DEADLOCK_DETECTED) {
            Self::Deadlock(e.to_string())
        } else {
            let msg = e.to_string();
            let full = match source_chain(&e) {
                Some(chain) => format!("{msg}: {chain}"),
                None => msg,
            };
            Self::Db(full)
        }
    }
}

fn source_chain(err: &dyn std::error::Error) -> Option<String> {
    let mut out = String::new();
    let mut cur = err.source();
    while let Some(e) = cur {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&e.to_string());
        cur = e.source();
    }
    (!out.is_empty()).then_some(out)
}
