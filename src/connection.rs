//! Proxy connection handling for Wayland message forwarding.
//!
//! This module provides the core [`ProxyConnection`] type that manages
//! bidirectional message forwarding between a Wayland client and the
//! upstream compositor, with selective interception of `xdg_toplevel.set_app_id`.
//!
//! # Architecture
//!
//! ```text
//! Client <-> ProxyConnection <-> Compositor
//! ```
//!
//! # Security Model
//!
//! - **Clients are untrusted**: All message parsing is defensive
//! - **Compositor is trusted**: Server responses are forwarded as-is
//! - **App ID is enforced**: Clients cannot override injected app_id

use std::collections::VecDeque;
use std::ffi::CString;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use rustix::net::{
    recvmsg, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags,
};

// =============================================================================
// Constants and Limits
// =============================================================================

/// Maximum message size (Wayland spec allows up to 4096, we use same limit)
const MAX_MESSAGE_SIZE: usize = 4096;

/// Minimum valid message size (8-byte header)
const MIN_MESSAGE_SIZE: usize = 8;

/// Maximum file descriptors per message (Wayland limit)
const MAX_FDS_PER_MESSAGE: usize = 28;

/// Buffer capacity for socket I/O
const SOCKET_BUFFER_SIZE: usize = 4096;

// =============================================================================
// Protocol Constants
// =============================================================================

/// Well-known object IDs in Wayland protocol
mod object_ids {
    /// wl_display is always object 1
    pub const WL_DISPLAY: u32 = 1;
}

/// Opcodes for various Wayland interfaces
mod opcodes {
    /// wl_display.get_registry
    pub const WL_DISPLAY_GET_REGISTRY: u16 = 1;
    /// wl_registry.bind
    pub const WL_REGISTRY_BIND: u16 = 0;
    /// xdg_wm_base.destroy
    pub const XDG_WM_BASE_DESTROY: u16 = 0;
    /// xdg_wm_base.get_xdg_surface
    pub const XDG_WM_BASE_GET_XDG_SURFACE: u16 = 2;
    /// xdg_surface.destroy
    pub const XDG_SURFACE_DESTROY: u16 = 0;
    /// xdg_surface.get_toplevel
    pub const XDG_SURFACE_GET_TOPLEVEL: u16 = 1;
    /// xdg_toplevel.destroy
    pub const XDG_TOPLEVEL_DESTROY: u16 = 0;
    /// xdg_toplevel.set_app_id
    pub const XDG_TOPLEVEL_SET_APP_ID: u16 = 3;
}

// =============================================================================
// Object Tracker
// =============================================================================

/// Minimal object tracker for xdg-shell objects.
///
/// Tracks objects in the path to `xdg_toplevel`:
/// - `wl_registry` (from `wl_display.get_registry`)
/// - `xdg_wm_base` (from `wl_registry.bind`)
/// - `xdg_surface` (from `xdg_wm_base.get_xdg_surface`)
/// - `xdg_toplevel` (from `xdg_surface.get_toplevel`)
///
/// # Security
///
/// Object IDs are validated to be non-zero before tracking.
/// Objects are removed when destroyed to allow ID reuse.
#[derive(Debug, Default)]
pub struct ObjectTracker {
    /// Active wl_registry object IDs (from wl_display.get_registry)
    wl_registries: Vec<u32>,
    /// Active xdg_wm_base object IDs
    xdg_wm_bases: Vec<u32>,
    /// Active xdg_surface object IDs  
    xdg_surfaces: Vec<u32>,
    /// Active xdg_toplevel object IDs
    xdg_toplevels: Vec<u32>,
}

impl ObjectTracker {
    /// Create a new empty tracker.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Track a `wl_display.get_registry` call.
    ///
    /// # Arguments
    /// * `new_id` - The object ID assigned to the new wl_registry
    pub fn track_registry(&mut self, new_id: u32) {
        debug_assert!(new_id != 0, "Object ID must be non-zero");
        
        log::debug!(
            "[TRACK] wl_display.get_registry -> wl_registry@{} (total: {})",
            new_id,
            self.wl_registries.len() + 1
        );
        self.wl_registries.push(new_id);
    }

