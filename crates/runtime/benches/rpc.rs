use appbase_runtime::cpu_timer::CpuLimits;
use appbase_runtime::v8::{create_v8_runtime, create_v8_runtime_with_snapshot, handle_rpc};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

static CPU_LIMITS: CpuLimits = CpuLimits { max_cpu_per_request: None, max_cpu_total: None };

fn setup_runtime() -> (deno_core::JsRuntime, std::rc::Rc<appbase_runtime::v8::RpcResult>) {
    let db_path = format!("/tmp/appbase-bench-{}.db", std::process::id());
    let (mut runtime, rpc_result) = create_v8_runtime(&db_path).unwrap();

    // Load test server functions
    runtime
        .execute_script(
            "<test>",
            r#"
const todos = db.collection('todos');
async function addTodo(text) { return todos.insert({ text, done: false }); }
async function getTodos() { return todos.find(); }
globalThis.__rpc = { addTodo, getTodos };
"#,
        )
        .unwrap();

    // Tokio runtime to drive the event loop
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(runtime.run_event_loop(Default::default()))
        .unwrap();

    (runtime, rpc_result)
}

fn bench_runtime_startup(c: &mut Criterion) {
    c.bench_function("runtime_startup", |b| {
        b.iter(|| {
            let db_path = format!("/tmp/appbase-bench-startup-{}.db", rand_id());
            let (runtime, _) = create_v8_runtime(&db_path).unwrap();
            black_box(runtime);
            let _ = std::fs::remove_file(&db_path);
        });
    });
}

fn bench_rpc_insert(c: &mut Criterion) {
    let (mut runtime, rpc_result) = setup_runtime();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("rpc_insert", |b| {
        b.iter(|| {
            let result = rt.block_on(handle_rpc(
                &mut runtime,
                &rpc_result,
                &CPU_LIMITS,
                black_box(r#"{"jsonrpc":"2.0","method":"addTodo","params":["bench"],"id":1}"#),
            ));
            black_box(result.unwrap());
        });
    });
}

fn bench_rpc_find(c: &mut Criterion) {
    let (mut runtime, rpc_result) = setup_runtime();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Insert some data first
    for i in 0..100 {
        rt.block_on(handle_rpc(
            &mut runtime,
            &rpc_result,
            &format!(r#"{{"jsonrpc":"2.0","method":"addTodo","params":["item {i}"],"id":{i}}}"#),
        ))
        .unwrap();
    }

    c.bench_function("rpc_find_100_items", |b| {
        b.iter(|| {
            let result = rt.block_on(handle_rpc(
                &mut runtime,
                &rpc_result,
                &CPU_LIMITS,
                black_box(r#"{"jsonrpc":"2.0","method":"getTodos","params":[],"id":1}"#),
            ));
            black_box(result.unwrap());
        });
    });
}

fn bench_rpc_batch(c: &mut Criterion) {
    let (mut runtime, rpc_result) = setup_runtime();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let batch = r#"[
        {"jsonrpc":"2.0","method":"addTodo","params":["batch1"],"id":1},
        {"jsonrpc":"2.0","method":"addTodo","params":["batch2"],"id":2},
        {"jsonrpc":"2.0","method":"addTodo","params":["batch3"],"id":3},
        {"jsonrpc":"2.0","method":"getTodos","params":[],"id":4}
    ]"#;

    c.bench_function("rpc_batch_4_calls", |b| {
        b.iter(|| {
            let result = rt.block_on(handle_rpc(
                &mut runtime,
                &rpc_result,
                &CPU_LIMITS,
                black_box(batch),
            ));
            black_box(result.unwrap());
        });
    });
}

fn bench_rpc_noop(c: &mut Criterion) {
    let (mut runtime, rpc_result) = setup_runtime();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Add a no-op function to measure pure dispatch overhead
    runtime
        .execute_script("<noop>", r#"globalThis.__rpc.noop = () => "ok";"#)
        .unwrap();

    c.bench_function("rpc_dispatch_noop", |b| {
        b.iter(|| {
            let result = rt.block_on(handle_rpc(
                &mut runtime,
                &rpc_result,
                &CPU_LIMITS,
                black_box(r#"{"jsonrpc":"2.0","method":"noop","params":[],"id":1}"#),
            ));
            black_box(result.unwrap());
        });
    });
}

fn rand_id() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn bench_runtime_startup_with_snapshot(c: &mut Criterion) {
    // Load snapshot if available
    let snapshot_path = "/tmp/appbase-snapshot.bin";
    let snapshot_data = match std::fs::read(snapshot_path) {
        Ok(data) => data,
        Err(_) => {
            eprintln!("Snapshot not found at {snapshot_path}. Run `cargo run --features snapshot --bin appbase-snapshot -- {snapshot_path}` first.");
            return;
        }
    };
    // Leak the data to get a 'static reference (fine for benchmarks)
    let snapshot: &'static [u8] = Box::leak(snapshot_data.into_boxed_slice());

    c.bench_function("runtime_startup_with_snapshot", |b| {
        b.iter(|| {
            let db_path = format!("/tmp/appbase-bench-snap-{}.db", rand_id());
            let (runtime, _) = create_v8_runtime_with_snapshot(&db_path, snapshot).unwrap();
            black_box(runtime);
            let _ = std::fs::remove_file(&db_path);
        });
    });
}

criterion_group!(
    benches,
    bench_runtime_startup,
    bench_runtime_startup_with_snapshot,
    bench_rpc_noop,
    bench_rpc_insert,
    bench_rpc_find,
    bench_rpc_batch,
);
criterion_main!(benches);
