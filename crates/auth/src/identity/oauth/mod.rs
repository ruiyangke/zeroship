//! OAuth/OIDC federation clients (Google, GitHub).
//!
//! Each provider module owns the upstream-specific dance: building the
//! authorize URL, exchanging the code, and normalising the upstream profile
//! into a shape the [`crate::identity::linker`] can consume. Cookie / CSRF /
//! stash handling lives in the HTTP layer (`crate::ui::oauth_google` etc.),
//! not here.

pub mod github;
pub mod google;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamPrompt {
    login: bool,
    select_account: bool,
}

impl UpstreamPrompt {
    pub fn from_values(login: bool, select_account: bool) -> Option<Self> {
        (login || select_account).then_some(Self {
            login,
            select_account,
        })
    }

    pub fn from_oidc_prompt(prompt: Option<&str>, max_age: Option<&str>) -> Option<Self> {
        let mut login = max_age.map(str::trim) == Some("0");
        let mut select_account = false;
        for value in prompt
            .unwrap_or("")
            .split_ascii_whitespace()
            .filter(|value| !value.is_empty())
        {
            match value {
                "login" => login = true,
                "select_account" => select_account = true,
                _ => {}
            }
        }
        Self::from_values(login, select_account)
    }

    pub fn combine(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(a), Some(b)) => {
                Self::from_values(a.login || b.login, a.select_account || b.select_account)
            }
            (Some(prompt), None) | (None, Some(prompt)) => Some(prompt),
            (None, None) => None,
        }
    }

    pub fn prompt_value(self) -> &'static str {
        match (self.login, self.select_account) {
            (true, true) => "login select_account",
            (true, false) => "login",
            (false, true) => "select_account",
            (false, false) => "",
        }
    }

    pub fn requests_login(self) -> bool {
        self.login
    }
}