    /// Check if object ID is a known wl_registry.
    #[inline]
    pub fn is_wl_registry(&self, id: u32) -> bool {
        self.wl_registries.contains(&id)
    }

    /// Track a `wl_registry.bind` for `xdg_wm_base`.
    ///
    /// # Arguments
    /// * `interface` - The interface name being bound
    /// * `new_id` - The object ID assigned to the new binding
    pub fn track_bind(&mut self, interface: &str, new_id: u32) {
        debug_assert!(new_id != 0, "Object ID must be non-zero");
        
        if interface == "xdg_wm_base" {
            log::debug!(
                "[TRACK] wl_registry.bind: xdg_wm_base@{} (total: {})",
                new_id,
                self.xdg_wm_bases.len() + 1
            );
            self.xdg_wm_bases.push(new_id);
        } else {
            log::trace!("[TRACK] wl_registry.bind: {}@{} (ignored)", interface, new_id);
        }
    }

    /// Track a request that might create an xdg_surface or xdg_toplevel.
    ///
    /// # Returns
    /// `Some(toplevel_id)` if a new xdg_toplevel was created, `None` otherwise.
    pub fn track_request(&mut self, object_id: u32, opcode: u16, new_id: u32) -> Option<u32> {
        // Only process if this could be a relevant request
        // (new_id == 0 means this isn't creating a new object we care about)
        if new_id == 0 {
            return None;
        }

        // xdg_wm_base.get_xdg_surface (opcode 2)
        if opcode == opcodes::XDG_WM_BASE_GET_XDG_SURFACE && self.is_xdg_wm_base(object_id) {
            log::debug!(
                "[TRACK] xdg_wm_base@{}.get_xdg_surface -> xdg_surface@{}",
                object_id, new_id
            );
            self.xdg_surfaces.push(new_id);
            return None;
        }

        // xdg_surface.get_toplevel (opcode 1)
        if opcode == opcodes::XDG_SURFACE_GET_TOPLEVEL && self.is_xdg_surface(object_id) {
            log::debug!(
                "[TRACK] xdg_surface@{}.get_toplevel -> xdg_toplevel@{}",
                object_id, new_id
            );
            self.xdg_toplevels.push(new_id);
            return Some(new_id);
        }

        None
    }

    /// Check if object ID is a known xdg_wm_base.
    #[inline]
    pub fn is_xdg_wm_base(&self, id: u32) -> bool {
        self.xdg_wm_bases.contains(&id)
    }

    /// Check if object ID is a known xdg_surface.
    #[inline]
    pub fn is_xdg_surface(&self, id: u32) -> bool {
        self.xdg_surfaces.contains(&id)
    }

    /// Check if object ID is a known xdg_toplevel.
    #[inline]
    pub fn is_xdg_toplevel(&self, id: u32) -> bool {
        self.xdg_toplevels.contains(&id)
    }

    /// Check if this message is `xdg_toplevel.set_app_id`.
    #[inline]
    pub fn is_set_app_id(&self, object_id: u32, opcode: u16) -> bool {
        opcode == opcodes::XDG_TOPLEVEL_SET_APP_ID && self.is_xdg_toplevel(object_id)
    }

    /// Track object destruction.
    ///
    /// Removes the object from tracking if it was tracked.
    /// Returns `true` if the object was being tracked.
    pub fn track_destroy(&mut self, object_id: u32, opcode: u16) -> bool {
        // xdg_toplevel.destroy (opcode 0)
        if opcode == opcodes::XDG_TOPLEVEL_DESTROY && self.is_xdg_toplevel(object_id) {
            log::debug!("[TRACK] xdg_toplevel@{} destroyed", object_id);
            self.xdg_toplevels.retain(|&id| id != object_id);
            return true;
        }

        // xdg_surface.destroy (opcode 0)
        if opcode == opcodes::XDG_SURFACE_DESTROY && self.is_xdg_surface(object_id) {
            log::debug!("[TRACK] xdg_surface@{} destroyed", object_id);
            self.xdg_surfaces.retain(|&id| id != object_id);
            return true;
        }

        // xdg_wm_base.destroy (opcode 0)
        if opcode == opcodes::XDG_WM_BASE_DESTROY && self.is_xdg_wm_base(object_id) {
            log::debug!("[TRACK] xdg_wm_base@{} destroyed", object_id);
            self.xdg_wm_bases.retain(|&id| id != object_id);
            return true;
        }

        false
    }

