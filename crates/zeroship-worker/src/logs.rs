use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use ntex::web::{self, HttpRequest, HttpResponse};
use uuid::Uuid;

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
    if let Some(resp) = check_worker_auth(&req, &config.worker_key) {
        return resp;
    }

    let app_id = match path.parse::<Uuid>() {
        Ok(id) => id,
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
}
