// Copyright (c) 2019 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use boringtun::device::drop_privileges::drop_privileges;
use boringtun::device::{DeviceConfig, DeviceHandle};
use clap::{Arg, Command};
use daemonize::{Daemonize, Outcome};
use std::os::unix::io::RawFd; 
use std::fs::File;
use std::os::unix::net::UnixDatagram;
use std::process::exit;
use tracing::Level;

fn check_tun_name(_v: String) -> Result<(), String> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    {
        if boringtun::device::tun::parse_utun_name(&_v).is_ok() {
            Ok(())
        } else {
            Err("Tunnel name must have the format 'utun[0-9]+', use 'utun' for automatic assignment".to_owned())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(())
    }
}

fn main() {
    let matches = Command::new("boringtun")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Vlad Krasnov <vlad@cloudflare.com>")
        .args(&[
            Arg::new("INTERFACE_NAME")
                .required(true)
                .takes_value(true)
                .validator(|tunname| check_tun_name(tunname.to_string()))
                .help("The name of the created interface"),
            Arg::new("foreground")
                .long("foreground")
                .short('f')
                .help("Run and log in the foreground"),
            Arg::new("threads")
                .takes_value(true)
                .long("threads")
                .short('t')
                .env("WG_THREADS")
                .help("Number of OS threads to use")
                .default_value("4"),
            Arg::new("verbosity")
                .takes_value(true)
                .long("verbosity")
                .short('v')
                .env("WG_LOG_LEVEL")
                .possible_values(["error", "info", "debug", "trace"])
                .help("Log verbosity")
                .default_value("error"),
            Arg::new("uapi-fd")
                .long("uapi-fd")
                .env("WG_UAPI_FD")
                .help("File descriptor for the user API")
                .default_value("-1"),
            Arg::new("tun-fd")
                .long("tun-fd")
                .env("WG_TUN_FD")
                .help("File descriptor for an already-existing TUN device")
                .default_value("-1"),
            Arg::new("log")
                .takes_value(true)
                .long("log")
                .short('l')
                .env("WG_LOG_FILE")
                .help("Log file")
                .default_value("/tmp/boringtun.out"),
            Arg::new("disable-drop-privileges")
                .long("disable-drop-privileges")
                .env("WG_SUDO")
                .help("Do not drop sudo privileges"),
            Arg::new("disable-connected-udp")
                .long("disable-connected-udp")
                .help("Disable connected UDP sockets to each peer"),
            Arg::new("udp-fd")
                .long("udp-fd")
                .env("WG_SOCKET_FD")
                .help("File descriptor for an already-open UDP socket")
                .default_value("-1"),
            #[cfg(target_os = "linux")]
            Arg::new("disable-multi-queue")
                .long("disable-multi-queue")
                .help("Disable using multiple queues for the tunnel interface"),
        ])
        .get_matches();

    let background = !matches.is_present("foreground");
    // Convert UAPI FD to Option<RawFd>
    #[cfg(target_os = "linux")]
    let raw_uapi: i32 = matches
        .value_of_t("uapi-fd")
        .unwrap_or_else(|e| e.exit());
    // Convert to Option<RawFd>
    #[cfg(target_os = "linux")]
    let uapi_fd: Option<RawFd> = (raw_uapi >= 0).then(|| raw_uapi as RawFd);
    let udp_fd: i32 = matches
            .value_of_t("udp-fd")
            .unwrap_or_else(|e| e.exit());
    let socket_fd = (udp_fd >= 0).then(|| udp_fd as RawFd);


    let tun_fd: isize = matches.value_of_t("tun-fd").unwrap_or_else(|e| e.exit());
    let tun_name = matches.value_of("INTERFACE_NAME").unwrap();
    let tun_fd = (tun_fd >= 0).then(|| tun_fd as RawFd);
    let n_threads: usize = matches.value_of_t("threads").unwrap_or_else(|e| e.exit());
    let log_level: Level = matches.value_of_t("verbosity").unwrap_or_else(|e| e.exit());

    // Create a socketpair to communicate between forked processes
    let (sock1, sock2) = UnixDatagram::pair().unwrap();
    let _ = sock1.set_nonblocking(true);

    let config = DeviceConfig {
        n_threads,
        use_connected_socket: !matches.is_present("disable-connected-udp"),
        #[cfg(target_os = "linux")]
        uapi_fd,
        #[cfg(target_os = "linux")]
        use_multi_queue: !matches.is_present("disable-multi-queue"),
        tun_fd,
        udp4_fd: socket_fd,
        udp6_fd: socket_fd,
     };

    if background {
        let log = matches.value_of("log").unwrap();

        let log_file =
            File::create(log).unwrap_or_else(|_| panic!("Could not create log file {}", log));

        let (non_blocking, _guard) = tracing_appender::non_blocking(log_file);

        tracing_subscriber::fmt()
            .with_max_level(log_level)
            .with_writer(non_blocking)
            .with_ansi(false)
            .init();

        match Daemonize::new()
            .working_directory("/tmp").execute() {
                Outcome::Parent(_) => {
                    // Original parent: wait on sock2, then exit
                    let mut buf = [0u8; 1];
                    if sock2.recv(&mut buf).is_ok() && buf[0] == 1 {
                        tracing::info!("BoringTun started successfully");
                        exit(0);
                    } else {
                        tracing::error!("BoringTun failed to start");
                        exit(1);
                    }
                }
                Outcome::Child(_) => {
                    // Daemonized child: drop privileges, init device, signal parent, wait
                    if !matches.is_present("disable-drop-privileges") {
                        if let Err(e) = drop_privileges() {
                            tracing::error!("Failed to drop privileges: {:?}", e);
                            let _ = sock1.send(&[0]);
                            exit(1);
                        }
                    }

                    let mut device_handle = DeviceHandle::new(tun_name, config)
                        .unwrap_or_else(|e| {
                            tracing::error!("Failed to initialize tunnel: {:?}", e);
                            let _ = sock1.send(&[0]);
                            exit(1);
                        }
                    );

                    let _ = sock1.send(&[1]);
                    tracing::info!("BoringTun started successfully (child)");
                    device_handle.wait();
                    return;
                }
            }
    } 

    // Foreground logging
    tracing_subscriber::fmt()
        .pretty()
        .with_max_level(log_level)
        .init();
    

    let mut handle = DeviceHandle::new(tun_name, config)
        .unwrap_or_else(|e| {
            tracing::error!("Failed to initialize tunnel: {:?}", e);
            sock1.send(&[0]).unwrap();
            exit(1);
        }
    );

    if !matches.is_present("disable-drop-privileges") {
        drop_privileges().unwrap_or_else(|e| {
            tracing::error!("Failed to drop privileges: {:?}", e);
            sock1.send(&[0]).unwrap();
            exit(1);
        });
    }

    // No parent to notify in foreground, just start
    tracing::info!("BoringTun started successfully (foreground)");
    handle.wait();
    
}
