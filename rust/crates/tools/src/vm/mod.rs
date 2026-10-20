//! microVM subsystem (Firecracker on Linux/KVM, QEMU fallback on macOS / no-KVM).
//!
//! A persistent microVM gives the agent a complete disposable Linux machine with
//! a real browser (Chrome + CDP), a filesystem and terminal (SSH), MCP-style
//! structured access, and Computer Use (CDP-driven). Instances live under
//! `<cwd>/.nonoclaw/vm/<name>/` and survive across sessions.
//!
//! Layout:
//! - [`backend`]  — `VmBackend` trait, per-instance state, lifecycle (start/stop)
//! - [`ssh`]      — host↔guest channel: exec / put / get via system `ssh` binary
//! - [`cdp`]      — Chrome DevTools Protocol client over TCP (guest port forward)
//! - [`tools`]    — agent-facing tools: `Vm`, `VmExec`, `VmFs`, `VmBrowser`

pub mod backend;
pub mod bash_route;
pub mod cdp;
pub mod firecracker;
pub mod qemu;
pub mod ssh;
pub mod tools;

pub use backend::{
    guest_ip_for, instances_dir, load_vm_settings, scan_instances, vm_root, VmBackend, VmInstance,
    VmSettings, VmState,
};
pub use tools::{prewarm_default, VmBrowserTool, VmExecTool, VmFsTool, VmTool};
pub use bash_route::maybe_run_in_vm;
