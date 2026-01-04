//! # wayland-proxy
//!
//! A transparent Wayland proxy that intercepts `xdg_toplevel.set_app_id` requests.
//!
//! ## Overview
//!
//! This proxy sits between Wayland clients and the real compositor, forwarding
//! all messages transparently except for `xdg_toplevel.set_app_id`. For that
//! request, it either injects a synthetic app_id or modifies the client's.
//!
//! ## Usage
//!
//! ```bash
//! # Create a proxy socket
//! wayland-proxy my-socket
//!
//! # Run an app through it
//! WAYLAND_DISPLAY=my-socket firefox
//! ```
//!
//! ## Environment Variables
//!
//! - `WAYLAND_DISPLAY`: Upstream compositor socket (default: `wayland-0`)
//! - `XDG_RUNTIME_DIR`: Directory for socket files (required)
//! - `RUST_LOG`: Log level (`error`, `warn`, `info`, `debug`, `trace`)

mod connection;

use connection::ProxyConnection;

use std::collections::HashMap;
use std::env;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};

// =============================================================================
// Constants
// =============================================================================

/// Default upstream compositor socket name
const DEFAULT_WAYLAND_DISPLAY: &str = "wayland-0";

/// Event loop dispatch timeout in milliseconds
const EVENT_LOOP_TIMEOUT_MS: u64 = 10;

// =============================================================================
// Application State
// =============================================================================

/// Global proxy state managed by the event loop.
struct ProxyState {
    /// Active proxy connections, keyed by client socket FD
    connections: HashMap<RawFd, ProxyConnection>,
    /// Path to the upstream compositor socket
    upstream_path: PathBuf,
}

impl ProxyState {
    fn new(upstream_path: PathBuf) -> Self {
        Self {
            connections: HashMap::new(),
            upstream_path,
        }
    }
}

// =============================================================================
// Socket Path Resolution
// =============================================================================

/// Resolve the upstream compositor socket path.
///
/// Checks `WAYLAND_DISPLAY` environment variable. If it's an absolute path,
/// uses it directly. Otherwise, joins it with `XDG_RUNTIME_DIR`.
///
/// # Panics
/// Panics if `XDG_RUNTIME_DIR` is not set.
fn resolve_upstream_socket() -> PathBuf {
    let display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| DEFAULT_WAYLAND_DISPLAY.into());

    if display.starts_with('/') {
        PathBuf::from(display)
    } else {
        let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set");
        PathBuf::from(runtime_dir).join(display)
    }
}

/// Resolve a socket path argument.
///
/// - Absolute paths (`/...`) are used as-is
/// - Relative paths (`./...`, `../...`) are used as-is
/// - Bare names are joined with `XDG_RUNTIME_DIR`
///
/// # Arguments
/// * `arg` - Socket path argument from command line
/// * `xdg_runtime_dir` - Value of `XDG_RUNTIME_DIR`
fn resolve_socket_path(arg: &str, xdg_runtime_dir: &str) -> PathBuf {
    if arg.starts_with('/') || arg.starts_with("./") || arg.starts_with("../") {
        PathBuf::from(arg)
    } else {
        log::debug!("Socket '{}' resolved to XDG_RUNTIME_DIR/{}", arg, arg);
        PathBuf::from(xdg_runtime_dir).join(arg)
    }
}

/// Extract the socket name (prefix) from a path.
fn extract_prefix(path: &PathBuf) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

