// Copyright (c) 2019 Cloudflare, Inc.
// SPDX-License-Identifier: BSD-3-Clause

use std::{fs::File, os::{fd::RawFd, unix::net::UnixDatagram}, process::exit};

use boringtun::device::{drop_privileges::drop_privileges, DeviceConfig, DeviceHandle};
use clap::{arg, Parser};
use daemonize::{Daemonize, Outcome};
use tracing::Level;
use tracing_appender::non_blocking;

/// CLI arguments
#[derive(Parser, Debug)]
#[clap(name = "boringtun", version, author)]
struct Args {
    /// The name of the created interface
    #[arg(value_parser = check_tun_name)]
    interface_name: String,

    /// Run and log in the foreground
    #[arg(short, long)]
    foreground: bool,

    /// Number of OS threads to use
    #[arg(short, long, env = "WG_THREADS", default_value = "4")]
    threads: usize,

    /// Log verbosity (error, info, debug, trace)
    #[arg(short, long, env = "WG_LOG_LEVEL", default_value = "error")]
    verbosity: Level,

    /// File descriptor for the user API (Linux)
    #[arg(long, env = "WG_UAPI_FD", default_value = "-1")]
    uapi_fd: i32,

    /// File descriptor for an existing TUN device
    #[arg(long, env = "WG_TUN_FD", default_value = "-1")]
    tun_fd: isize,

    /// File descriptor for an already-open UDP socket
    #[arg(long, env = "WG_SOCKET_FD", default_value = "-1")]
    udp_fd: i32,

    /// Log file (when daemonized)
    #[arg(short, long, env = "WG_LOG_FILE", default_value = "/tmp/boringtun.out")]
    log_file: String,

    /// Do not drop sudo privileges
    #[arg(long, env = "WG_SUDO")]
    disable_drop_privileges: bool,

    /// Disable connected UDP sockets to each peer
    #[arg(long)]
    disable_connected_udp: bool,

    /// Disable multi-queue on Linux
    #[arg(long)]
    #[cfg(target_os = "linux")]
    disable_multi_queue: bool,
}

fn check_tun_name(name: &str) -> Result<String, String> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    {
        boringtun::device::tun::parse_utun_name(name)
            .map(|_| name.to_string())
            .map_err(|_| {
                "Tunnel name must be 'utunN'; use 'utun' for auto assignment".to_string()
            })
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "tvos")))]
    {
        Ok(name.to_string())
    }
}

fn main() {
    let args = Args::parse();

    // Determine mode
    let is_daemon = !args.foreground;

    // Convert sentinel <0 → None
    #[cfg(target_os = "linux")]
    let uapi_fd = (args.uapi_fd >= 0).then(|| args.uapi_fd as RawFd);
    let tun_fd = (args.tun_fd >= 0).then(|| args.tun_fd as RawFd);
    let udp_fd = (args.udp_fd >= 0).then(|| args.udp_fd as RawFd);

    // Build config
    let config = DeviceConfig {
        n_threads: args.threads,
        use_connected_socket: !args.disable_connected_udp,
        #[cfg(target_os = "linux")]
        use_multi_queue: !args.disable_multi_queue,
        #[cfg(target_os = "linux")]
        uapi_fd,
        tun_fd,
        udp4_fd: udp_fd,
        udp6_fd: udp_fd,
    };

    // Prepare sync socketpair
    let (sock1, sock2) = UnixDatagram::pair().expect("socketpair failed");
    sock1.set_nonblocking(true).ok();

    // Logging setup
    if is_daemon {
        let file = File::create(&args.log_file)
            .unwrap_or_else(|_| panic!("Cannot create log file {}", args.log_file));
        let (non_blocking, _guard) = non_blocking(file);
        tracing_subscriber::fmt()
            .with_max_level(args.verbosity)
            .with_writer(non_blocking)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .pretty()
            .with_max_level(args.verbosity)
            .init();
    }

    // Daemonize if needed
    if is_daemon {
        match Daemonize::new().working_directory("/tmp").execute() {
            Outcome::Parent(_) => {
                // Parent: wait for child startup signal
                let mut buf = [0u8];
                match sock2.recv(&mut buf) {
                    Ok(1) => {
                        tracing::info!("BoringTun started successfully");
                        exit(0)
                    }
                    _ => {
                        tracing::error!("BoringTun failed to start");
                        exit(1)
                    }
                }
            }
            Outcome::Child(_) => {
                // Child: drop privileges, init, signal parent, serve
                if !args.disable_drop_privileges {
                    drop_privileges().unwrap_or_else(|e| {
                        tracing::error!("drop_privileges failed: {:?}", e);
                        sock1.send(&[0]).ok();
                        exit(1)
                    });
                }

                let mut handle = DeviceHandle::new(&args.interface_name, config)
                    .unwrap_or_else(|e| {
                        tracing::error!("DeviceHandle::new failed: {:?}", e);
                        sock1.send(&[0]).ok();
                        exit(1)
                    });

                sock1.send(&[1]).ok();
                tracing::info!("BoringTun daemon running");
                handle.wait();
                return;
            }
        }
    }

    // Foreground mode: no daemon
    if !args.disable_drop_privileges {
        drop_privileges().unwrap_or_else(|e| {
            tracing::error!("drop_privileges failed: {:?}", e);
            exit(1)
        });
    }
    let mut handle = DeviceHandle::new(&args.interface_name, config)
        .unwrap_or_else(|e| {
            tracing::error!("DeviceHandle::new failed: {:?}", e);
            exit(1)
        });

    tracing::info!("BoringTun running in foreground");
    handle.wait();
}