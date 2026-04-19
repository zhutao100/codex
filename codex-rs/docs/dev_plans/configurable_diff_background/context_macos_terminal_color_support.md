## macOS Terminal.app color support (14 → 15 → 26 → “future”)

### What changed materially

Apple explicitly introduced **24-bit (“truecolor”) rendering** for Terminal in **macOS Tahoe (macOS 26)**:

* WWDC25 *Platforms State of the Union* transcript: “Terminal has a fresh look with **24-bit color** …” ([Apple Developer][1])
* Apple’s “All New Features” PDF for macOS Tahoe: “With **24-bit color support**, your commands now have over **16 million** color choices.” ([Apple][2])

Before macOS 26, Terminal is repeatedly described (including on Apple’s own support forum) as **256-color (8-bit)** rather than truecolor, and a 24-bit escape-sequence test is reported to *not* render as intended. ([Apple Support Community][3])

---

## Version-by-version summary

| macOS version          |                                                    Terminal.app max *rendered* color level (per evidence) | Primary evidence                                                                                                                                                              | Notes for TUIs (capability negotiation)                                                                                                                                                                                                                                                    |
| ---------------------- | --------------------------------------------------------------------------------------------------------: | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **macOS 14 (Sonoma)**  |                                         **256 colors (8-bit)**; truecolor not supported (pre-26 evidence) | Apple Support Community response: “supports 8-bit color; 256 colors.” ([Apple Support Community][3]) + ongoing pre-26 truecolor requests/tests ([Apple Support Community][4]) | Terminal’s profile “Declare terminal as” sets `TERM` (terminfo) rather than “changing hardware capabilities.” ([Apple Support][5])                                                                                                                                                         |
| **macOS 15 (Sequoia)** | **256 colors (8-bit)** (no Apple announcement of 24-bit prior to 26; pre-26 forum evidence still applies) | Apple’s 24-bit announcement is explicitly tied to macOS Tahoe (26), implying it was not previously an in-box feature. ([Apple Developer][1])                                  | Default terminal-type posture has long been `xterm-256color` (changed in OS X Lion; still the common baseline). ([Apple Support Community][6])                                                                                                                                             |
| **macOS 26 (Tahoe)**   |                                                                                    **Truecolor (24-bit)** | WWDC25 transcript + Apple Tahoe feature PDF. ([Apple Developer][1])                                                                                                           | **Important gotcha:** system `ncurses` is still reported as **6.0** on Tahoe 26.1, and `tput colors`/terminfo numeric `colors` can’t represent 16M without ncurses ≥ 6.1. So terminfo-driven probes may still “look like 256” even if the emulator renders truecolor. ([Ask Different][7]) |
| **Future (post-26)**   |                                                          **Unknown** (no authoritative forward guarantee) | Apple only commits in public docs to macOS 26’s change. ([Apple Developer][1])                                                                                                | Best practice is runtime detection + override knobs; don’t assume terminfo will stay in sync with emulator capabilities. ([Ask Different][7])                                                                                                                                              |

---

## Evidence about “environment awareness” (TERM / terminfo vs real rendering)

### 1) “Declare terminal as …” is *TERM/terminfo*, not full emulation switching

Apple’s Terminal User Guide is explicit: the Advanced setting “Declare terminal as” **sets the `TERM` environment variable** used by `terminfo`. ([Apple Support][5])
So a TUI that keys only on `TERM` is consuming what Terminal *declares*, not necessarily all it *can* do.

### 2) Default terminal type has been `xterm-256color` since Lion

An Apple Support Community thread documents: “The default terminal type was changed in Lion to **xterm-256color**.” ([Apple Support Community][6])
This underpins why modern macOS Terminal environments often look like `xterm-256color` even today.

### 3) macOS 26 adds truecolor, but terminfo may lag

Ask Different reports **Sonoma 14.8.2** and **Tahoe 26.1** both using **ncurses 6.0.20150808**, and notes that representing 16M colors in terminfo (`colors#0x1000000`, `tput colors`) requires ncurses ≥ 6.1 due to terminfo format constraints. ([Ask Different][7])
Separately, early Tahoe beta testers observed truecolor rendering but were unsure whether it “advertises the env vars.” ([GitHub][8])

A concrete example from macOS Tahoe 26.4 shows a Terminal-launched process environment including `COLORTERM=truecolor` *and* `TERM=xterm-256color`—suggesting apps should not equate `TERM=xterm-256color` with “no truecolor” on 26+. ([GitHub][9])

---

## Minimal takeaway for TUI authors

* **macOS 14/15 Terminal.app:** treat as **256-color** (8-bit) unless the user is on a different terminal emulator. ([Apple Support Community][3])
* **macOS 26 Terminal.app:** treat as **truecolor-capable**, but **don’t rely solely on terminfo numeric `colors`** to discover it (system ncurses/terminfo format can lag). ([Apple Developer][1])

[1]: https://developer.apple.com/videos/play/wwdc2025/102/ "Platforms State of the Union - WWDC25 - Videos - Apple Developer"
[2]: https://www.apple.com/os/pdf/All_New_Features_macOS_Tahoe_Sept_2025.pdf "All New Features_macOS Tahoe_Sept_2025"
[3]: https://discussions.apple.com/thread/254789350 "Native Terminal True Color Support - Apple Community"
[4]: https://discussions.apple.com/thread/250459018 "Terminal.app true/24-bit color: how to lo… - Apple Community"
[5]: https://support.apple.com/en-lb/guide/terminal/trmladvn/mac "Change Profiles Advanced settings in Terminal on Mac - Apple Support (LB)"
[6]: https://discussions.apple.com/thread/3252834 "Terminal input wrong behaviour - Apple Community"
[7]: https://apple.stackexchange.com/questions/484144/what-version-of-ncurses-is-included-with-sequoia-and-tahoe "macos - What version of ncurses is included with Sequoia and Tahoe? - Ask Different"
[8]: https://github.com/termstandard/colors/issues/69 "Terminal.app for macOS Tahoe supports truecolor · Issue #69 · termstandard/colors · GitHub"
[9]: https://github.com/pbek/QOwnNotes/issues/3553 "Two 'leave' buttons in Distraction Free Writing Mode are shown · Issue #3553 · pbek/QOwnNotes · GitHub"
