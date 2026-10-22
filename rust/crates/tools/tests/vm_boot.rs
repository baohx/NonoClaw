//! Manual integration test: boot a real microVM via the QEMU backend on this
//! macOS host and run a command inside it over SSH.
//!
//! ```sh
//! cargo test -p nonoclaw-tools --test vm_boot -- --ignored --nocapture
//! ```
//!
//! Requires ~/.nonoclaw/vm/ assets (kernel.img, initrd.img, rootfs-bare.ext4,
//! ssh_key{,.pub}) and qemu-system-x86_64 on PATH. Boots a disposable
//! instance under a temp cwd, then destroys it.

use nonoclaw_core::permissions::PermissionMode;
use nonoclaw_tools::tool::{Tool, ToolCtx, ToolOptions};
use nonoclaw_tools::vm::{VmExecTool, VmTool};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn ctx<'a>(
    cwd: &'a std::path::Path,
    cancel: &'a CancellationToken,
    options: &'a ToolOptions,
) -> ToolCtx<'a> {
    ToolCtx {
        cwd,
        options,
        cancel,
        tool_use_id: "vm-boot-test",
        task_scope: None,
        subagent: None,
        graph_runner: None,
        question: None,
        background_registry: None,
    }
}

/// VMExec success shape: `"[exit 0]\n<out>"`; failures come back as
/// ToolResult::error text (not `Err`).
fn ok_data(r: nonoclaw_tools::tool::ToolResult) -> String {
    assert!(
        !r.data.starts_with("no VM instance")
            && !r.data.starts_with("VM ")
            && !r.data.contains("Connection refused"),
        "vm exec failed: {}",
        r.data
    );
    r.data
}

#[tokio::test]
#[ignore = "manual: needs VM assets + qemu on PATH; boots a real VM"]
async fn boots_qemu_vm_and_execs_over_ssh() {
    let cwd = std::env::temp_dir().join("nc-vm-boot-test");
    std::fs::create_dir_all(&cwd).unwrap();
    let cancel = CancellationToken::new();
    let options = ToolOptions { model: "test".into(), permission_mode: PermissionMode::default(), is_non_interactive: true, max_budget_usd: None };

    // ensure: create + start (QEMU backend, hvf)
    let ensure = VmTool
        .call(
            json!({"type": "ensure", "name": "e2e"}),
            &ctx(&cwd, &cancel, &options),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    println!("== ensure ==\n{}", ensure.data);
    assert!(
        !ensure.data.starts_with("error") && !ensure.data.contains("missing"),
        "ensure failed: {}",
        ensure.data
    );

    // boot takes a while on first start; poll uname until SSH is up
    let mut booted = None;
    for i in 0..40 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let r = VmExecTool
            .call(
                json!({"vm": "e2e", "command": "uname -sm && cat /etc/debian_version"}),
                &ctx(&cwd, &cancel, &options),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        println!("== try {i}: ==\n{}", r.data);
        // Only a clean exit means sshd is really up — slirp accepts the TCP
        // connect before the guest sshd is ready, so transient `[exit 255]`
        // (banner timeout) during boot must keep polling, not break.
        if r.data.starts_with("[exit 0]") {
            booted = Some(r.data);
            break;
        }
    }
    let out = booted.expect("VM never became SSH-reachable");
    assert!(out.contains("Linux x86_64"), "unexpected uname output: {out}");
    assert!(out.contains("12."), "not Debian 12: {out}");

    // persistence across execs (same instance disk)
    let r = ok_data(
        VmExecTool
            .call(
                json!({"vm": "e2e", "command": "echo marker-$((6*7)) > /persist.txt && cat /persist.txt"}),
                &ctx(&cwd, &cancel, &options),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    );
    assert!(r.contains("marker-42"), "persist read failed: {r}");

    // teardown
    let r = VmTool
        .call(
            json!({"type": "destroy", "name": "e2e"}),
            &ctx(&cwd, &cancel, &options),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    println!("== destroy ==\n{}", r.data);
    let _ = std::fs::remove_dir_all(&cwd);
}
