use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use ntex::web::{self, HttpRequest, HttpResponse};
use uuid::Uuid;

use zeroship_core::app_id::AppId;

use crate::handler::check_worker_auth;
use crate::WorkerConfig;

const MAX_LINES_PER_APP: usize = 1000;

pub type SharedLogs = Arc<RwLock<HashMap<Uuid, VecDeque<String>>>>;

pub fn new_store() -> SharedLogs {
    Arc::new(RwLock::new(HashMap::new()))
}

pub fn append(store: &SharedLogs, app_id: Uuid, lines: Vec<String>) {
    if lines.is_empty() {
        return;
    }

    let Ok(mut guard) = store.write() else {
        tracing::error!(app_id = %app_id, "worker-logs: log store lock poisoned");
        return;
    };
    let ring = guard.entry(app_id).or_default();
    for line in lines {
        ring.push_back(line);
    }
    while ring.len() > MAX_LINES_PER_APP {
        ring.pop_front();
    }
}

pub fn get(store: &SharedLogs, app_id: &Uuid) -> Vec<String> {
    store
        .read()
        .ok()
        .and_then(|guard| {
            guard
                .get(app_id)
                .map(|lines| lines.iter().cloned().collect())
        })
        .unwrap_or_default()
}

pub async fn get_logs(
    req: HttpRequest,
    config: web::types::State<Arc<WorkerConfig>>,
    logs: web::types::State<SharedLogs>,
    path: web::types::Path<String>,
) -> HttpResponse {
    // The log read is called by the CONTROL plane, not the gateway, and the
    // allowlist says so: `svc/control` holds the grant on this endpoint and
    // `svc/gateway` does not.
    if let Some(resp) = check_worker_auth(
        &req,
        &config.service_auth,
        zeroship_core::service_identity::endpoints::WORKER_APP_LOGS,
    )
    .await
    {
        return resp;
    }

    // The path segment is a typed app id, the same as `/dispatch/{app_id}`,
    // and for the same reason: the caller and this process have to agree about
    // the RENDERING of the identity, and only one of the two spellings parses.
    // The store below is keyed by the uuid the control plane serves, so the id
    // is unwrapped here rather than carried - the transitional conversion
    // `handler::dispatch` documents in full.
    let app_id = match AppId::parse(path.as_str()) {
        Ok(id) => id.uuid(),
        Err(_) => {
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "invalid app_id"}));
        }
    };

    HttpResponse::Ok().json(&get(&logs, &app_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_last_1000_lines_per_app() {
        let store = new_store();
        let app_id = Uuid::new_v4();
        append(
            &store,
            app_id,
            (0..1005).map(|i| format!("line {i}")).collect(),
        );

        let lines = get(&store, &app_id);
        assert_eq!(lines.len(), 1000);
        assert_eq!(lines.first().map(String::as_str), Some("line 5"));
        assert_eq!(lines.last().map(String::as_str), Some("line 1004"));
    }

    #[test]
    fn empty_app_returns_empty_lines() {
        let store = new_store();
        assert!(get(&store, &Uuid::new_v4()).is_empty());
    }

    /// The log path segment is the SAME identity as the dispatch path segment,
    /// and it has to be refused on the same terms.
    ///
    /// The two endpoints have different callers - the gateway dispatches, the
    /// control plane reads logs - so a slice that tightened only one of them
    /// would leave the two halves of the worker's public surface disagreeing
    /// about what an app id is. This binds them to one answer without needing
    /// a server: the pair below is exactly what the handler branches on.
    #[test]
    fn the_log_path_accepts_one_rendering_of_an_app_id_and_refuses_the_other() {
        let raw = Uuid::new_v4();

        assert!(
            AppId::parse(&raw.to_string()).is_err(),
            "a uuid rendering must not be readable as an app id"
        );

        let canonical = zeroship_core::app_id::canonical_app_id_for(&raw);
        let parsed = AppId::parse(canonical.as_str()).expect("the canonical rendering parses");
        assert_eq!(
            parsed.uuid(),
            raw,
            "and it must unwrap to the uuid the store is keyed by"
        );
    }
}