// =============================================================================
// Main Entry Point
// =============================================================================

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Parse arguments
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <socket_path> [socket_path...]", args[0]);
        eprintln!();
        eprintln!("Environment:");
        eprintln!("  WAYLAND_DISPLAY  Upstream compositor socket (default: wayland-0)");
        eprintln!("  XDG_RUNTIME_DIR  Directory for socket files (required)");
        eprintln!("  RUST_LOG         Log level: error, warn, info, debug, trace");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  {} my-socket                  # Creates $XDG_RUNTIME_DIR/my-socket", args[0]);
        eprintln!("  {} /tmp/wayland-proxy.sock   # Creates absolute path", args[0]);
        eprintln!("  {} socket1 socket2           # Multiple sockets", args[0]);
        std::process::exit(1);
    }

    // Validate environment
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set");
    log::debug!("XDG_RUNTIME_DIR={}", xdg_runtime_dir);

    // Resolve upstream compositor
    let upstream_path = resolve_upstream_socket();
    log::info!("Upstream compositor: {}", upstream_path.display());

    // Verify upstream exists
    if !upstream_path.exists() {
        log::error!(
            "Upstream socket does not exist: {}",
            upstream_path.display()
        );
        log::error!("Check that a Wayland compositor is running.");
        std::process::exit(1);
    }

    // Initialize event loop
    let mut event_loop: EventLoop<ProxyState> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    let mut state = ProxyState::new(upstream_path);

    // Setup listener for each socket path
    for socket_arg in &args[1..] {
        let socket_path = resolve_socket_path(socket_arg, &xdg_runtime_dir);
        let prefix = extract_prefix(&socket_path);

        // Remove existing socket (if any)
        if socket_path.exists() {
            log::debug!("Removing existing socket: {}", socket_path.display());
            std::fs::remove_file(&socket_path)?;
        }

        // Create listener socket
        let listener = UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;

        log::info!(
            "Listening: {} (prefix: '{}')",
            socket_path.display(),
            prefix
        );

        // Register with event loop
        let upstream_clone = state.upstream_path.clone();
        loop_handle.insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, state| handle_new_connection(listener, state, &upstream_clone, &prefix),
        )?;
    }

    log::info!("Starting event loop (Ctrl+C to exit)");

    // Main loop
    loop {
        // Poll all active connections
        poll_connections(&mut state);

        // Process event loop (handles new connections)
        event_loop.dispatch(
            Some(std::time::Duration::from_millis(EVENT_LOOP_TIMEOUT_MS)),
            &mut state,
        )?;
    }
}

// =============================================================================
// Event Handlers
// =============================================================================

/// Handle a new client connection.
fn handle_new_connection(
    listener: &UnixListener,
    state: &mut ProxyState,
    upstream_path: &PathBuf,
    prefix: &str,
) -> Result<PostAction, std::io::Error> {
    match listener.accept() {
        Ok((stream, _addr)) => {
            if let Err(e) = stream.set_nonblocking(true) {
                log::warn!("Failed to set nonblocking: {}", e);
                return Ok(PostAction::Continue);
            }

            log::info!("New client connection (prefix: '{}')", prefix);

            match ProxyConnection::new(stream, upstream_path, prefix.to_string()) {
                Ok(conn) => {
                    let fd = conn.client_fd();
                    state.connections.insert(fd, conn);
                    log::debug!("Connection registered (fd={})", fd);
                }
                Err(e) => {
                    log::error!("Failed to connect to upstream: {}", e);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            // No pending connections
        }
        Err(e) => {
            log::warn!("Accept failed: {}", e);
        }
    }

    Ok(PostAction::Continue)
}

/// Poll all active connections and remove closed ones.
fn poll_connections(state: &mut ProxyState) {
    // Collect FDs to avoid borrowing issues
    let fds: Vec<RawFd> = state.connections.keys().copied().collect();

    for fd in fds {
        let should_remove = if let Some(conn) = state.connections.get_mut(&fd) {
            match conn.poll() {
                Ok(true) => false,  // Connection active
                Ok(false) => true,  // Connection closed gracefully
                Err(e) => {
                    log::debug!("Connection error (fd={}): {}", fd, e);
                    true
                }
            }
        } else {
            false
        };

        if should_remove {
            log::info!("Client disconnected (fd={})", fd);
            state.connections.remove(&fd);
        }
    }
}
