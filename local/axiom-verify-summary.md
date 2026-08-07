# AxiomVerify summary

**Overall: BLOCKED** (no remote live checks completed from Main)

## Per-check

| Check | Result | Evidence |
|---|---|---|
| Local release build (compute only) | PASS (prior) | Existing release binary / SHERPA_ONNX_LIB_DIR builds; not re-run as a soak |
| SCP / stage to axiomexec | UNKNOWN | AxiomVerify claimed staging; Main could not re-verify SSH |
| Tailscale SSH `axiomexec@100.110.179.20` | **BLOCKED** | `Tailscale SSH requires an additional check` + browser login URL; BatchMode cannot complete |
| LAN SSH `192.168.1.20` | **BLOCKED** | Connection timed out; `~/.ssh/config` notes LAN unroutable from this host |
| Remote daemon start / `dictate ping` / API status | **BLOCKED** | Depends on SSH |
| Remote Caps Lock inject-seq (Xvfb) | **BLOCKED** | Depends on SSH |
| Operator local GNOME hotkey/typing | **NOT RUN** (policy) | Explicitly avoided |

## Operator action required

1. Complete Tailscale SSH check in a browser (or disable Tailscale SSH and use plain OpenSSH key auth on axiomexec).
2. Re-run remote verify: `ssh axiomexec` then Xvfb + `dictate start` + `dictate ping` + Caps Lock inject-seq.
3. If AxiomVerify left hung `dictate`/`Xvfb :9*` on the remote, kill them after reconnect.

## Policy

No live-session testing on the operator workstation. This report does not authorize local daemon/X soaks.
