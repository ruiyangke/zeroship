use std::process::Command;

use zeroship_workflow_scheduler::STANDALONE_SCHEDULER_UNAVAILABLE;

#[test]
fn standalone_binary_refuses_to_run_until_dispatch_ack_loop_is_extracted() {
    let output = Command::new(env!("CARGO_BIN_EXE_zeroship-workflow-scheduler"))
        .output()
        .expect("run standalone scheduler binary");

    assert!(
        !output.status.success(),
        "standalone scheduler binary must fail until dispatch/ack handling is wired"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(STANDALONE_SCHEDULER_UNAVAILABLE),
        "stderr should explain why the binary is not runnable; got: {stderr}"
    );
}