    /// Get statistics for debugging.
    #[allow(dead_code)]
    pub fn stats(&self) -> String {
        format!(
            "registries={}, wm_bases={}, surfaces={}, toplevels={}",
            self.wl_registries.len(),
            self.xdg_wm_bases.len(),
            self.xdg_surfaces.len(),
            self.xdg_toplevels.len()
        )
    }
}

// =============================================================================
// Wire Protocol Helpers
// =============================================================================

/// Parse a Wayland message header.
///
/// # Wire Format
/// ```text
/// ┌─────────────┬─────────────┐
/// │  object_id  │ size|opcode │
/// │  (4 bytes)  │ (4 bytes)   │
/// └─────────────┴─────────────┘
/// ```
///
/// # Security
/// - Validates minimum message size
/// - Returns `None` for malformed headers
#[inline]
fn parse_message_header(data: &[u8]) -> Option<(u32, usize, u16)> {
    if data.len() < MIN_MESSAGE_SIZE {
        return None;
    }

    let object_id = u32::from_ne_bytes([data[0], data[1], data[2], data[3]]);
    let size_opcode = u32::from_ne_bytes([data[4], data[5], data[6], data[7]]);
    let msg_size = (size_opcode >> 16) as usize;
    let opcode = (size_opcode & 0xFFFF) as u16;

    // Validate message size bounds
    if msg_size < MIN_MESSAGE_SIZE || msg_size > MAX_MESSAGE_SIZE {
        log::warn!(
            "[WIRE] Invalid message size {} (object={}, opcode={})",
            msg_size, object_id, opcode
        );
        return None;
    }

    Some((object_id, msg_size, opcode))
}

/// Build a `set_app_id` message.
///
/// # Wire Format
/// ```text
/// ┌───────────┬─────────────┬──────────┬──────────┬─────────┐
/// │ object_id │ size|opcode │ str_len  │ string   │ padding │
/// │ (4 bytes) │ (4 bytes)   │ (4 bytes)│ (n bytes)│ (0-3 b) │
/// └───────────┴─────────────┴──────────┴──────────┴─────────┘
/// ```
fn build_set_app_id_message(toplevel_id: u32, app_id: &str) -> Vec<u8> {
    let app_id_cstring = CString::new(app_id).unwrap_or_else(|_| {
        log::warn!("[WIRE] app_id contains null byte, sanitizing");
        CString::new(app_id.replace('\0', "_")).unwrap()
    });
    
    let str_bytes = app_id_cstring.as_bytes_with_nul();
    let str_len = str_bytes.len() as u32;
    let padded_len = ((str_len as usize) + 3) & !3;
    let msg_size = MIN_MESSAGE_SIZE + 4 + padded_len;

    debug_assert!(msg_size <= MAX_MESSAGE_SIZE, "Message too large");

    let mut msg = Vec::with_capacity(msg_size);
    
    // Object ID
    msg.extend_from_slice(&toplevel_id.to_ne_bytes());
    
    // Size (upper 16) | Opcode (lower 16)
    let size_opcode = ((msg_size as u32) << 16) | (opcodes::XDG_TOPLEVEL_SET_APP_ID as u32);
    msg.extend_from_slice(&size_opcode.to_ne_bytes());
    
    // String length (includes null terminator)
    msg.extend_from_slice(&str_len.to_ne_bytes());
    
    // String data
    msg.extend_from_slice(str_bytes);
    
    // Padding to 4-byte boundary
    while msg.len() < msg_size {
        msg.push(0);
    }

    debug_assert_eq!(msg.len(), msg_size, "Message size mismatch");
    
    log::trace!(
        "[WIRE] Built set_app_id message: {} bytes, app_id='{}'",
        msg_size, app_id
    );

    msg
}

