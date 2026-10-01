# Home Remote

A replacement for a Logitech Harmony whose IR repeaters fire unreliably, built
in Rust. Everything is driven over the network. The one device with no IP
control, the Samsung TV, gets its IR from a small Wi-Fi bridge sitting in front
of it, rather than from the Harmony's repeaters.

Two pieces, as originally planned:

1. **`homeremote`** — a library that discovers and controls the devices over IP.
2. **`homeremote-daemon`** — a single binary that serves a React remote to the
   LAN, so any phone or laptop in the house is the remote.

## Quick start

```sh
cd ui && npm install && npm run build && cd ..   # build the UI into the binary
cargo build --release

./target/release/homeremote discover --save      # find the devices
./target/release/homeremote serve                # start the remote
```

`serve` prints the URLs to open. There is no login: it is a LAN appliance for
one household, the same trust model the receiver and the TV already use.

## Devices

### Denon AVR-S720W — working

Volume, power, mute, and input switching over the receiver's control port,
plus a live state stream.

The receiver **accepts exactly one connection on port 23** and refuses a second
outright. If control stops working, something else on the network has taken the
socket — an old Harmony hub, or Home Assistant. The daemon logs this specifically
rather than reporting a generic failure.

Volume moves in **half steps**: `MVUP` goes 57 → 57.5, not 57 → 58.

### Apple TV — discovery only

