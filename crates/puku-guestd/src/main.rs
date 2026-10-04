//! puku-guestd: init and host agent for Cloud Hypervisor guests.
//!
//! What microsandbox's agentd does for libkrun guests, this does for Cloud
//! Hypervisor ones: it is the guest's PID 1, lays the root filesystem out,
//! mounts the host's shared directories, brings the network up, and then
//! serves the host over vsock -- run a command, open a guest port, power off.
//! Protocol: `puku_cloud_proto::guest_proto`.
//!
//! It is copied into every guest image at build time
//! (deploy/scripts/build-ch-rootfs.sh), so guest images themselves stay
//! ordinary OCI images with nothing of ours in them.

mod cmdline;
mod frames;
mod package_watcher;

#[cfg(target_os = "linux")]
mod linux;

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("puku-guestd is the init of Linux guests; there is nothing for it to do on this OS");
        std::process::exit(1);
    }
}