/// Parse a `wl_registry.bind` request to extract interface name and new_id.
///
/// # Wire Format
/// ```text
/// Header (8) + name (4) + str_len (4) + interface (padded) + version (4) + new_id (4)
/// ```
fn parse_bind_request(msg_data: &[u8]) -> Option<(String, u32)> {
    // Minimum: header(8) + name(4) + str_len(4) + str(4) + version(4) + new_id(4) = 28
    if msg_data.len() < 28 {
        log::trace!("[PARSE] bind request too short: {} bytes", msg_data.len());
        return None;
    }

    // String length at offset 12 (after header + name)
    let str_len = u32::from_ne_bytes([
        msg_data[12], msg_data[13], msg_data[14], msg_data[15]
    ]) as usize;

    // Security: Validate string length
    if str_len == 0 || str_len > 256 {
        log::warn!("[PARSE] Invalid interface string length: {}", str_len);
        return None;
    }

    let padded_len = (str_len + 3) & !3;
    let required_len = 16 + padded_len + 8; // header+name+strlen + string + version+new_id

    if msg_data.len() < required_len {
        log::trace!(
            "[PARSE] bind request truncated: have {} need {}",
            msg_data.len(), required_len
        );
        return None;
    }

    // Extract interface string (str_len includes null terminator)
    let interface_end = 16 + str_len.saturating_sub(1);
    let interface_bytes = &msg_data[16..interface_end];
    let interface = String::from_utf8_lossy(interface_bytes).to_string();

    // new_id is at: 16 + padded_string + 4 (version)
    let new_id_offset = 16 + padded_len + 4;
    let new_id = u32::from_ne_bytes([
        msg_data[new_id_offset],
        msg_data[new_id_offset + 1],
        msg_data[new_id_offset + 2],
        msg_data[new_id_offset + 3],
    ]);

    log::trace!("[PARSE] bind: interface='{}', new_id={}", interface, new_id);

    Some((interface, new_id))
}

/// Parse a string argument from a message (used for set_app_id).
fn parse_string_arg(msg_data: &[u8], offset: usize) -> Option<String> {
    if msg_data.len() < offset + 4 {
        return None;
    }

    let str_len = u32::from_ne_bytes([
        msg_data[offset],
        msg_data[offset + 1],
        msg_data[offset + 2],
        msg_data[offset + 3],
    ]) as usize;

    // Security: Validate string length
    if str_len == 0 {
        return Some(String::new());
    }
    
    if str_len > MAX_MESSAGE_SIZE || msg_data.len() < offset + 4 + str_len {
        log::warn!("[PARSE] Invalid string length {} at offset {}", str_len, offset);
        return None;
    }

    // Extract string (str_len includes null terminator)
    let string_end = offset + 4 + str_len.saturating_sub(1);
    let string_bytes = &msg_data[offset + 4..string_end];
    
    Some(String::from_utf8_lossy(string_bytes).to_string())
}

// =============================================================================
// Socket I/O with File Descriptors
// =============================================================================

/// Receive data and file descriptors from a Unix socket.
///
/// # Security
/// - Limits FDs to `MAX_FDS_PER_MESSAGE`
/// - Uses `mem::forget` to transfer FD ownership safely
fn recv_with_fds(
    socket: &UnixStream,
    buf: &mut [u8],
    fds: &mut VecDeque<RawFd>,
) -> io::Result<usize> {
    let mut cmsg_space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_MESSAGE))];
    let mut cmsg_buffer = RecvAncillaryBuffer::new(&mut cmsg_space);
    let mut iov = [IoSliceMut::new(buf)];

    let msg = recvmsg(socket.as_fd(), &mut iov[..], &mut cmsg_buffer, RecvFlags::DONTWAIT)?;

    // Collect file descriptors
    for cmsg in cmsg_buffer.drain() {
        if let RecvAncillaryMessage::ScmRights(received_fds) = cmsg {
            for fd in received_fds {
                let raw_fd = fd.as_raw_fd();
                log::trace!("[FD] Received fd={}", raw_fd);
                fds.push_back(raw_fd);
                // Transfer ownership - FD will be passed along or closed later
                std::mem::forget(fd);
            }
        }
    }

    if msg.bytes > 0 {
        log::trace!("[RECV] {} bytes, {} fds pending", msg.bytes, fds.len());
    }

    Ok(msg.bytes)
}

