# Screen Control

The LCD backlight can be turned on or off independently of animation streaming.

---

## Commands

The GUI exposes two buttons in the header: **ON** and **OFF**.

These send `ScreenOn` / `ScreenOff` commands to the daemon, which writes a single 440-byte interrupt packet to the control endpoint.

---

## USB protocol

Both commands use **Interface 0**, EP `0x01` OUT (440-byte interrupt).

### Screen ON

```
Byte 0:  0x12
Byte 1:  0x01
Byte 2:  0x00
Byte 3:  0x80
Byte 4:  0x64   ← brightness (100 decimal = full brightness)
Byte 5…: 0x00
```

### Screen OFF

```
Byte 0:  0x12
Byte 1:  0x01
Byte 2:  0x00
Byte 3:  0x80
Byte 4:  0x00   ← brightness = 0
Byte 5…: 0x00
```

No ACK is read for these commands — they are fire-and-forget.

---

## Notes

- Turning the screen off does not stop animation streaming. The daemon continues sending frames; they are simply not displayed.
- Turning the screen back on while streaming resumes display immediately.
- The device remembers its on/off state across USB reconnects within the same session, but not across power cycles.
