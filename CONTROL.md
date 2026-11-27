# Controlling DeckHand from other programs

Only one program can drive the Stream Deck at a time. If another program
(e.g. coppersmith) wants the deck, it asks DeckHand to **pause**
(release the device, stay in the tray) or **quit**, then hands it back with
**resume** when done.

## Option 1 – command line (simplest)

```
DeckHand.exe --status   # 0 = running, 3 = paused, 1 = not running
DeckHand.exe --pause    # release the deck; returns once it is actually free
DeckHand.exe --resume   # take the deck back
DeckHand.exe --quit     # exit; returns once the process has gone
```

Exit codes: `0` ok, `1` not running, `2` timed out (5 s), `64` unknown option.
`--pause` and `--quit` block until the deck is really released, so the caller
can open the device straight after they return.

> It is a GUI-subsystem exe: from an interactive PowerShell prompt use
> `Start-Process DeckHand.exe --pause -Wait`. Programs that spawn it
> (Rust `Command::status()`, Python `subprocess.run`) wait normally.

### Rust (coppersmith)

```rust
use std::process::Command;

/// Ask DeckHand to let go of the deck. Ok(true) = it was running and
/// is now paused; Ok(false) = it wasn't running (deck already free).
fn borrow_stream_deck(exe: &str) -> std::io::Result<bool> {
    match Command::new(exe).arg("--pause").status()?.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        other => Err(std::io::Error::other(format!("pause failed: {other:?}"))),
    }
}

fn return_stream_deck(exe: &str) {
    let _ = Command::new(exe).arg("--resume").status();
}
```

## Option 2 – named events (no exe path needed)

A running instance owns these per-session named events. Open one with
`EVENT_MODIFY_STATE` and `SetEvent` it:

| Name                            | Effect |
|---------------------------------|--------|
| `Local\DeckHand.Quit`           | exit |
| `Local\DeckHand.Pause`          | release the deck |
| `Local\DeckHand.Resume`         | take the deck back |
| `Local\DeckHand.Paused`         | *manual-reset; signalled while released*: wait on it (`SYNCHRONIZE`) to know the deck is free |
| `Local\DeckHand.SingleInstance` | mutex; exists while the app is running |

If `OpenEventW` returns null, the app isn't running.

```rust
// windows-sys features: Win32_Foundation, Win32_System_Threading
use windows_sys::Win32::{Foundation::{CloseHandle, WAIT_OBJECT_0}, System::Threading::*};

fn w(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }

/// true = deck is free (paused, or the tool wasn't running)
fn pause_deckhand() -> bool {
    unsafe {
        let ev = OpenEventW(EVENT_MODIFY_STATE, 0, w(r"Local\DeckHand.Pause").as_ptr());
        if ev.is_null() { return true; } // not running
        SetEvent(ev);
        CloseHandle(ev);
        let ack = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, w(r"Local\DeckHand.Paused").as_ptr());
        if ack.is_null() { return false; }
        let ok = WaitForSingleObject(ack, 5000) == WAIT_OBJECT_0;
        CloseHandle(ack);
        ok
    }
}
```

The tray menu's **Pause (release deck)** tick follows the current state either way.
