# DEPRECATED
I'm giving up this since it is manually editing raw protocol and is very fragile. Just for sure, Use `waypipe` instead. I'm daily using now.


# wayland-proxy

A transparent Wayland proxy that intercepts and modifies `xdg_toplevel.set_app_id` requests.

## Overview

This proxy sits between Wayland clients and the real compositor, forwarding all messages transparently except for `xdg_toplevel.set_app_id`. For that request, it either:

1. **Injects** a synthetic `set_app_id` immediately after toplevel creation (proactive)
2. **Modifies** the client's `set_app_id` to prefix it with the socket name
3. **Drops** subsequent `set_app_id` calls after injection (enforces our ID)

This allows you to identify which "virtual display" (socket) a window belongs to, useful for:
- Window manager rules based on socket name
- Multi-seat or sandboxed application identification
- Debugging which proxy socket an app is using

## Architecture

```
┌─────────────┐      ┌─────────────────┐      ┌─────────────┐
│   Client    │────▶│  wayland-proxy  │────▶│ Compositor  │
│    (app)    │◀────│ (my-socket)     │◀────│ (wayland-1) │
└─────────────┘      └─────────────────┘      └─────────────┘
                           │
                    Intercepts xdg_toplevel.set_app_id
                    Injects: "my-socket" or "my-socket:original_app_id"
```

## Building

```bash
# Requires Rust 1.70+
cargo build --release

# Binary at: target/release/wayland-proxy
```

## Usage

```bash
# Basic usage - create a proxy socket
wayland-proxy my-socket

# Multiple sockets
wayland-proxy socket1 socket2 socket3

# With absolute path
wayland-proxy /tmp/my-wayland-socket

# With debugging
RUST_LOG=debug wayland-proxy my-socket
```

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `WAYLAND_DISPLAY` | Upstream compositor socket | `wayland-0` |
| `XDG_RUNTIME_DIR` | Directory for socket files | Required |
| `RUST_LOG` | Log level (error/warn/info/debug/trace) | `error` |

### Running Applications Through the Proxy

```bash
# Run an application through the proxy
WAYLAND_DISPLAY=my-socket firefox

# The app's window will have app_id: "my-socket:firefox" (or just "my-socket" if no app_id sent)
```

## Protocol Details

### Tracked Objects

The proxy minimally tracks only what's needed:

| Object | Why Tracked |
|--------|-------------|
| `xdg_wm_base` | To detect `get_xdg_surface` calls |
| `xdg_surface` | To detect `get_toplevel` calls |
| `xdg_toplevel` | To intercept `set_app_id` (opcode 3) |

### Message Flow

1. Client sends `wl_registry.bind` for `xdg_wm_base` → Proxy tracks the new object ID
2. Client sends `xdg_wm_base.get_xdg_surface` → Proxy tracks the `xdg_surface` ID
3. Client sends `xdg_surface.get_toplevel` → Proxy:
   - Forwards the request
   - **Immediately injects** `xdg_toplevel.set_app_id` with the socket prefix
   - Marks toplevel as "has app_id"
4. If client later sends `set_app_id` → Proxy **drops it** (already set)

### Wire Format

Wayland messages use a simple wire format:

```
┌────────────────┬────────────────┬─────────────────────┐
│  object_id     │  size | opcode │  payload...         │
│  (4 bytes)     │  (4 bytes)     │  (variable)         │
└────────────────┴────────────────┴─────────────────────┘
     u32             u16    u16
```

String arguments include a 4-byte length prefix and are null-terminated with 4-byte alignment padding.

## Security Considerations

### Input Validation

- **Message size bounds**: Max 4KB per message, prevents memory exhaustion
- **String length validation**: Checked before parsing to prevent buffer overruns
- **Object ID tracking**: Only tracks known safe interfaces
- **FD limits**: Max 28 file descriptors per message (Wayland limit)

### Trust Model

- Clients are **untrusted**: All parsing is defensive
- Compositor is **trusted**: Server responses are forwarded without modification
- The proxy **enforces** app_id: Clients cannot override the injected ID

### Known Limitations

- Does not validate Wayland protocol semantics (relies on compositor)
- FD passing uses `mem::forget` to transfer ownership (safe but requires care)
- No authentication/authorization beyond Unix socket permissions

## Debugging

### Log Levels

```bash
# Errors only
RUST_LOG=error wayland-proxy my-socket

# Informational (connection events, app_id modifications)
RUST_LOG=info wayland-proxy my-socket

# Debug (object tracking, message inspection)
RUST_LOG=debug wayland-proxy my-socket

# Trace (wire protocol hex dumps)
RUST_LOG=trace wayland-proxy my-socket
```

### Common Issues

| Symptom | Cause | Solution |
|---------|-------|----------|
| "client closed" immediately | Protocol error | Check `RUST_LOG=debug` for details |
| App shows generic icon | `set_app_id` not matching .desktop | Expected - proxy sets custom ID |
| "upstream connection failed" | Compositor not running | Check `WAYLAND_DISPLAY` |

## License

MIT OR Apache-2.0
