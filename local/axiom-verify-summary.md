# AxiomVerify summary

**Result: PASS (remote Xvfb)**  
**Host:** `axiomexec@192.168.0.135` (hostname `axiomserver`) via LAN BatchMode SSH  
**Not used:** Tailscale `100.110.179.20` (interactive login URL / timeout); operator local GNOME/DISPLAY

## Checks

| Check | Result |
|---|---|
| `dictate start` on Xvfb `:97` | PASS — PID running, Caps Lock PTT armed |
| `dictate status` | PASS |
| `dictate ping` | PASS — `pong 27.9 ms` |
| `dictate api status` | PASS — JSON with `api:true`, stage idle, model path, type armed |
| Caps Lock inject-seq (`hotkey_demo` keycode 66) | PASS — sequences completed; after `stop`, keycode 66 = `Caps_Lock NoSymbol Caps_Lock` |
| Cancel inject (`53:tap` while held) | exercised (sequence done) |
| Local GNOME/typing | **not touched** |

## Notes

- Isolated tree: `~/light-dictate-verify/{bin,lib,models,config-xdg}` with `provider=cpu`, Parakeet int8 model, sherpa shared libs under `lib/`.
- Daemon still logged to `~/.cache/dictate/dictate.log` (did not honor `XDG_CACHE_HOME` for the worker log path in this run) — follow-up if we want fully sandboxed logs.
- `type_output=true` required to start; typing only hit Xvfb `:97`, never the operator session.

## HEAD exercised

Release binaries built from workspace at verify time (post-`ad96f0f` rebuild).
