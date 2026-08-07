# GPU soak (remote)

**Result: PASS**  
**Host:** axiomexec@192.168.0.135 (RTX 4090)  
**Not used:** operator workstation GPU/DISPLAY

## Evidence
- 100/100 identical JFK transcripts (`en.wav`)
- `vram_before_mib=530` → `vram_after_mib=530` (`delta=0`)
- Required staging cuDNN 9 libs into `~/light-dictate-verify/lib` (host lacked system cuDNN 9)

## Notes
- One-shot processes (not resident daemon). No display/typing.