/// Send data and file descriptors to a Unix socket.
///
/// # Security
/// - FDs are consumed (drained) and ownership transferred
/// - Uses `mem::forget` after send to avoid double-close
fn send_with_fds(
    socket: &UnixStream,
    buf: &[u8],
    fds: &mut VecDeque<RawFd>,
) -> io::Result<usize> {
    let iov = [IoSlice::new(buf)];

    if fds.is_empty() {
        // Fast path: no FDs to send
        let mut empty_cmsg = [];
        let mut cmsg_buffer = SendAncillaryBuffer::new(&mut empty_cmsg);
        let result = sendmsg(socket.as_fd(), &iov, &mut cmsg_buffer, SendFlags::DONTWAIT)?;
        log::trace!("[SEND] {} bytes (no fds)", result);
        Ok(result)
    } else {
        // Slow path: send with FDs
        let fd_count = fds.len();
        let owned_fds: Vec<OwnedFd> = fds
            .drain(..)
            .map(|fd| {
                log::trace!("[FD] Sending fd={}", fd);
                // SAFETY: We own these FDs from recv_with_fds
                unsafe { OwnedFd::from_raw_fd(fd) }
            })
            .collect();

        let borrowed: Vec<BorrowedFd> = owned_fds.iter().map(|fd| fd.as_fd()).collect();

        let mut cmsg_space = vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(borrowed.len()))];
        let mut cmsg_buffer = SendAncillaryBuffer::new(&mut cmsg_space);
        cmsg_buffer.push(SendAncillaryMessage::ScmRights(&borrowed));

        let result = sendmsg(socket.as_fd(), &iov, &mut cmsg_buffer, SendFlags::DONTWAIT)?;

        // FDs are now owned by the receiver - drop the Vec to forget all
        drop(borrowed);
        for fd in owned_fds {
            std::mem::forget(fd);
        }

        log::trace!("[SEND] {} bytes, {} fds", result, fd_count);
        Ok(result)
    }
}

// =============================================================================
// Proxy Connection
// =============================================================================

/// A proxy connection between a Wayland client and the upstream compositor.
///
/// # Responsibilities
///
/// 1. Forward messages bidirectionally (client ↔ compositor)
/// 2. Track xdg-shell objects to identify xdg_toplevel
/// 3. Inject/modify `xdg_toplevel.set_app_id` with socket prefix
/// 4. Pass file descriptors correctly
///
/// # Security Considerations
///
/// - Client messages are parsed defensively (untrusted)
/// - Server messages are forwarded as-is (trusted)
/// - Buffer sizes are bounded to prevent memory exhaustion
/// - FD count is limited per message
pub struct ProxyConnection {
    /// Socket to the Wayland client
    client: UnixStream,
    /// Socket to the upstream compositor
    server: UnixStream,
    /// Prefix to inject/prepend to app_id
    prefix: String,
    /// Object tracker for xdg-shell objects
    tracker: ObjectTracker,
    /// Buffer for incomplete client messages
    client_buf: Vec<u8>,
    /// Pending file descriptors from client
    client_fds: VecDeque<RawFd>,
    /// Pending file descriptors from server
    server_fds: VecDeque<RawFd>,
    /// Connection ID for logging
    conn_id: u64,
}