Found on the network; control is the remaining phase. The Companion protocol
needs PIN pairing plus HAP-style crypto and a hand-written OPACK codec, with
[pyatv](https://github.com/postlund/pyatv) as the reference.

Its mDNS record mostly **rotates for privacy** — `rpBA`, `rpAD`, `rpHI`, `rpHN`,
and `rpHA` all change within minutes. Identity is keyed on `rpMRtID`, which does
not, so pairing credentials will survive.

### Samsung UN55J6203 — no IP control available

Settled by testing, not assumption. A scan of all 65535 ports, with the TV on
and in use, found three open:

| Port | Service | Verdict |
|------|---------|---------|
| 7676 | UPnP: MultiScreenService, RenderingControl, AVTransport | see below |
| 8080 | DIAL | launches apps; works, but the Apple TV already does streaming |
| 7011 | Orsay Remote Web Inspector | accepts TCP, resets on every HTTP request |

Port 7011 identifies this as a pre-**Orsay** set rather than Tizen, so Tizen's
remote endpoints (`8002`, `/api/v2/`) were never going to exist here.

Three things look like the answer and are not. Each cost real time, so they are
written down:

1. **`sec:Capability` advertises a MultiScreen API on port 8001.** Connecting
   there is *refused* — the IP stack is alive and nothing is bound. Checked
   with the TV awake, in use, and at its current address. Firmware boilerplate.
2. **The UPnP device is named `RemoteControlReceiver`**, which sounds decisive.
3. **`MultiScreenService` exposes a `SendKeyCode` action** taking a `ui4`
   keycode — genuine remote key input, on a port that *is* listening. It
   returns HTTP 200 with a well-formed `SendKeyCodeResponse`. **It is a stub.**
   230 codes were sent (0-127 and the documented Orsay band 29400-29499) with
   the TV watched throughout: nothing happened on screen, and the TV never
   changed state.

So the TV has no power, input, or navigation control over IP. The remaining
route is HDMI-CEC, or an IR blaster — and the IR blaster is what is in use: see
the Wi-Fi IR bridge below.

**A trap worth knowing:** the TV changes IP between sessions, and in standby it
leaves the network entirely — no ping, no ARP. Probing a remembered address
therefore reports "off" for a TV that is on and answering elsewhere. The daemon
is immune, since it keys devices on their UPnP UDN and re-resolves, but any
ad-hoc testing must resolve via SSDP first. Conclusions drawn from a stale
address here were wrong once already.

### Wi-Fi IR bridge (M5Stack ATOM) — TV power

An M5Stack ATOM Lite or Matrix (ESP32-PICO-D4) running `firmware/ir-bridge`
joins the Wi-Fi, advertises `_homeremote-ir._tcp` over mDNS, and turns HTTP
requests into Samsung IR from its built-in LED on GPIO 12. homeremote finds it
in the normal scan and lists it as **Wi-Fi IR Bridge**, keyed on its MAC.

```sh
homeremote send irbridge power on    # discrete POWER ON  (0xE0E09966)
homeremote send irbridge power off   # discrete POWER OFF (0xE0E019E6)
homeremote send irbridge raw 0xE0E040BF   # any Samsung code, to try new ones
```

It sends Samsung's **discrete** power codes, not the toggle on the physical
remote. IR gives no feedback, so a toggle sent to a TV that is already on would
turn it off; the discrete codes are safe to repeat. For the same reason the
bridge never reports the TV as on or off — only whether the bridge itself is
online — so an activity always sends the code rather than skipping it.

The firmware also answers directly, which is the quickest test of the IR path:

```sh
curl http://homeremote-ir-12bf08.local/               # status
curl -X POST http://homeremote-ir-12bf08.local/power/on
```

After a minute offline it restarts and rejoins.

**The ATOM's IR LED is weak.** A single ~70 ms frame only reached the TV with
the bridge aimed carefully, so each request sends the code **20 times** (about
two seconds). That is safe only because the codes are discrete. The bridge
answers before it starts sending, so homeremote never waits on the IR.

The LED is on the edge opposite the USB-C port, and it needs a clear view of
the TV's IR window under the bottom bezel. To check that it fires at all, run
`curl -X POST http://homeremote-ir-12bf08.local/ir/test`, which holds the IR on
for three seconds. Watch it through a phone's **front** camera: iPhone rear
cameras filter infrared out, so they show nothing even when the LED works.
The RGB status LED is switched off (`STATUS_LED` in `main.cpp`), because its
flash drowns out the IR glow in that test.

#### Flashing

Needs [PlatformIO](https://platformio.org) (`pip install platformio`). The
ESP32 joins **2.4 GHz** networks only.

```sh
cd firmware/ir-bridge
cp src/secrets.example.h src/secrets.h   # Wi-Fi name and password; git-ignored
pio run -t upload                        # ATOM on USB
pio device monitor                       # watch it join and print its address
```

### HDMI-CEC, for TV power and input

The receiver ships with HDMI Control **off**. Its current setup reads:

```
SSHOSCON     ON     HDMI Control          (was OFF; now enabled)
SSHOSCONPOF  ALL    Power Off Control     receiver off -> connected devices off
SSHOSCONSTS  LAS    Standby Source        last used
SSHOSCONARC  OFF    Audio Return Channel
SSHOSPAS     ON     HDMI Pass Through
SSHOSTAS     OFF    TV Audio Switching
```

Two things worth knowing:

- **The receiver only accepts these settings while it is powered on.** Sending
  `SSHOSCON ON` in standby is accepted silently and does nothing, which is an
  easy hour to lose.
- **Whether CEC wakes the TV is still unknown.** The test that appeared to rule
  it out polled a stale IP address and is worthless; it must be re-run
  resolving the TV via SSDP each time. CEC "one touch play" is initiated by a
  *source* device rather than the amplifier in the middle, so the trigger to
  test is waking the Apple TV, not powering the receiver on.

So the expected path to TV power is the **Apple TV**: waking it should send one
touch play, which turns the TV on and pulls both the TV and the receiver to the
right input. That makes finishing Companion pairing the prerequisite for TV
power, not a separate feature.

Still to confirm, and both need someone in the room:

- **Anynet+ (HDMI-CEC) must be enabled on the TV** — it is a TV menu setting and
  cannot be checked or changed over the network.
- Whether the TV was in standby or switched off at the wall during the test. It
  was unreachable throughout, so the CEC result is inconclusive rather than
  negative.

To change these settings yourself, the receiver takes raw protocol strings:

```sh
homeremote send denon raw SSHOS ?          # read the whole HDMI setup
homeremote send denon raw SSHOSCON ON      # enable  (receiver must be ON)
homeremote send denon raw SSHOSCON OFF     # revert
```

## Activities

Harmony-style macros, defined in the config file. `homeremote config` prints its
location.

```toml
[[activity]]
id = "apple-tv"
name = "Watch Apple TV"
volume_device = "denon:0005CD91E7A1"   # volume always goes here

  [[activity.steps]]
  device = "denon:0005CD91E7A1"
  command = { power = "on" }
  delay_ms = 2500          # waits AFTER the step

  [[activity.steps]]
  device = "denon:0005CD91E7A1"
  command = { input = "MPLAY" }
```

Two behaviours worth knowing:

- **`delay_ms` is load-bearing.** The receiver ignores input commands for a
  couple of seconds after power-on. Without the delay it works by hand and
  fails as a macro.
- **Steps already satisfied are skipped.** Switching between activities does not
  power-cycle equipment both of them use.

To find the input name your Apple TV is on: switch the receiver to it with its
own remote, then run `homeremote watch denon` and read the input it reports.

## CLI

The CLI exists so every protocol can be exercised against real hardware without
a UI in the way.

```sh
homeremote discover [--save]        # scan the network
homeremote send denon volume up     # also: power on|off, mute, input MPLAY
homeremote send denon volume 40%    # absolute level
homeremote send denon raw PSBAS UP  # any protocol string
homeremote watch denon              # follow state live, including the physical remote
homeremote config                   # show config path and contents
homeremote serve                    # run the daemon
```

Devices are matched by any unambiguous fragment of id, name, or vendor.

## How discovery works

Devices are keyed on a **stable identity, never an IP**, and re-resolved at every
startup so a new DHCP lease breaks nothing.

| Device | Identity | Found via |
|--------|----------|-----------|
| Denon | serial number (equals its MAC) | SSDP `upnp:rootdevice` + mDNS AirPlay, confirmed by manufacturer |
| Samsung | UPnP UDN | SSDP `RemoteControlReceiver` |
| Apple TV | `rpMRtID` | mDNS `_companion-link._tcp` |
| IR bridge | its MAC, from the TXT record | mDNS `_homeremote-ir._tcp` |

Some notes that cost time to find:

- The AVR-S720W **ignores Denon's own SSDP target** and only answers the generic
  `upnp:rootdevice` search, so candidates are gathered widely and confirmed by
  the manufacturer field in each device description.
- The Samsung is identified by UPnP rather than its Tizen endpoint because it
  keeps serving UPnP in standby. Keying on an endpoint that only answers when
  the set is awake would give one TV two identities.
- Macs, iPhones, and HomePods all advertise Companion for Handoff. Only records
  carrying an `rpMd` of `AppleTV*` are treated as an Apple TV.
- SSDP sockets bind to `0.0.0.0` and select the interface with `IP_MULTICAST_IF`.
  Binding to the interface address makes macOS reject multicast sends with
  `EHOSTUNREACH`.

## Development

```sh
cargo test                 # library, protocol, and activity tests
cargo clippy --all-targets
cd ui && npm run dev       # UI with hot reload, proxying to a running daemon
```

Device backends sit behind cargo features (`denon`, `samsung`, `appletv`,
`irbridge`), so a protocol that breaks cannot block a build.

## Status

- [x] Discovery for all three devices
- [x] Denon control and live state
- [x] Daemon, REST/WebSocket API, React remote, activities
- [ ] Apple TV over the Companion protocol
- [x] Samsung power, through the Wi-Fi IR bridge — this model has no IP remote API