/// Global connection counter for logging
static CONN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl ProxyConnection {
    /// Create a new proxy connection.
    ///
    /// # Arguments
    /// * `client_stream` - Socket from the client
    /// * `upstream_path` - Path to the upstream compositor socket
    /// * `prefix` - String to inject as app_id prefix
    ///
    /// # Errors
    /// Returns an error if the upstream connection fails.
    pub fn new(
        client_stream: UnixStream,
        upstream_path: &Path,
        prefix: String,
    ) -> io::Result<Self> {
        let conn_id = CONN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        
        log::debug!(
            "[CONN:{}] Connecting to upstream: {}",
            conn_id,
            upstream_path.display()
        );

        let server_stream = UnixStream::connect(upstream_path)?;
        server_stream.set_nonblocking(true)?;
        client_stream.set_nonblocking(true)?;

        log::info!(
            "[CONN:{}] Established (prefix='{}', upstream={})",
            conn_id,
            prefix,
            upstream_path.display()
        );

        Ok(Self {
            client: client_stream,
            server: server_stream,
            prefix,
            tracker: ObjectTracker::new(),
            client_buf: Vec::with_capacity(SOCKET_BUFFER_SIZE),
            client_fds: VecDeque::new(),
            server_fds: VecDeque::new(),
            conn_id,
        })
    }

    /// Get the client socket file descriptor (for event loop registration).
    #[inline]
    pub fn client_fd(&self) -> RawFd {
        self.client.as_raw_fd()
    }

    /// Get the connection ID for logging.
    #[inline]
    #[allow(dead_code)]
    pub fn id(&self) -> u64 {
        self.conn_id
    }

    /// Poll the connection for activity.
    ///
    /// # Returns
    /// - `Ok(true)` - Connection is still active
    /// - `Ok(false)` - Connection should be closed
    /// - `Err(_)` - I/O error occurred
    pub fn poll(&mut self) -> io::Result<bool> {
        // Process client → server
        match self.process_client_to_server() {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => {
                log::debug!("[CONN:{}] Client error: {}", self.conn_id, e);
                log::debug!("[CONN:{}] Client buffer size: {} bytes", self.conn_id, self.client_buf.len());
                log::debug!("[CONN:{}] Tracker state: {}", self.conn_id, self.tracker.stats());
                log::debug!("[CONN:{}] Pending client fds: {}", self.conn_id, self.client_fds.len());
                log::debug!("[CONN:{}] Pending server fds: {}", self.conn_id, self.server_fds.len());
                return Ok(false);
            }
        }

        // Process server → client
        match self.process_server_to_client() {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => {
                log::debug!("[CONN:{}] Server error: {}", self.conn_id, e);
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Process messages from client to server.
    fn process_client_to_server(&mut self) -> io::Result<()> {
        // Read available data
        let mut buf = [0u8; SOCKET_BUFFER_SIZE];
        let n = recv_with_fds(&self.client, &mut buf, &mut self.client_fds)?;

        if n == 0 {
            log::debug!("[CONN:{}] Client closed the connection", self.conn_id);
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "client closed"));
        }

        self.client_buf.extend_from_slice(&buf[..n]);
        log::trace!(
            "[CONN:{}] Client buffer: {} bytes",
            self.conn_id,
            self.client_buf.len()
        );

        // Security: Limit buffer size to prevent memory exhaustion
        if self.client_buf.len() > MAX_MESSAGE_SIZE * 4 {
            log::warn!(
                "[CONN:{}] Client buffer overflow ({} bytes), closing",
                self.conn_id,
                self.client_buf.len()
            );
            return Err(io::Error::new(io::ErrorKind::InvalidData, "buffer overflow"));
        }

        // Process complete messages
        while let Some((object_id, msg_size, opcode)) = parse_message_header(&self.client_buf) {
            if self.client_buf.len() < msg_size {
                log::trace!(
                    "[CONN:{}] Need more data: have {}, need {}",
                    self.conn_id,
                    self.client_buf.len(),
                    msg_size
                );
                break;
            }

            // Extract the complete message
            let msg_data: Vec<u8> = self.client_buf.drain(..msg_size).collect();

            log::trace!(
                "[CONN:{}] Processing: object={}, opcode={}, size={}",
                self.conn_id,
                object_id,
                opcode,
                msg_size
            );

            // Handle the message
            self.handle_client_message(object_id, opcode, msg_data)?;
        }

        Ok(())
    }

    /// Handle a single client message.
    fn handle_client_message(
        &mut self,
        object_id: u32,
        opcode: u16,
        msg_data: Vec<u8>,
    ) -> io::Result<()> {
        // Track objects that might lead to xdg_toplevel
        let new_toplevel = self.track_client_request(object_id, opcode, &msg_data);

        // Check if this is xdg_toplevel.set_app_id
        if self.tracker.is_set_app_id(object_id, opcode) {
            // Modify and forward the client's app_id (overrides initial unknown)
            self.send_modified_app_id(object_id, &msg_data)?;
            // Note: we don't prevent further set_app_id calls - client can update
            return Ok(());
        }

        // Forward the message normally
        send_with_fds(&self.server, &msg_data, &mut self.client_fds)?;

        // If a new toplevel was created, inject our initial app_id
        // Client's subsequent set_app_id will override this
        if let Some(toplevel_id) = new_toplevel {
            self.inject_app_id(toplevel_id)?;
        }

        Ok(())
    }

    /// Track client requests that might create tracked objects.
    fn track_client_request(
        &mut self,
        object_id: u32,
        opcode: u16,
        msg_data: &[u8],
    ) -> Option<u32> {
        // First, check for destroy operations
        self.tracker.track_destroy(object_id, opcode);

        // wl_display.get_registry (object 1, opcode 1)
        if object_id == object_ids::WL_DISPLAY && opcode == opcodes::WL_DISPLAY_GET_REGISTRY {
            if msg_data.len() >= 12 {
                let new_id = u32::from_ne_bytes([
                    msg_data[8], msg_data[9], msg_data[10], msg_data[11]
                ]);
                self.tracker.track_registry(new_id);
            }
            return None;
        }

        // wl_registry.bind (dynamically tracked registry, opcode 0)
        if self.tracker.is_wl_registry(object_id) && opcode == opcodes::WL_REGISTRY_BIND {
            if let Some((interface, new_id)) = parse_bind_request(msg_data) {
                self.tracker.track_bind(&interface, new_id);
            }
            return None;
        }

        // Track xdg_wm_base.get_xdg_surface and xdg_surface.get_toplevel
        // Both have new_id as first argument after header
        if msg_data.len() >= 12 {
            let new_id = u32::from_ne_bytes([
                msg_data[8], msg_data[9], msg_data[10], msg_data[11]
            ]);
            return self.tracker.track_request(object_id, opcode, new_id);
        }

        None
    }

    /// Inject a synthetic `set_app_id` for a newly created toplevel.
    ///
    /// This sets an initial `{prefix}:unknown` app_id which will be
    /// overridden if/when the client sends its own set_app_id.
    fn inject_app_id(&mut self, toplevel_id: u32) -> io::Result<()> {
        let app_id = format!("{}:unknown", &self.prefix);

        log::info!(
            "[CONN:{}] Injecting initial app_id='{}' for xdg_toplevel@{}",
            self.conn_id,
            app_id,
            toplevel_id
        );

        let msg = build_set_app_id_message(toplevel_id, &app_id);
        
        // Send without any FDs
        let mut no_fds = VecDeque::new();
        send_with_fds(&self.server, &msg, &mut no_fds)?;

        Ok(())
    }

    /// Modify and forward a client's `set_app_id` request.
    fn send_modified_app_id(&mut self, object_id: u32, msg_data: &[u8]) -> io::Result<()> {
        // Parse the original app_id
        let old_app_id = parse_string_arg(msg_data, MIN_MESSAGE_SIZE)
            .unwrap_or_else(|| String::from("<invalid>"));

        let new_app_id = format!("{}:{}", self.prefix, old_app_id);

        log::info!(
            "[CONN:{}] Modifying app_id: '{}' -> '{}' (xdg_toplevel@{})",
            self.conn_id,
            old_app_id,
            new_app_id,
            object_id
        );

        let msg = build_set_app_id_message(object_id, &new_app_id);
        send_with_fds(&self.server, &msg, &mut self.client_fds)?;

        Ok(())
    }

    /// Process messages from server to client (passthrough).
    fn process_server_to_client(&mut self) -> io::Result<()> {
        let mut buf = [0u8; SOCKET_BUFFER_SIZE];
        let n = recv_with_fds(&self.server, &mut buf, &mut self.server_fds)?;

        if n == 0 {
            log::debug!("[CONN:{}] Server closed the connection", self.conn_id);
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "server closed"));
        }

        // Forward directly to client - we trust the compositor
        send_with_fds(&self.client, &buf[..n], &mut self.server_fds)?;

        log::trace!("[CONN:{}] Server -> Client: {} bytes", self.conn_id, n);

        Ok(())
    }
}

impl Drop for ProxyConnection {
    fn drop(&mut self) {
        log::debug!(
            "[CONN:{}] Closing (tracker: {})",
            self.conn_id,
            self.tracker.stats()
        );

        // Close any remaining FDs to avoid leaks
        for fd in self.client_fds.drain(..) {
            log::trace!("[CONN:{}] Closing leaked client fd={}", self.conn_id, fd);
            unsafe { libc::close(fd) };
        }
        for fd in self.server_fds.drain(..) {
            log::trace!("[CONN:{}] Closing leaked server fd={}", self.conn_id, fd);
            unsafe { libc::close(fd) };
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_message_header() {
        // Valid header: object=1, size=12, opcode=0
        // Wire format: size_opcode = (size << 16) | opcode
        // For size=12, opcode=0: 0x000C0000
        // u32::from_ne_bytes on little-endian: [0x00, 0x00, 0x0C, 0x00] = 0x000C0000
        // size = 0x000C0000 >> 16 = 0x000C = 12
        // opcode = 0x000C0000 & 0xFFFF = 0
        
        let header = [
            0x01u8, 0x00, 0x00, 0x00, // object_id = 1
            0x00, 0x00, 0x0C, 0x00,   // size_opcode (le): size=12, opcode=0
        ];
        
        if let Some((obj, size, op)) = parse_message_header(&header) {
            assert_eq!(obj, 1);
            assert_eq!(size, 12);
            assert_eq!(op, 0);
        } else {
            panic!("Failed to parse valid header");
        }
    }

    #[test]
    fn test_parse_message_header_too_small() {
        let data = [0x01, 0x00, 0x00]; // Only 3 bytes
        assert!(parse_message_header(&data).is_none());
    }

    #[test]
    fn test_build_set_app_id_message() {
        let msg = build_set_app_id_message(42, "test");
        
        // Check object ID
        let object_id = u32::from_ne_bytes([msg[0], msg[1], msg[2], msg[3]]);
        assert_eq!(object_id, 42);
        
        // Check opcode
        let size_opcode = u32::from_ne_bytes([msg[4], msg[5], msg[6], msg[7]]);
        let opcode = (size_opcode & 0xFFFF) as u16;
        assert_eq!(opcode, opcodes::XDG_TOPLEVEL_SET_APP_ID);
        
        // Check string length (5 = "test" + null)
        let str_len = u32::from_ne_bytes([msg[8], msg[9], msg[10], msg[11]]);
        assert_eq!(str_len, 5);
    }

    #[test]
    fn test_object_tracker() {
        let mut tracker = ObjectTracker::new();
        
        // Initially empty
        assert!(!tracker.is_wl_registry(2));
        assert!(!tracker.is_xdg_wm_base(10));
        assert!(!tracker.is_xdg_toplevel(20));
        
        // Track wl_registry
        tracker.track_registry(2);
        assert!(tracker.is_wl_registry(2));
        
        // Track xdg_wm_base
        tracker.track_bind("xdg_wm_base", 10);
        assert!(tracker.is_xdg_wm_base(10));
        
        // Track xdg_surface via xdg_wm_base.get_xdg_surface
        let result = tracker.track_request(10, opcodes::XDG_WM_BASE_GET_XDG_SURFACE, 15);
        assert!(result.is_none());
        assert!(tracker.is_xdg_surface(15));
        
        // Track xdg_toplevel via xdg_surface.get_toplevel
        let result = tracker.track_request(15, opcodes::XDG_SURFACE_GET_TOPLEVEL, 20);
        assert_eq!(result, Some(20));
        assert!(tracker.is_xdg_toplevel(20));
        
        // Check set_app_id detection
        assert!(tracker.is_set_app_id(20, opcodes::XDG_TOPLEVEL_SET_APP_ID));
        assert!(!tracker.is_set_app_id(20, 0)); // Wrong opcode
        assert!(!tracker.is_set_app_id(99, opcodes::XDG_TOPLEVEL_SET_APP_ID)); // Wrong object
        
        // Test destroy tracking
        assert!(tracker.track_destroy(20, opcodes::XDG_TOPLEVEL_DESTROY));
        assert!(!tracker.is_xdg_toplevel(20));
        
        assert!(tracker.track_destroy(15, opcodes::XDG_SURFACE_DESTROY));
        assert!(!tracker.is_xdg_surface(15));
        
        assert!(tracker.track_destroy(10, opcodes::XDG_WM_BASE_DESTROY));
        assert!(!tracker.is_xdg_wm_base(10));
    }
}
